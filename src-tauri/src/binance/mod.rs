pub mod market_ws;
pub mod rest;
pub mod user_ws;

pub use rest::{Creds, RestClient, WS_BASE};

use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::WebSocketStream;

/// 合约名转小写流名, 交易所要求全小写.
pub fn stream_symbol(symbol: &str) -> String {
    symbol.to_lowercase()
}

/// 用 reqwest 完成 HTTP Upgrade, 再把连接交给 tungstenite.
///
/// 这样 WS 与 REST 共用同一套系统代理 (HTTPS_PROXY / ALL_PROXY) 和 TLS 配置,
/// 不必单独实现 CONNECT 隧道; 直连时同样走这条路径, 逻辑只有一份.
pub async fn connect_ws(url: &str) -> Result<WebSocketStream<reqwest::Upgraded>, String> {
    let client = reqwest::Client::builder()
        .user_agent("binance-mini/0.1")
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("WS 客户端初始化失败: {e}"))?;

    // reqwest 只认 http/https, wss 前缀要换成 https 才能走同一套代理与 TLS.
    let http_url = url
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| url.strip_prefix("ws://").map(|rest| format!("http://{rest}")))
        .unwrap_or_else(|| url.to_string());

    let resp = client
        .get(http_url)
        .header(reqwest::header::CONNECTION, "Upgrade")
        .header(reqwest::header::UPGRADE, "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .send()
        .await
        .map_err(|e| format!("WS 握手请求失败: {e}"))?;

    if resp.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("WS 握手被拒绝: HTTP {}", resp.status().as_u16()));
    }

    let upgraded = resp
        .upgrade()
        .await
        .map_err(|e| format!("WS 连接升级失败: {e}"))?;

    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
}
