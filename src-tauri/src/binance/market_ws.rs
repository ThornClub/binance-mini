use crate::binance::{WS_BASE, connect_ws, stream_symbol};
use crate::engine;
use crate::state::AppState;
use futures_util::StreamExt;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;

/// 公开行情流: 单条组合流承载所有持仓合约的标记价与 1 分钟 K 线.
/// 订阅集合变化时整条连接重建 (设计取舍 D7).
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let mut symbols_rx = state.subscribe_symbols();
        let mut backoff = 2u64;

        loop {
            let symbols = symbols_rx.borrow().clone();
            if symbols.is_empty() {
                // 无持仓时不需要行情流, 这是正常状态, 不能标成故障.
                engine::set_market_state(&app, true, None);
                if symbols_rx.changed().await.is_err() {
                    return;
                }
                continue;
            }

            match run_once(&app, &symbols, &mut symbols_rx).await {
                Ok(()) => {
                    backoff = 2;
                }
                Err(e) => {
                    engine::set_market_state(&app, false, Some(e));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    });
}

fn build_url(symbols: &[String]) -> String {
    let streams: Vec<String> = symbols
        .iter()
        .flat_map(|s| {
            let s = stream_symbol(s);
            vec![format!("{}@markPrice@1s", s), format!("{}@kline_1m", s)]
        })
        .collect();
    format!("{}/stream?streams={}", WS_BASE, streams.join("/"))
}

async fn run_once(
    app: &AppHandle,
    symbols: &[String],
    symbols_rx: &mut tokio::sync::watch::Receiver<Vec<String>>,
) -> Result<(), String> {
    let url = build_url(symbols);
    let ws = connect_ws(&url).await.map_err(|e| format!("行情流连接失败: {e}"))?;

    eprintln!("[market] 已连接: {url}");
    engine::set_market_state(app, true, None);
    let mut stream = ws;
    // 30 秒没有消息就认为连接半死, 主动重连.
    let mut idle = interval(Duration::from_secs(30));
    idle.tick().await;

    let result = loop {
        tokio::select! {
            _ = idle.tick() => {
                if engine::market_idle_ms(app) > 60_000 {
                    break Err("行情流已连接但 60 秒收不到推送, 检查网络或代理".to_string());
                }
            }
            changed = symbols_rx.changed() => {
                break match changed {
                    Ok(()) => Ok(()),
                    Err(_) => Err("订阅集通道关闭".to_string()),
                };
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(txt))) => {
                        engine::touch_market(app);
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(txt.as_str()) {
                            engine::handle_market_event(app, &v);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(format!("行情流错误: {e}")),
                    None => break Err("行情流被关闭".to_string()),
                }
            }
        }
    };

    engine::set_market_state(app, false, None);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_lists_mark_price_and_kline_per_symbol() {
        let url = build_url(&["BTCUSDT".to_string(), "ETHUSDT".to_string()]);
        assert!(url.starts_with("wss://fstream.binance.com/stream?streams="));
        // 币安的流名一律小写
        assert!(url.contains("btcusdt@markPrice@1s"));
        assert!(url.contains("btcusdt@kline_1m"));
        assert!(url.contains("ethusdt@markPrice@1s"));
        // 每个合约 3 个 @: symbol@markPrice@1s 与 symbol@kline_1m
        assert_eq!(url.matches('@').count(), 6);
    }
}
