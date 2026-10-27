use crate::state::now_ms;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Method, header::HeaderMap};
use serde_json::Value;
use sha2::Sha256;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use zeroize::Zeroize;

pub const REST_BASE: &str = "https://fapi.binance.com";
pub const WS_MARKET_BASE: &str = "wss://fstream.binance.com/market";
pub const WS_PRIVATE_BASE: &str = "wss://fstream.binance.com/private";

const RECV_WINDOW: &str = "5000";

/// API 凭证. Debug 实现做脱敏, 防止误打出日志.
#[derive(Clone)]
pub struct Creds {
    pub key: String,
    pub secret: String,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Creds {{ key: {}****, secret: **** }}", mask(&self.key))
    }
}

impl Drop for Creds {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// 只保留前 4 位, 用于日志定位是哪个 Key.
pub fn mask(s: &str) -> String {
    s.chars().take(4).collect()
}

#[derive(Debug)]
pub struct RestClient {
    http: reqwest::Client,
    /// 服务器时间与本地时间的偏差 (毫秒), 用于避免 -1021 签名过期.
    time_offset_ms: AtomicI64,
    /// 最近一次响应携带的 1 分钟已用权重.
    pub used_weight: AtomicU32,
    /// 被限流(429/418)后的解禁时间戳, 期间所有请求直接短路, 不再打服务器.
    blocked_until_ms: AtomicI64,
    #[cfg(test)]
    test_base: Option<String>,
}

#[cfg(test)]
impl RestClient {
    /// 只给本机 loopback 测试用: 指向假服务并禁用代理, 不触碰真实交易所.
    pub(crate) fn with_test_base(mut self, base: impl Into<String>) -> Self {
        self.test_base = Some(base.into());
        self.http = reqwest::Client::builder().no_proxy()
            .timeout(std::time::Duration::from_secs(2)).build().expect("测试客户端");
        self
    }
}

impl RestClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("binance-mini/0.1")
            // 代理不通时必须尽快失败并上报, 否则请求会挂到 TCP 超时才返回.
            .timeout(std::time::Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(safe_network_error)
            .expect("创建 REST 客户端失败");
        Self {
            http,
            time_offset_ms: AtomicI64::new(0),
            used_weight: AtomicU32::new(0),
            blocked_until_ms: AtomicI64::new(0),
            #[cfg(test)]
            test_base: None,
        }
    }

    pub fn used_weight(&self) -> u32 {
        self.used_weight.load(Ordering::Relaxed)
    }

    /// 距离限流解禁还剩多少毫秒, 0 表示当前可以请求.
    pub fn blocked_ms(&self) -> i64 {
        (self.blocked_until_ms.load(Ordering::Relaxed) - now_ms()).max(0)
    }

    fn now(&self) -> i64 {
        now_ms() + self.time_offset_ms.load(Ordering::Relaxed)
    }

    /// 校准服务器时间. 签名请求前必须跑一次, 否则跨时区机器容易被拒.
    pub async fn sync_time(&self) -> Result<(), String> {
        let v = self.get("/fapi/v1/time", &[], None).await?;
        let server = v
            .get("serverTime")
            .and_then(Value::as_i64)
            .ok_or_else(|| "服务器时间响应缺少 serverTime".to_string())?;
        self.time_offset_ms
            .store(server - now_ms(), Ordering::Relaxed);
        Ok(())
    }

    async fn get(
        &self,
        path: &str,
        params: &[(&str, String)],
        creds: Option<&Creds>,
    ) -> Result<Value, String> {
        self.request(Method::GET, path, params, creds, creds.is_some()).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        creds: Option<&Creds>,
        signed: bool,
    ) -> Result<Value, String> {
        let mut query = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        // 已发出的并发请求允许完成, 已知冷却生效后禁止发起新请求; 不串行阻塞风险查询.
        let blocked = self.blocked_ms();
        if blocked > 0 {
            return Err(format!("被限流, 冷却中({} 秒)", blocked / 1000 + 1));
        }
        if signed {
            let c = creds.ok_or("签名请求缺少凭据")?;
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&format!("timestamp={}&recvWindow={}", self.now(), RECV_WINDOW));
            let signature = sign(&c.secret, &query)?;
            query.push_str(&format!("&signature={signature}"));
        }
        let base = REST_BASE;
        #[cfg(test)]
        let base = self.test_base.as_deref().unwrap_or(base);
        let mut url = format!("{base}{path}");
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        let mut req = self.http.request(method, url);
        if let Some(c) = creds {
            req = req.header("X-MBX-APIKEY", &c.key);
        }
        let resp = req.send().await.map_err(|e| format!("网络错误: {}", safe_network_error(e)))?;
        let status = resp.status();
        // 必须先记录头再读正文, 正文超时/断开/被取消都不能丢掉冷却信号.
        self.observe_headers(status.as_u16(), resp.headers(), now_ms());
        let body = resp.text().await.map_err(|e| format!("读取响应失败: {}", safe_network_error(e)))?;
        if !status.is_success() {
            return Err(format_error(status.as_u16(), &body));
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|_| "解析 JSON 失败".to_string())
    }

    fn observe_headers(&self, status: u16, headers: &HeaderMap, now: i64) {
        if let Some(w) = headers.get("x-mbx-used-weight-1m")
            .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u32>().ok())
        {
            self.used_weight.store(w, Ordering::Relaxed);
        }
        if matches!(status, 418 | 429) {
            let until = headers.get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| retry_after_until(v, now))
                .unwrap_or_else(|| now.saturating_add(60_000));
            self.blocked_until_ms.fetch_max(until, Ordering::Relaxed);
        }
    }

    pub async fn position_risk(&self, c: &Creds) -> Result<Value, String> {
        self.get("/fapi/v2/positionRisk", &[], Some(c)).await
    }

    /// 平仓需要的合约过滤规则. 单合约查询, 权重低.
    pub async fn exchange_info_symbol(&self, symbol: &str) -> Result<Value, String> {
        self.get("/fapi/v1/exchangeInfo", &[("symbol", symbol.to_string())], None).await
    }

    /// 市价单写操作. 与其他请求共用冷却与请求预算.
    pub async fn new_market_order(&self, c: &Creds, params: &[(&str, String)]) -> Result<Value, String> {
        self.request(Method::POST, "/fapi/v1/order", params, Some(c), true).await
    }

    /// 用本地 clientOrderId 查单, 用于发单结果未知时判定真实状态.
    pub async fn query_order(&self, c: &Creds, symbol: &str, client_id: &str) -> Result<Value, String> {
        self.get("/fapi/v1/order", &[("symbol", symbol.to_string()), ("origClientOrderId", client_id.to_string())], Some(c)).await
    }

    pub async fn account(&self, c: &Creds) -> Result<Value, String> {
        self.get("/fapi/v2/account", &[], Some(c)).await
    }

    pub async fn klines(&self, symbol: &str, interval: &str, limit: u32) -> Result<Value, String> {
        self.get(
            "/fapi/v1/klines",
            &[
                ("symbol", symbol.to_string()),
                ("interval", interval.to_string()),
                ("limit", limit.to_string()),
            ],
            None,
        )
        .await
    }

    /// 当前挂单. 止盈止损是挂在订单上的, positionRisk 里没有, 只能从这里取.
    pub async fn open_orders(&self, symbol: &str, c: &Creds) -> Result<Value, String> {
        self.get("/fapi/v1/openOrders", &[("symbol", symbol.to_string())], Some(c))
            .await
    }

    /// 全量挂单 (不带 symbol). 只用于启动时诊断, 权重 40, 不能进轮询.
    pub async fn all_open_orders(&self, c: &Creds) -> Result<Value, String> {
        self.get("/fapi/v1/openOrders", &[], Some(c)).await
    }

    /// 历史订单 (含已成交/已撤销). 只用于诊断条件委托到底存在哪.
    pub async fn all_orders(
        &self,
        symbol: &str,
        limit: u32,
        c: &Creds,
    ) -> Result<Value, String> {
        self.get(
            "/fapi/v1/allOrders",
            &[("symbol", symbol.to_string()), ("limit", limit.to_string())],
            Some(c),
        )
        .await
    }

    /// 24 小时最高/最低/涨跌幅/成交额.
    pub async fn ticker_24hr(&self, symbol: &str) -> Result<Value, String> {
        self.get("/fapi/v1/ticker/24hr", &[("symbol", symbol.to_string())], None)
            .await
    }

    /// 标记价 + 资金费率 + 下次结算时间. WS 被墙时靠它兜底刷新.
    pub async fn premium_index(&self, symbol: &str) -> Result<Value, String> {
        self.get("/fapi/v1/premiumIndex", &[("symbol", symbol.to_string())], None)
            .await
    }

    pub async fn listen_key_create(&self, c: &Creds) -> Result<String, String> {
        // listenKey 是 API-key 接口, 不要求签名, 不把密钥材料带进 URL.
        let v = self.request(Method::POST, "/fapi/v1/listenKey", &[], Some(c), false).await?;
        v.get("listenKey")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "listenKey 响应字段缺失".to_string())
    }

    pub async fn listen_key_keepalive(&self, c: &Creds) -> Result<(), String> {
        self.request(Method::PUT, "/fapi/v1/listenKey", &[], Some(c), false).await?;
        Ok(())
    }
}

/// 不回传底层错误链或响应原文, 代理/服务端可能把认证 URL 或头部回显其中.
pub fn safe_network_error(error: reqwest::Error) -> &'static str {
    let error = error.without_url();
    if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败, 请检查网络或代理"
    } else if error.is_body() || error.is_decode() {
        "响应传输失败"
    } else {
        "网络请求失败"
    }
}

/// Retry-After 支持秒数与 HTTP 标准 IMF-fixdate, 无效值由调用方采用60秒保护.
fn retry_after_until(value: &str, now: i64) -> Option<i64> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: u64 = value.parse().unwrap_or(u64::MAX);
        return Some(now.saturating_add(seconds.saturating_mul(1000).min(i64::MAX as u64) as i64));
    }
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 || !parts[0].ends_with(',') || parts[5] != "GMT" {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter().position(|m| *m == parts[2])?;
    let year: i64 = parts[3].parse().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let leap = |y: i64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=month_days[month]).contains(&day) {
        return None;
    }
    let time: Vec<i64> = parts[4].split(':').map(str::parse).collect::<Result<_, _>>().ok()?;
    if time.len() != 3 || !(0..24).contains(&time[0]) || !(0..60).contains(&time[1]) || !(0..60).contains(&time[2]) {
        return None;
    }
    let days: i64 = (1970..year).map(|y| if leap(y) { 366 } else { 365 }).sum::<i64>()
        + month_days[..month].iter().sum::<i64>() + day - 1;
    Some(((days * 86_400 + time[0] * 3600 + time[1] * 60 + time[2]) * 1000).max(now))
}

pub fn sign(secret: &str, query: &str) -> Result<String, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| "HMAC 初始化失败".to_string())?;
    mac.update(query.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// 仅展示错误码和本地解释, 不展示可能含认证材料的服务端 msg.
fn format_error(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    // 451/403 由交易所按 IP/地区拒绝, 与应用权限和重试次数无关, 必须指引换出口 IP.
    let hint = match (status, code) {
        (451, _) => "交易所拒绝当前地区或 IP, 请切换代理节点或出口 IP 后重试",
        (403, _) => "访问被拒绝, 多为地区/IP 限制或代理拦截, 请切换代理节点或出口 IP",
        (_, -2015) => "认证或读取被拒绝, 请核查 API Key, IP 白名单, 合约账户状态与读取权限; 不要为排错开启交易或提现权限",
        (_, -2019) => "保证金不足, 交易所拒绝了该订单",
        (_, -2022) => "reduceOnly 被拒, 多为持仓模式或方向不符; 请核对单向/双向模式与持仓方向",
        (_, -4164) => "订单名义值低于交易所最小要求",
        (_, -1111) => "数量精度超过合约步长要求",
        (_, -4131) => "对手价保护拒绝该订单, 请稍后重试",
        (_, -4046) => "无需变更仓位, 当前持仓已满足该订单",
        (_, -1021) => "本机时间与服务器偏差过大",
        (_, -2014) => "API Key 格式错误",
        (_, -1102) => "参数缺失",
        _ => "交易所请求失败",
    };
    format!("HTTP {} (code {}) {}", status, code, hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicUsize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn local_client(response: String) -> (Arc<RestClient>, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                if n > 0 {
                    seen.fetch_add(1, Ordering::SeqCst);
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            }
        });
        let mut rest = RestClient::new();
        rest.http = reqwest::Client::builder().no_proxy()
            .timeout(std::time::Duration::from_secs(2)).build().unwrap();
        rest.test_base = Some(base);
        (Arc::new(rest), count, server)
    }

    fn fake_creds() -> Creds {
        Creds { key: "fake-key".into(), secret: "FAKE_SECRET".into() }
    }

    /// 币安官方文档签名示例: 用固定 secret 与 query 校验 HMAC 结果稳定.
    #[test]
    fn signature_matches_known_vector() {
        let sig = sign("NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j", "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559").unwrap();
        assert_eq!(sig, "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71");
    }

    #[test]
    fn mask_keeps_prefix_only() {
        assert_eq!(mask("abcdefgh"), "abcd");
    }

    #[test]
    fn error_hint_for_permission_code() {
        let s = format_error(401, r#"{"code":-2015,"msg":"Invalid API-key, IP, or permissions for action."}"#);
        assert!(!s.contains("启用合约"), "读取失败不得引导用户开启可能允许交易的权限");
        assert!(s.contains("HTTP 401 (code -2015)"));
        assert!(s.contains("API Key"));
        assert!(s.contains("IP 白名单"));
        assert!(s.contains("合约账户状态"));
        assert!(s.contains("读取权限"));
        assert!(s.contains("不要为排错开启交易或提现权限"));
        assert!(!s.contains("Invalid API-key"));
    }
    #[test]
    fn region_block_codes_point_at_exit_ip_without_echoing_server_msg() {
        for status in [451u16, 403] {
            let s = format_error(status, r#"{"code":0,"msg":"Service unavailable in your region: 1.2.3.4"}"#);
            assert!(s.contains(&format!("HTTP {status}")), "{s}");
            assert!(s.contains("地区"), "{s}");
            assert!(s.contains("IP"), "{s}");
            assert!(s.contains("代理节点或出口 IP"), "{s}");
            assert!(!s.contains("Service unavailable"), "不得回显服务端 msg");
            assert!(!s.contains("1.2.3.4"), "不得回显服务端回显的地址");
        }
        // 451 即使携带业务 code 也必须优先按地区拒绝解释, 不能退化成笼统失败.
        let s = format_error(451, r#"{"code":-1003,"msg":"x"}"#);
        assert!(s.contains("地区"), "{s}");
    }

    #[test]
    fn retry_after_and_concurrent_limits_never_shorten_cooldown() {
        assert_eq!(retry_after_until("120", 1000), Some(121_000));
        assert_eq!(retry_after_until("Wed, 21 Oct 2015 07:28:00 GMT", 0), Some(1_445_412_480_000));
        assert_eq!(retry_after_until("Wed, 31 Feb 2015 07:28:00 GMT", 0), None);
        assert_eq!(retry_after_until("invalid", 0), None);
        let rest = RestClient::new();
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        rest.observe_headers(418, &headers, 1000);
        headers.insert(reqwest::header::RETRY_AFTER, "1".parse().unwrap());
        rest.observe_headers(429, &headers, 2000);
        assert_eq!(rest.blocked_until_ms.load(Ordering::Relaxed), 121_000);
        headers.remove(reqwest::header::RETRY_AFTER);
        rest.observe_headers(429, &headers, 200_000);
        assert_eq!(rest.blocked_until_ms.load(Ordering::Relaxed), 260_000);
    }

    #[tokio::test]
    async fn all_methods_and_shared_entrypoints_obey_cooldown_even_with_broken_body() {
        for method in [Method::GET, Method::POST, Method::PUT] {
            for status in [429, 418] {
                let response = format!("HTTP/1.1 {status} Limited\r\nRetry-After: 120\r\nx-mbx-used-weight-1m: 2345\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{}}");
                let (rest, count, server) = local_client(response).await;
                assert!(rest.request(method.clone(), "/test", &[], Some(&fake_creds()), false).await.is_err());
                assert!(rest.blocked_ms() > 110_000);
                assert_eq!(rest.used_weight(), 2345);
                let other_entrypoint = rest.clone();
                assert!(other_entrypoint.position_risk(&fake_creds()).await.unwrap_err().contains("冷却"));
                assert!(other_entrypoint.listen_key_create(&fake_creds()).await.unwrap_err().contains("冷却"));
                assert!(other_entrypoint.listen_key_keepalive(&fake_creds()).await.unwrap_err().contains("冷却"));
                assert!(other_entrypoint.klines("X", "1m", 20).await.unwrap_err().contains("冷却"));
                assert_eq!(count.load(Ordering::SeqCst), 1);
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn known_cooldown_blocks_parallel_entrypoints() {
        let (rest, count, server) = local_client("HTTP/1.1 429 Limited\r\nRetry-After: 60\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()).await;
        let c = fake_creds();
        assert!(rest.position_risk(&c).await.is_err());
        let (a, b, d) = tokio::join!(rest.position_risk(&c), rest.listen_key_create(&c), rest.listen_key_keepalive(&c));
        assert!(a.is_err() && b.is_err() && d.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn error_outputs_never_echo_secret_signature_or_listen_key() {
        let body = r#"{"code":-2015,"msg":"FAKE_SECRET https://test/ws/FAKE_LISTEN_KEY?signature=FAKE_SIGNATURE"}"#;
        let response = format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (rest, _, server) = local_client(response).await;
        let err = rest.position_risk(&fake_creds()).await.unwrap_err();
        for secret in ["FAKE_SECRET", "FAKE_LISTEN_KEY", "signature=", "https://"] {
            assert!(!err.contains(secret));
        }
        server.abort();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let err = client.get("http://127.0.0.1:0/ws/FAKE_LISTEN_KEY?signature=FAKE_SIGNATURE")
            .send().await.unwrap_err();
        let safe = safe_network_error(err);
        assert!(!safe.contains("FAKE_") && !safe.contains("http"));
    }

    #[test]
    fn server_error_does_not_echo_credentials() {
        let error = format_error(401, r#"{"code":-2015,"msg":"FAKE_SECRET FAKE_LISTEN_KEY signature=FAKE_SIGNATURE"}"#);
        assert!(!error.contains("FAKE_"), "错误信息不得回显认证材料");
    }
}
