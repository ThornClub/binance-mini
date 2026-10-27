use crate::binance::{Creds, RestClient};
use crate::state::{AppState, Side, now_ms};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// 平仓依赖的交易所过滤规则. 任一字段缺失都拒绝下单, 不用猜测的默认值.
#[derive(Clone, Debug, PartialEq)]
pub struct Filters {
    pub step: f64,
    pub min_notional: f64,
    pub status: String,
}

const FILTERS_TTL_MS: i64 = 300_000;

/// 解析 exchangeInfo 的单合约过滤规则. 市价单优先用 MARKET_LOT_SIZE.
pub fn parse_filters(v: &Value, symbol: &str) -> Result<Filters, String> {
    let list = v.get("symbols").and_then(Value::as_array).ok_or("合约规则返回非数组")?;
    let item = list.iter()
        .find(|s| s.get("symbol").and_then(Value::as_str) == Some(symbol))
        .ok_or_else(|| format!("未找到 {symbol} 的合约规则"))?;
    let status = item.get("status").and_then(Value::as_str).unwrap_or_default().to_string();
    let filters = item.get("filters").and_then(Value::as_array).ok_or("合约规则缺少 filters")?;
    let field = |kind: &str, name: &str| filters.iter()
        .find(|x| x.get("filterType").and_then(Value::as_str) == Some(kind))
        .and_then(|x| x.get(name))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0);
    let step = field("MARKET_LOT_SIZE", "stepSize").or_else(|| field("LOT_SIZE", "stepSize"))
        .ok_or("合约缺少有效下单步长")?;
    let min_notional = field("MIN_NOTIONAL", "notional").or_else(|| field("NOTIONAL", "notional"))
        .ok_or("合约缺少最小名义值")?;
    Ok(Filters { step, min_notional, status })
}

/// 按交易所步长向下取整, 任何情况下都不放大量.
pub fn round_down_qty(qty: f64, step: f64) -> f64 {
    if !qty.is_finite() || !step.is_finite() || qty <= 0.0 || step <= 0.0 { return 0.0; }
    let steps = (qty / step + 1e-9).floor();
    let out = normalize(steps * step, step);
    if out > qty && steps >= 1.0 { normalize((steps - 1.0) * step, step) } else { out }
}

/// 归一化到步长的小数位, 去掉浮点尾巴.
fn normalize(value: f64, step: f64) -> f64 {
    let decimals = step_decimals(step);
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

pub fn step_decimals(step: f64) -> u32 {
    (0..=8).find(|d| { let scaled = step * 10f64.powi(*d as i32); (scaled - scaled.round()).abs() < 1e-9 })
        .unwrap_or(8)
}

/// 提交给交易所的数量文本, 必须正好是步长整数倍.
pub fn format_qty(qty: f64, step: f64) -> String {
    format!("{:.*}", step_decimals(step) as usize, normalize(qty, step))
}

/// 平仓计划: 预览与发单共用同一份计算, 避免两处规则漂移.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ClosePlan {
    pub symbol: String,
    pub qty: f64,
    pub qty_text: String,
    pub notional: f64,
    /// 平仓实际发送的方向: 多仓 SELL, 空仓 BUY.
    pub order_side: String,
    pub position_side: String,
    pub reduce_only: bool,
    pub max_qty: f64,
    pub mark_price: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CloseResult {
    pub order_id: i64,
    pub status: String,
    pub filled_qty: f64,
    pub avg_price: f64,
    pub qty_text: String,
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_str).and_then(|x| x.parse::<f64>().ok()).filter(|x| x.is_finite())
}

/// 从 positionRisk 找到目标方向的持仓, 返回 (positionSide, 带符号持仓量).
pub fn find_position(risk: &Value, symbol: &str, side: Side) -> Result<(String, f64), String> {
    let list = risk.as_array().ok_or("positionRisk 返回非数组")?;
    for p in list {
        if p.get("symbol").and_then(Value::as_str) != Some(symbol) { continue; }
        let position_side = p.get("positionSide").and_then(Value::as_str).unwrap_or("BOTH");
        let matched = match side {
            Side::Both => position_side == "BOTH",
            Side::Long => position_side == "LONG",
            Side::Short => position_side == "SHORT",
        };
        let amount = num(p, "positionAmt").unwrap_or(0.0);
        if matched && amount != 0.0 { return Ok((position_side.to_string(), amount)); }
    }
    Err(format!("当前没有 {symbol} 可平仓的{}持仓", side_label(side)))
}

fn side_label(side: Side) -> &'static str {
    match side { Side::Long => "多", Side::Short => "空", Side::Both => "" }
}

fn mark_price_of(risk: &Value, symbol: &str) -> f64 {
    risk.as_array().and_then(|list| list.iter()
        .find(|p| p.get("symbol").and_then(Value::as_str) == Some(symbol)))
        .and_then(|p| num(p, "markPrice")).filter(|m| *m > 0.0).unwrap_or(0.0)
}

/// 校验并量化平仓数量. 数量上限来自调用方刚读到的持仓, 不信任前端数字.
pub fn plan_close(symbol: &str, amount: f64, position_side: String,
    requested: f64, mark: f64, filters: &Filters) -> Result<ClosePlan, String> {
    if filters.status != "TRADING" {
        return Err(format!("{symbol} 当前状态为 {} 不可交易", if filters.status.is_empty() { "未知" } else { &filters.status }));
    }
    let max_qty = amount.abs();
    if !requested.is_finite() || requested <= 0.0 { return Err("平仓数量必须大于 0".to_string()); }
    if requested > max_qty && requested - max_qty > max_qty * 1e-9 {
        return Err(format!("平仓数量不能超过当前持仓 {max_qty}"));
    }
    let qty = round_down_qty(requested.min(max_qty), filters.step);
    if qty <= 0.0 {
        return Err(format!("按步长 {} 取整后数量为 0, 请至少输入 {}", filters.step, format_qty(filters.step, filters.step)));
    }
    if mark <= 0.0 { return Err("缺少有效标记价, 无法校验最小名义值".to_string()); }
    let notional = qty * mark;
    if notional < filters.min_notional {
        let need = round_down_qty(filters.min_notional / mark, filters.step);
        // 名义值不足时给出能通过校验的最小数量, 但仍受当前持仓限制.
        let need = if (filters.min_notional / mark) > need { normalize(need + filters.step, filters.step) } else { need };
        return Err(format!("名义值 {:.2} USDT 低于最小要求 {:.2}, 至少约 {}",
            notional, filters.min_notional, format_qty(need.min(max_qty), filters.step)));
    }
    Ok(ClosePlan {
        symbol: symbol.to_string(),
        qty,
        qty_text: format_qty(qty, filters.step),
        notional,
        order_side: if amount > 0.0 { "SELL".into() } else { "BUY".into() },
        position_side: position_side.clone(),
        // 单向持仓用 reduceOnly; 双向持仓用 positionSide, 两者不能同时出现.
        reduce_only: position_side == "BOTH",
        max_qty,
        mark_price: mark,
    })
}

/// 组装下单参数. 单向模式不带 positionSide, 双向模式不带 reduceOnly.
pub fn order_params(plan: &ClosePlan, client_id: &str) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("symbol", plan.symbol.clone()),
        ("side", plan.order_side.clone()),
        ("type", "MARKET".to_string()),
        ("quantity", plan.qty_text.clone()),
        ("newClientOrderId", client_id.to_string()),
    ];
    if plan.reduce_only { params.push(("reduceOnly", "true".to_string())); }
    else { params.push(("positionSide", plan.position_side.clone())); }
    params
}

/// 网络中断/超时后结果不可判定, 必须查单而不是重发.
fn result_unknown(error: &str) -> bool {
    error.starts_with("网络错误") || error.starts_with("读取响应失败")
}

async fn filters_for(state: &AppState, rest: &RestClient, symbol: &str) -> Result<Filters, String> {
    let now = now_ms();
    if let Some(cached) = state.cached_filters(symbol, now, FILTERS_TTL_MS) { return Ok(cached); }
    // 拉取失败不回退旧缓存, 避免用过期规则下单.
    let value = rest.exchange_info_symbol(symbol).await?;
    let filters = parse_filters(&value, symbol)?;
    state.store_filters(symbol, filters.clone(), now);
    Ok(filters)
}

/// 预览与发单共用的服务端校验: 重新读持仓, 重新读规则.
async fn prepare(state: &AppState, rest: &RestClient, creds: &Creds, symbol: &str, side: Side, requested: f64)
    -> Result<ClosePlan, String> {
    let risk = rest.position_risk(creds).await?;
    let (position_side, amount) = find_position(&risk, symbol, side)?;
    let mark = mark_price_of(&risk, symbol);
    let filters = filters_for(state, rest, symbol).await?;
    plan_close(symbol, amount, position_side, requested, mark, &filters)
}

#[tauri::command]
pub async fn close_preview(app: AppHandle, symbol: String, side: Side, qty: f64) -> Result<ClosePlan, String> {
    let state = app.state::<AppState>();
    let session = state.session();
    let creds = session.creds.ok_or("会话没有凭据")?;
    let rest = app.state::<Arc<RestClient>>().inner().clone();
    prepare(&state, &rest, &creds, &symbol, side, qty).await
}

/// 市价部分平仓. 只减不增; 结果未知时查单一次, 绝不重发.
#[tauri::command]
pub async fn close_position(app: AppHandle, symbol: String, side: Side, qty: f64) -> Result<CloseResult, String> {
    let state = app.state::<AppState>();
    let session = state.session();
    let creds = session.creds.as_ref().ok_or("会话没有凭据")?;
    let rest = app.state::<Arc<RestClient>>().inner().clone();
    let plan = prepare(&state, &rest, creds, &symbol, side, qty).await?;
    let client_id = format!("mini{}", now_ms());
    let result = rest.new_market_order(creds, &order_params(&plan, &client_id)).await;
    let outcome = match result {
        Ok(value) => Ok(value),
        Err(error) if result_unknown(&error) => rest.query_order(creds, &symbol, &client_id).await
            .map_err(|_| format!("下单结果未知({error}), 请到币安核对持仓与订单, 不要重复点击")),
        Err(error) => Err(error),
    };
    // 无论结果如何都刷新一次持仓, 让界面回到真实状态.
    if let Err(error) = crate::engine::refresh_positions(&app, &rest, &session).await {
        eprintln!("[close] 平仓后刷新持仓失败: {error}");
    }
    crate::engine::emit_if_current(&app, session.generation);
    let value = outcome?;
    Ok(CloseResult {
        order_id: value.get("orderId").and_then(Value::as_i64).unwrap_or(0),
        status: value.get("status").and_then(Value::as_str).unwrap_or("UNKNOWN").to_string(),
        filled_qty: num(&value, "executedQty").unwrap_or(0.0),
        avg_price: num(&value, "avgPrice").unwrap_or(0.0),
        qty_text: plan.qty_text.clone(),
    })
}

#[cfg(test)]
mod tests;
