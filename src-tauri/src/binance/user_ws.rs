use crate::binance::{Creds, RestClient, WS_BASE, connect_ws};
use crate::engine;
use crate::state::AppState;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;

const KEEPALIVE_SECS: u64 = 25 * 60;

/// 私有数据流: 负责 listenKey 生命周期与持仓推送, 断开后指数退避重连.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let rest = RestClient::new();
        let mut creds_rx = state.subscribe_creds();
        let mut backoff = 2u64;

        loop {
            let creds = creds_rx.borrow().clone();
            let Some(creds) = creds else {
                // 没有凭证就等着, 不空转.
                if creds_rx.changed().await.is_err() {
                    return;
                }
                continue;
            };

            if rest.sync_time().await.is_err() {
                // 时间校准失败不阻断, 下一次请求会再试.
            }

            match run_once(&app, &rest, &creds, &mut creds_rx).await {
                Ok(()) => {
                    backoff = 2;
                }
                Err(e) => {
                    engine::set_error(&app, Some(e));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    });
}

async fn run_once(
    app: &AppHandle,
    rest: &RestClient,
    creds: &Creds,
    creds_rx: &mut tokio::sync::watch::Receiver<Option<Creds>>,
) -> Result<(), String> {
    let listen_key = rest.listen_key_create(creds).await?;
    let url = format!("{}/ws/{}", WS_BASE, listen_key);
    let ws = connect_ws(&url)
        .await
        .map_err(|e| format!("私有流连接失败: {e}"))?;

    eprintln!("[user] 已连接私有流");
    engine::set_stream_state(app, true, None);
    // 建连成功后立刻拉一次全量持仓, 补上 WS 建立前的变更.
    let _ = engine::refresh_positions(app, rest, creds).await;
    engine::emit(app);

    let (mut sink, mut stream) = ws.split();
    let mut keepalive = interval(Duration::from_secs(KEEPALIVE_SECS));
    keepalive.tick().await;

    let result = loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if let Err(e) = rest.listen_key_keepalive(creds).await {
                    // 续期失败按断线处理, 让外层重建 listenKey.
                    break Err(format!("listenKey 续期失败: {e}"));
                }
            }
            changed = creds_rx.changed() => {
                break match changed {
                    Ok(()) => Ok(()),
                    Err(_) => Err("凭证通道关闭".to_string()),
                };
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(txt))) => {
                        {
                            static FIRST_USER: std::sync::Once = std::sync::Once::new();
                            FIRST_USER.call_once(|| {
                                eprintln!("[user] 首帧: {}", &txt[..txt.len().min(80)])
                            });
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(txt.as_str()) {
                            engine::handle_user_event(app, &v);
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(format!("私有流错误: {e}")),
                    None => break Err("私有流被关闭".to_string()),
                }
            }
        }
    };

    engine::set_stream_state(app, false, None);
    result
}
