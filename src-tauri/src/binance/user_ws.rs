use crate::binance::{RestClient, WS_PRIVATE_BASE, connect_ws};
use crate::engine;
use crate::state::{AccountSession, AppState, until_changed};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::time::interval;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const KEEPALIVE_SECS: u64 = 25 * 60;

/// 私有数据流: 负责 listenKey 生命周期与持仓推送, 断开后指数退避重连.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let rest = app.state::<Arc<RestClient>>().inner().clone();
        let mut creds_rx = state.subscribe_creds();
        let mut backoff = 2u64;

        loop {
            let session = creds_rx.borrow_and_update().clone();
            if session.creds.is_none() {
                // 没有凭证就等着, 不空转.
                if creds_rx.changed().await.is_err() {
                    return;
                }
                backoff = 2;
                continue;
            }
            // 覆盖 listenKey 创建、完整握手、首次 REST 刷新、续期和 Pong 等全部等待.
            let result = until_changed(&mut creds_rx, run_once(&app, &rest, &session)).await;
            match result {
                None | Some(Ok(())) => {
                    backoff = 2;
                    if creds_rx.has_changed().is_err() {
                        return;
                    }
                }
                Some(Err(e)) => {
                    engine::set_stream_state(&app, session.generation, false, Some(e));
                    if until_changed(&mut creds_rx, tokio::time::sleep(Duration::from_secs(backoff))).await.is_none() {
                        backoff = 2;
                    } else {
                        backoff = (backoff * 2).min(60);
                    }
                }
            }
        }
    });
}

async fn run_once(
    app: &AppHandle,
    rest: &RestClient,
    session: &AccountSession,
) -> Result<(), String> {
    let creds = session.creds.as_ref().ok_or("会话没有凭据")?;
    let _ = rest.sync_time().await;
    let listen_key = rest.listen_key_create(creds).await?;
    let url = build_url(&listen_key);
    let ws = connect_ws(&url)
        .await
        .map_err(|e| format!("私有流连接失败: {e}"))?;

    engine::set_stream_state(app, session.generation, true, None);
    // 建连成功后立刻拉一次全量持仓, 补上 WS 建立前的变更.
    if let Err(e) = engine::refresh_positions(app, rest, session).await {
        engine::set_error(app, session.generation, Some(e));
    }
    engine::emit_if_current(app, session.generation);
    read_stream(ws, rest, session, |v| engine::handle_user_event(app, session.generation, v)).await
}

fn build_url(listen_key: &str) -> String { format!("{WS_PRIVATE_BASE}/stream?streams={listen_key}") }

// Tauri 只执行事件 IO, 同一收帧循环可用 loopback WebSocket 验证过期重建.
async fn read_stream<S, F>(
    ws: WebSocketStream<S>,
    rest: &RestClient,
    session: &AccountSession,
    mut handle_event: F,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnMut(&serde_json::Value) -> bool,
{
    let creds = session.creds.as_ref().ok_or("会话没有凭据")?;
    let (mut sink, mut stream) = ws.split();
    let mut keepalive = interval(Duration::from_secs(KEEPALIVE_SECS));
    keepalive.tick().await;

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if let Err(e) = rest.listen_key_keepalive(creds).await {
                    // 续期失败按断线处理, 让外层重建 listenKey.
                    return Err(format!("listenKey 续期失败: {e}"));
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(txt))) => {
                        // 禁止记录原始私有帧, 过期事件内也可能包含 listenKey.
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(txt.as_str())
                            && handle_event(v.get("data").unwrap_or(&v))
                        {
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await.map_err(|_| "私有流心跳发送失败".to_string())?;
                    }
                    Some(Ok(Message::Close(_))) | None => return Err("私有流被关闭".to_string()),
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return Err("私有流传输失败".to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::Creds;
    use tokio::net::TcpListener;

    #[test]
    fn user_url_uses_private_combined_partition() {
        assert_eq!(build_url("FAKE_LISTEN_KEY"), "wss://fstream.binance.com/private/stream?streams=FAKE_LISTEN_KEY");
    }
    #[tokio::test]
    async fn expired_message_exits_product_loop_without_server_closing_connection() {
        for message in [
            r#"{"e":"listenKeyExpired","listenKey":"FAKE_LISTEN_KEY"}"#,
            r#"{"stream":"FAKE_LISTEN_KEY","data":{"e":"listenKeyExpired","listenKey":"FAKE_LISTEN_KEY"}}"#,
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                ws.send(Message::Text(message.into())).await.unwrap();
                std::future::pending::<()>().await;
                drop(ws);
            });
            let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            let state = AppState::new();
            state.set_creds(Some(Creds { key: "fake".into(), secret: "fake".into() }));
            let session = state.session();
            state.write_if_current(session.generation, |s| s.health.user_stream_ok = true);
            let rest = RestClient::new();
            let result = tokio::time::timeout(Duration::from_secs(1), read_stream(ws, &rest, &session, |v| {
                engine::apply_user_event(&state, session.generation, v)
            })).await.unwrap();
            assert!(result.is_ok());
            assert!(!state.snapshot().health.user_stream_ok);
            assert!(!server.is_finished());
            server.abort();
        }
    }
}
