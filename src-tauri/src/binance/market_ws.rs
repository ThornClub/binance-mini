use crate::binance::{WS_MARKET_BASE, connect_ws, stream_symbol};
use crate::engine;
use crate::state::{AppState, MarketSubscription, until_changed};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::time::{Instant, interval};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

/// 风险行情流只订阅markPrice, 图表焦点与周期变化不影响本连接.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let mut symbols_rx = state.subscribe_symbols();
        let mut backoff = 2u64;
        loop {
            let sub = symbols_rx.borrow_and_update().clone();
            if sub.symbols.is_empty() {
                engine::set_market_state(&app, &sub, true, None);
                if symbols_rx.changed().await.is_err() { return; }
                backoff = 2;
                continue;
            }
            match until_changed(&mut symbols_rx, run_once(&app, &sub, &mut backoff)).await {
                None | Some(Ok(())) => {
                    backoff = 2;
                    if symbols_rx.has_changed().is_err() { return; }
                }
                Some(Err(e)) => {
                    engine::set_market_state(&app, &sub, false, Some(e));
                    if until_changed(&mut symbols_rx, tokio::time::sleep(Duration::from_secs(backoff))).await.is_none() { backoff = 2; }
                    else { backoff = (backoff * 2).min(60); }
                }
            }
        }
    });
}
fn build_url(symbols: &[String]) -> String {
    let streams: Vec<String> = symbols.iter().map(|s| format!("{}@markPrice@1s", stream_symbol(s))).collect();
    format!("{}/stream?streams={}", WS_MARKET_BASE, streams.join("/"))
}
async fn run_once(app: &AppHandle, sub: &MarketSubscription, backoff: &mut u64) -> Result<(), String> {
    let ws = connect_ws(&build_url(&sub.symbols)).await.map_err(|e| format!("行情流连接失败: {e}"))?;
    engine::set_market_state(app, sub, true, None);
    read_stream(ws, backoff, |v| engine::handle_market_event(app, sub, v)).await
}
async fn read_stream<S, F>(mut ws: WebSocketStream<S>, backoff: &mut u64, mut accept: F) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnMut(&serde_json::Value) -> bool,
{
    let mut idle = interval(Duration::from_secs(30));
    let mut last_valid = Instant::now();
    idle.tick().await;
    loop {
        tokio::select! {
            _ = idle.tick() => {
                if last_valid.elapsed() > Duration::from_secs(60) { return Err("行情流60秒未收到有效推送".into()); }
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(txt))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(txt.as_str())
                        && accept(&v) { last_valid = Instant::now(); *backoff = 2; }
                }
                Some(Ok(Message::Ping(payload))) => {
                    ws.send(Message::Pong(payload)).await.map_err(|_| "行情流心跳发送失败".to_string())?;
                }
                Some(Ok(Message::Close(_))) | None => return Err("行情流被关闭".into()),
                Some(Ok(_)) => {}
                Some(Err(_)) => return Err("行情流传输失败".into()),
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AlertLine;

    #[test]
    fn url_lists_only_mark_price_per_symbol() {
        let url = build_url(&["BTCUSDT".to_string(), "ETHUSDT".to_string()]);
        assert!(url.starts_with("wss://fstream.binance.com/market/stream?streams="));
        assert!(url.contains("btcusdt@markPrice@1s"));
        assert!(url.contains("ethusdt@markPrice@1s"));
        assert!(!url.contains("kline"));
        assert_eq!(url.matches('@').count(), 4);
    }
    #[test]
    fn public_mark_url_uses_market_partition() {
        assert_eq!(build_url(&["BTCUSDT".to_string()]),
            "wss://fstream.binance.com/market/stream?streams=btcusdt@markPrice@1s");
    }
    #[tokio::test]
    #[ignore = "手动公开行情烟测, 使用当前网络代理, 不访问账户或keyring"]
    async fn public_mark_smoke() {
        tokio::time::timeout(Duration::from_secs(30), async {
            // 空内存状态只承载公开行情解析, 不安装凭据或发起账户请求.
            let state = AppState::new();
            let sub = MarketSubscription { symbols: vec!["BTCUSDT".into()], ..Default::default() };
            let ws = connect_ws(&build_url(&sub.symbols)).await.expect("公开标记价握手失败");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let mut tx = Some(tx);
            let mut backoff = 2;
            let read = read_stream(ws, &mut backoff, |v| {
                let valid = engine::apply_market_event(&state, &sub, v).is_some();
                if valid && let Some(tx) = tx.take() { let _ = tx.send(()); }
                valid
            });
            tokio::select! {
                result = read => panic!("公开标记价未取得有效帧: {result:?}"),
                result = rx => { result.unwrap(); eprintln!("Rust公开标记价: 已收到产品解析器接受的有效帧"); }
            }
        }).await.expect("公开标记价烟测30秒超时");
    }
    #[tokio::test]
    async fn only_valid_product_frames_reset_reconnect_backoff() {
        for valid in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let price = if valid { "100" } else { "NaN" };
                let message = serde_json::json!({"stream":"btcusdt@markPrice@1s","data":{"e":"markPriceUpdate","s":"BTCUSDT","p":price}});
                ws.send(Message::Text(message.to_string().into())).await.unwrap();
                ws.close(None).await.unwrap();
            });
            let state = AppState::new();
            state.set_creds(Some(crate::binance::Creds { key: "fake".into(), secret: "fake".into() }));
            state.write(|s| s.alerts.push(AlertLine { id: "line".into(), symbol: "BTCUSDT".into(), price: 1.0, enabled: true }));
            let sub = state.subscribe_symbols().borrow().clone();
            let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            let mut backoff = 32;
            let result = read_stream(ws, &mut backoff, |v| engine::apply_market_event(&state, &sub, v).is_some()).await;
            assert!(result.is_err());
            server.await.unwrap();
            assert_eq!(backoff, if valid { 2 } else { 32 });
        }
    }
}
