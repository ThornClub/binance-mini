use crate::binance::RestClient;
use crate::freshness::{CHART_MAX_AGE_MS, CHART_REFRESH_INTERVAL_MS, Freshness};
use crate::state::{AppState, now_ms, until_changed};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Candle {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub trades: u64,
    pub closed: bool,
}
impl Candle {
    pub fn valid(&self) -> bool {
        self.t > 0 && self.t <= 9_007_199_254_740_991 && self.trades <= 9_007_199_254_740_991 &&
            [self.o, self.h, self.l, self.c].iter().all(|x| x.is_finite() && *x > 0.0) &&
            self.h >= self.o.max(self.c).max(self.l) && self.l <= self.o.min(self.c)
    }
    pub fn newer_than(&self, old: &Self) -> bool {
        self.t > old.t || (self.t == old.t &&
            ((self.trades > old.trades && (!old.closed || self.closed)) ||
             (self.trades == old.trades && self.closed && !old.closed)))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChartToken {
    pub account_generation: u64,
    pub chart_generation: u64,
    pub symbol: String,
    pub interval: String,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChartStatus { Loading, Recovering, Live, Polling, Stale, Error }
#[derive(Clone, Debug, Serialize)]
pub struct ChartView {
    pub token: Option<ChartToken>,
    pub reconnect_epoch: u64,
    pub live_seq: u64,
    pub candle: Option<Candle>,
    pub connected: bool,
    pub connection_error: Option<String>,
    pub last_live_ms: i64,
    pub refresh_interval_ms: i64,
    pub status: ChartStatus,
    pub freshness: Freshness,
    #[serde(skip)]
    pub request_id: u64,
    #[serde(skip)]
    pub history_ready: bool,
}
impl Default for ChartView {
    fn default() -> Self {
        Self { token: None, reconnect_epoch: 0, live_seq: 0, candle: None, connected: false,
            connection_error: None, last_live_ms: 0, refresh_interval_ms: CHART_REFRESH_INTERVAL_MS,
            status: ChartStatus::Loading, freshness: Freshness::new(CHART_MAX_AGE_MS), request_id: 0, history_ready: false }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct ChartHistory {
    pub token: ChartToken,
    pub reconnect_epoch: u64,
    pub request_id: u64,
    pub rows: Vec<Candle>,
}
pub fn valid_target(symbol: &str, interval: &str) -> bool {
    !symbol.is_empty() && symbol.len() <= 40 && symbol.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') &&
        matches!(interval, "1m" | "15m" | "1h" | "1d")
}
fn number(v: &Value) -> Option<f64> { v.as_str()?.parse::<f64>().ok().filter(|v| v.is_finite()) }
pub fn parse_history(value: &Value) -> Result<Vec<Candle>, String> {
    let arr = value.as_array().filter(|a| !a.is_empty()).ok_or("K线返回空或非数组")?;
    let mut rows: Vec<Candle> = Vec::with_capacity(arr.len());
    for item in arr {
        let cols = item.as_array().ok_or("K线行无效")?;
        let price = |i: usize| cols.get(i).and_then(number).ok_or("K线价格无效");
        let candle = Candle {
            t: cols.first().and_then(Value::as_i64).ok_or("K线时间无效")?,
            o: price(1)?, h: price(2)?, l: price(3)?, c: price(4)?,
            trades: cols.get(8).and_then(Value::as_u64).ok_or("K线成交笔数无效")?, closed: false,
        };
        if !candle.valid() || rows.last().is_some_and(|last| candle.t <= last.t) { return Err("K线顺序或OHLC无效".into()); }
        rows.push(candle);
    }
    // 下一根已经存在才确认上一根结束, 不以未校准本机时钟判断最后一根.
    let closed_count = rows.len().saturating_sub(1);
    for candle in rows.iter_mut().take(closed_count) { candle.closed = true; }
    Ok(rows)
}
pub fn parse_live(value: &Value, token: &ChartToken) -> Option<Candle> {
    let data = value.get("data").unwrap_or(value);
    if data.get("e")?.as_str()? != "kline" || data.get("s")?.as_str()? != token.symbol { return None; }
    let k = data.get("k")?;
    if k.get("s")?.as_str()? != token.symbol || k.get("i")?.as_str()? != token.interval { return None; }
    let candle = Candle { t: k.get("t")?.as_i64()?, o: number(k.get("o")?)?, h: number(k.get("h")?)?,
        l: number(k.get("l")?)?, c: number(k.get("c")?)?, trades: k.get("n")?.as_u64()?, closed: k.get("x")?.as_bool()? };
    candle.valid().then_some(candle)
}
pub fn begin_connection(state: &AppState, token: &ChartToken) -> Option<u64> {
    state.write_if_chart(token, None, |chart| {
        // 失败尝试不建立新数据边界, 在途REST与已补齐历史仍有效.
        chart.connected = false;
        chart.last_live_ms = 0;
        chart.status = if chart.history_ready { ChartStatus::Polling } else { ChartStatus::Recovering };
        chart.reconnect_epoch
    })
}
pub fn connection_result(state: &AppState, token: &ChartToken, epoch: u64, result: Result<(), String>) -> Option<u64> {
    state.write_if_chart(token, Some(epoch), |chart| {
        chart.connected = result.is_ok();
        match result {
            Ok(()) => {
                // 握手期间可能跨过K线边界, 之前的历史不能证明本次连接已补齐.
                chart.reconnect_epoch += 1;
                chart.request_id = 0;
                chart.history_ready = false;
                chart.last_live_ms = 0;
                chart.connection_error = None;
                chart.status = ChartStatus::Recovering;
            }
            Err(error) => {
                chart.connection_error = Some(error);
                chart.status = if chart.history_ready { ChartStatus::Polling } else { ChartStatus::Stale };
            }
        }
        chart.reconnect_epoch
    })
}
pub fn apply_live(state: &AppState, token: &ChartToken, epoch: u64, value: &Value, now: i64) -> bool {
    let Some(candle) = parse_live(value, token) else { return false };
    state.write_if_chart(token, Some(epoch), |chart| {
        if chart.candle.as_ref().is_some_and(|old| candle != *old && !candle.newer_than(old)) { return false; }
        // 无成交时相同有效帧仍证明数据源活跃, 但不会改变OHLC版本.
        chart.candle = Some(candle);
        chart.live_seq += 1;
        chart.last_live_ms = now;
        chart.connection_error = None;
        chart.freshness.last_success_ms = now;
        // live不消除补洞失败, 必须由成功history确认恢复完整窗口.
        if chart.history_ready && chart.connected { chart.freshness.error = None; chart.status = ChartStatus::Live; }
        true
    }).unwrap_or(false)
}
pub async fn history(state: &AppState, rest: &RestClient, token: ChartToken, epoch: u64, request: u64, limit: u32) -> Result<ChartHistory, String> {
    let mut rx = state.subscribe_chart();
    if rx.borrow_and_update().as_ref() != Some(&token) { return Err("图表会话已变更".into()); }
    let accepted = state.write_if_chart(&token, Some(epoch), |chart| {
        if request <= chart.request_id { return false; }
        chart.request_id = request;
        true
    }).unwrap_or(false);
    if !accepted { return Err("图表请求已过期".into()); }
    let result = until_changed(&mut rx, async {
        let value = rest.klines(&token.symbol, &token.interval, limit.clamp(20, 500)).await?;
        parse_history(&value)
    }).await.ok_or("图表会话已变更")?;
    finish_history(state, token, epoch, request, result, now_ms())
}
pub fn finish_history(state: &AppState, token: ChartToken, epoch: u64, request: u64, result: Result<Vec<Candle>, String>, now: i64) -> Result<ChartHistory, String> {
    state.write_if_chart(&token, Some(epoch), |chart| {
        if chart.request_id != request { return Err("图表请求已过期".into()); }
        match result {
            Ok(rows) => {
                chart.history_ready = true;
                chart.freshness.success(now);
                chart.status = if chart.connected && chart.last_live_ms > 0 { ChartStatus::Live }
                    else if chart.connected { ChartStatus::Recovering } else { ChartStatus::Polling };
                Ok(rows)
            }
            Err(error) => {
                chart.history_ready = false;
                chart.status = if chart.candle.is_some() { ChartStatus::Stale } else { ChartStatus::Error };
                chart.freshness.error = Some(error.clone());
                Err(error)
            }
        }
    }).ok_or("图表会话已变更")?.map(|rows| ChartHistory { token, reconnect_epoch: epoch, request_id: request, rows })
}

#[cfg(test)]
mod tests;
