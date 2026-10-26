pub mod market_ws;
pub mod kline_ws;
pub mod rest;
pub mod user_ws;

pub use rest::{Creds, RestClient, WS_MARKET_BASE, WS_PRIVATE_BASE};

use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::WebSocketStream;

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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
        .connect_timeout(HANDSHAKE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("WS 客户端初始化失败: {}", rest::safe_network_error(e)))?;
    connect_ws_with(&client, url).await
}

async fn connect_ws_with(client: &reqwest::Client, url: &str) -> Result<WebSocketStream<reqwest::Upgraded>, String> {
    // 总期限包含请求、响应头和 upgrade, 建连后的长连接不受此期限影响.
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
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
            .map_err(|e| format!("WS 握手请求失败: {}", rest::safe_network_error(e)))?;
        if resp.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
            return Err(format!("WS 握手被拒绝: HTTP {}", resp.status().as_u16()));
        }
        let upgraded = resp
            .upgrade()
            .await
            .map_err(|e| format!("WS 连接升级失败: {}", rest::safe_network_error(e)))?;
        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
    }).await.map_err(|_| "WS 完整握手超时(15秒)".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::until_changed;
    use tokio::net::TcpListener;
    use tokio::sync::{oneshot, watch};

    async fn hanging_server() -> (String, oneshot::Receiver<()>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws/FAKE_LISTEN_KEY", listener.local_addr().unwrap());
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = tx.send(());
            std::future::pending::<()>().await;
            drop(socket);
        });
        (url, rx, server)
    }

    #[tokio::test]
    async fn tcp_connected_without_upgrade_hits_total_deadline() {
        let (url, accepted, server) = hanging_server().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let connect = tokio::spawn(async move { connect_ws_with(&client, &url).await });
        accepted.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(HANDSHAKE_TIMEOUT + std::time::Duration::from_secs(1)).await;
        let error = connect.await.unwrap().unwrap_err();
        assert!(error.contains("15秒"));
        assert!(!error.contains("FAKE_LISTEN_KEY"));
        server.abort();
    }

    #[tokio::test]
    async fn change_cancels_hanging_handshake_before_deadline() {
        let (url, accepted, server) = hanging_server().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (tx, mut rx) = watch::channel(0u64);
        let connect = tokio::spawn(async move { until_changed(&mut rx, connect_ws_with(&client, &url)).await });
        accepted.await.unwrap();
        tx.send_replace(1);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(1), connect).await.unwrap().unwrap().is_none());
        server.abort();
    }
}
