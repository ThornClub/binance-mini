use crate::state::now_ms;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use zeroize::Zeroize;

pub const REST_BASE: &str = "https://fapi.binance.com";
pub const WS_BASE: &str = "wss://fstream.binance.com";

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

#[derive(Debug, Default)]
pub struct RestClient {
    http: reqwest::Client,
    /// 服务器时间与本地时间的偏差 (毫秒), 用于避免 -1021 签名过期.
    time_offset_ms: AtomicI64,
    /// 最近一次响应携带的 1 分钟已用权重.
    pub used_weight: AtomicU32,
    /// 被限流(429/418)后的解禁时间戳, 期间所有请求直接短路, 不再打服务器.
    blocked_until_ms: AtomicI64,
}

impl RestClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("binance-mini/0.1")
            // 代理不通时必须尽快失败并上报, 否则请求会挂到 TCP 超时才返回.
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .unwrap_or_default();
        Self {
            http,
            time_offset_ms: AtomicI64::new(0),
            used_weight: AtomicU32::new(0),
            blocked_until_ms: AtomicI64::new(0),
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
        let mut query = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let mut url = format!("{}{}", REST_BASE, path);

        if let Some(c) = creds {
            let signed_query = if query.is_empty() {
                format!("timestamp={}&recvWindow={}", self.now(), RECV_WINDOW)
            } else {
                format!(
                    "{}&timestamp={}&recvWindow={}",
                    query,
                    self.now(),
                    RECV_WINDOW
                )
            };
            let signature = sign(&c.secret, &signed_query)?;
            query = format!("{}&signature={}", signed_query, signature);
        }

        if !query.is_empty() {
            url = format!("{}?{}", url, query);
        }

        let blocked = self.blocked_ms();
        if blocked > 0 {
            return Err(format!("被限流, 冷却中({} 秒)", blocked / 1000 + 1));
        }

        let mut req = self.http.get(url);
        if let Some(c) = creds {
            req = req.header("X-MBX-APIKEY", &c.key);
        }

        let resp = req.send().await.map_err(|e| format!("网络错误: {e}"))?;

        if let Some(w) = resp
            .headers()
            .get("x-mbx-used-weight-1m")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u32>().ok())
        {
            self.used_weight.store(w, Ordering::Relaxed);
        }

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if !status.is_success() {
            // 429/418 是限流信号: 记一个 60 秒冷却窗口, 期间不再发请求.
            if matches!(status.as_u16(), 418 | 429) {
                self.blocked_until_ms
                    .store(now_ms() + 60_000, Ordering::Relaxed);
            }
            return Err(format_error(status.as_u16(), &body));
        }
        serde_json::from_str(&body).map_err(|e| format!("解析 JSON 失败: {e}"))
    }

    pub async fn position_risk(&self, c: &Creds) -> Result<Value, String> {
        self.get("/fapi/v2/positionRisk", &[], Some(c)).await
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
        let v = self.get_signed_no_params("/fapi/v1/listenKey", c).await?;
        v.get("listenKey")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "listenKey 响应字段缺失".to_string())
    }

    /// listenKey 只在 POST 创建, 续期是 PUT. 两者都不能带参数.
    async fn get_signed_no_params(&self, path: &str, c: &Creds) -> Result<Value, String> {
        let url = format!(
            "{}?timestamp={}&recvWindow={}",
            path,
            self.now(),
            RECV_WINDOW
        );
        let signature = sign(&c.secret, &url)?;
        let full = format!("{}{}&signature={}", REST_BASE, url, signature);
        let resp = self
            .http
            .post(&full)
            .header("X-MBX-APIKEY", &c.key)
            .send()
            .await
            .map_err(|e| format!("网络错误: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if !status.is_success() {
            // 429/418 是限流信号: 记一个 60 秒冷却窗口, 期间不再发请求.
            if matches!(status.as_u16(), 418 | 429) {
                self.blocked_until_ms
                    .store(now_ms() + 60_000, Ordering::Relaxed);
            }
            return Err(format_error(status.as_u16(), &body));
        }
        serde_json::from_str(&body).map_err(|e| format!("解析 JSON 失败: {e}"))
    }

    pub async fn listen_key_keepalive(&self, c: &Creds) -> Result<(), String> {
        let query = format!("timestamp={}&recvWindow={}", self.now(), RECV_WINDOW);
        let signature = sign(&c.secret, &query)?;
        let url = format!(
            "{}/fapi/v1/listenKey?{}&signature={}",
            REST_BASE, query, signature
        );
        let resp = self
            .http
            .put(&url)
            .header("X-MBX-APIKEY", &c.key)
            .send()
            .await
            .map_err(|e| format!("网络错误: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format_error(status, &body));
        }
        Ok(())
    }
}

pub fn sign(secret: &str, query: &str) -> Result<String, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("HMAC 初始化失败: {e}"))?;
    mac.update(query.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// 把币安错误码翻成人话, 便于 UI 直接展示.
fn format_error(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    let msg = v
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("未知错误")
        .to_string();
    let hint = match code {
        -2015 => "API Key 无效或缺少合约权限, 请检查是否勾选\"启用合约\"以及 IP 白名单",
        -1021 => "本机时间与服务器偏差过大",
        -2014 => "API Key 格式错误",
        -1102 => "参数缺失",
        _ => "",
    };
    if hint.is_empty() {
        format!("HTTP {} (code {}) {}", status, code, msg)
    } else {
        format!("HTTP {} (code {}) {} — {}", status, code, msg, hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 币安官方文档签名示例: 用固定 secret 与 query 校验 HMAC 结果稳定.
    #[test]
    fn signature_matches_known_vector() {
        let sig = sign("NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j", "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559").unwrap();
        assert_eq!(
            sig,
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }

    #[test]
    fn mask_keeps_prefix_only() {
        assert_eq!(mask("abcdefgh"), "abcd");
    }

    #[test]
    fn error_hint_for_permission_code() {
        let s = format_error(
            401,
            r#"{"code":-2015,"msg":"Invalid API-key, IP, or permissions for action."}"#,
        );
        assert!(s.contains("启用合约"));
    }
}
