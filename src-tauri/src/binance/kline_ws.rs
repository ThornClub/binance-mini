use crate::binance::{WS_MARKET_BASE, connect_ws, stream_symbol};
use crate::chart::{self, ChartToken};
use crate::engine;
use crate::state::{AppState, now_ms, until_changed};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::time::{Instant, interval};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let mut target_rx = state.subscribe_chart();
        let mut backoff = 2;
        loop {
            let target = target_rx.borrow_and_update().clone();
            let Some(token) = target else {
                if target_rx.changed().await.is_err() { return; }
                continue;
            };
            let Some(epoch) = chart::begin_connection(&state, &token) else { continue };
            engine::emit_if_current(&app, token.account_generation);
            // 包围握手、完整读循环、Pong和退避; 不借用market订阅watch.
            match until_changed(&mut target_rx, run_once(&app, &token, epoch, &mut backoff)).await {
                None | Some(Ok(())) => {
                    backoff = 2;
                    if target_rx.has_changed().is_err() { return; }
                }
                Some(Err((failed_epoch, error))) => {
                    chart::connection_result(&state, &token, failed_epoch, Err(error));
                    engine::emit_if_current(&app, token.account_generation);
                    if until_changed(&mut target_rx, tokio::time::sleep(Duration::from_secs(backoff))).await.is_none() { backoff = 2; }
                    else { backoff = (backoff * 2).min(60); }
                }
            }
        }
    });
}
fn build_url(token: &ChartToken) -> String {
    format!("{}/ws/{}@kline_{}", WS_MARKET_BASE, stream_symbol(&token.symbol), token.interval)
}
async fn run_once(app: &AppHandle, token: &ChartToken, epoch: u64, backoff: &mut u64) -> Result<(), (u64, String)> {
    let ws = connect_ws(&build_url(token)).await.map_err(|e| (epoch, format!("K线连接失败: {e}")))?;
    let state = app.state::<AppState>();
    let epoch = chart::connection_result(&state, token, epoch, Ok(()))
        .ok_or_else(|| (epoch, "图表会话已变更".to_string()))?;
    engine::emit_if_current(app, token.account_generation);
    read_stream(ws, backoff, |v| {
        let accepted = chart::apply_live(&state, token, epoch, v, now_ms());
        // 单一读任务在处理下一帧前发快照, 不把多根压成一次异步通知.
        if accepted { engine::emit_if_current(app, token.account_generation); }
        chart::parse_live(v, token).is_some()
    }).await.map_err(|error| (epoch, error))
}
async fn read_stream<S, F>(mut ws: WebSocketStream<S>, backoff: &mut u64, mut accept: F) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnMut(&serde_json::Value) -> bool,
{
    let mut idle = interval(Duration::from_secs(5));
    let mut last_valid = Instant::now();
    idle.tick().await;
    loop {
        tokio::select! {
            _ = idle.tick() => {
                if last_valid.elapsed() > Duration::from_secs(15) { return Err("K线15秒未收到有效数据".into()); }
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(v) = serde_json::from_str(text.as_str()) && accept(&v) { last_valid = Instant::now(); *backoff = 2; }
                }
                Some(Ok(Message::Ping(payload))) => ws.send(Message::Pong(payload)).await.map_err(|_| "K线心跳发送失败".to_string())?,
                Some(Ok(Message::Close(_))) | None => return Err("K线连接已关闭".into()),
                Some(Err(_)) => return Err("K线传输失败".into()),
                Some(Ok(_)) => {}
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_four_intervals_use_one_target() {
        for i in ["1m", "15m", "1h", "1d"] {
            let token = ChartToken { account_generation: 1, chart_generation: 1, symbol: "BTCUSDT".into(), interval: i.into() };
            assert_eq!(build_url(&token), format!("wss://fstream.binance.com/market/ws/btcusdt@kline_{i}"));
        }
    }
    #[tokio::test]
    async fn only_valid_product_frames_reset_reconnect_backoff() {
        for valid in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let close = if valid { "105" } else { "NaN" };
                let message = serde_json::json!({"e":"kline","s":"BTCUSDT","k":{"s":"BTCUSDT","i":"1m","t":60000,"o":"100","h":"110","l":"90","c":close,"n":1,"x":false}});
                ws.send(Message::Text(message.to_string().into())).await.unwrap();
                ws.close(None).await.unwrap();
            });
            let token = ChartToken { account_generation: 0, chart_generation: 1, symbol: "BTCUSDT".into(), interval: "1m".into() };
            let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            let mut backoff = 32;
            let result = read_stream(ws, &mut backoff, |v| chart::parse_live(v, &token).is_some()).await;
            assert!(result.is_err());
            server.await.unwrap();
            assert_eq!(backoff, if valid { 2 } else { 32 });
        }
    }
    #[tokio::test]
    #[ignore = "手动公开行情烟测, 使用当前网络代理, 不访问账户或keyring"]
    async fn public_kline_smoke() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let token = ChartToken { account_generation: 0, chart_generation: 0, symbol: "BTCUSDT".into(), interval: "1m".into() };
            let ws = connect_ws(&build_url(&token)).await.expect("公开K线握手失败");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let mut tx = Some(tx);
            let mut backoff = 2;
            let read = read_stream(ws, &mut backoff, |v| {
                let valid = chart::parse_live(v, &token).is_some();
                if valid && let Some(tx) = tx.take() { let _ = tx.send(()); }
                valid
            });
            tokio::select! {
                result = read => panic!("公开K线未取得有效帧: {result:?}"),
                result = rx => { result.unwrap(); eprintln!("Rust公开K线: 已收到产品解析器接受的有效帧"); }
            }
        }).await.expect("公开K线烟测30秒超时");
    }
    #[tokio::test]
    async fn target_change_cancels_actual_pending_read_loop() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            std::future::pending::<()>().await;
            drop(ws);
        });
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let (tx, mut rx) = tokio::sync::watch::channel(0);
        let worker = tokio::spawn(async move { until_changed(&mut rx, read_stream(ws, &mut 2, |_| false)).await });
        tx.send_replace(1);
        assert!(tokio::time::timeout(Duration::from_secs(1), worker).await.unwrap().unwrap().is_none());
        server.abort();
    }
}
