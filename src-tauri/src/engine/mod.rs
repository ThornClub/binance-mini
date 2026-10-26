pub mod alerts;
pub mod tick;
pub mod supplement;

use crate::binance::RestClient;
use crate::state::{AccountSession, AccountView, AppState, Inner, LiqRuntime, MarketSubscription, MarketView, PositionView, Side, now_ms};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

pub const EVT_SNAPSHOT: &str = "state://snapshot";
static ORDER_PROBE_DONE: AtomicBool = AtomicBool::new(false);
pub const EVT_LIQ: &str = "state://liq";
pub const LIQ_COOLDOWN_MS: i64 = 3 * 60 * 1000;
pub fn emit(app: &AppHandle) {
    let snap = app.state::<AppState>().snapshot();
    let _ = app.emit(EVT_SNAPSHOT, snap);
}
pub fn emit_if_current(app: &AppHandle, generation: u64) {
    app.state::<AppState>().effect_if_current(generation, || emit(app));
}
pub fn set_stream_state(app: &AppHandle, generation: u64, ok: bool, err: Option<String>) {
    app.state::<AppState>().write_if_current(generation, |s| {
        s.health.user_stream_ok = ok;
        if ok { s.health.last_error = None; } else if let Some(e) = err { s.health.last_error = Some(e); }
    });
    emit_if_current(app, generation);
}
pub fn set_market_state(app: &AppHandle, sub: &MarketSubscription, ok: bool, err: Option<String>) {
    if app.state::<AppState>().write_if_subscription(sub, |s| {
        s.health.market_stream_ok = ok;
        if let Some(e) = err { s.health.last_error = Some(e); }
        else if ok { s.health.last_error = None; }
    }).is_some() { emit_if_current(app, sub.generation); }
}
pub fn apply_error(state: &AppState, generation: u64, err: Option<String>) -> bool {
    state.write_if_current(generation, |s| s.health.last_error = err).is_some()
}
pub fn set_error(app: &AppHandle, generation: u64, err: Option<String>) {
    let state = app.state::<AppState>();
    if apply_error(&state, generation, err.clone()) {
        state.effect_if_current(generation, || {
            if let Some(msg) = err.as_deref() { eprintln!("[error] {msg}"); }
            emit(app);
        });
    }
}
pub struct LiqIntent {
    pub generation: u64,
    pub symbol: String,
    pub side: Side,
    pub distance: f64,
    pub short: bool,
}
fn collect_liq_risk_inner(s: &mut Inner, generation: u64, now: i64) -> Vec<LiqIntent> {
    let mut fired = Vec::new();
    let warn = s.risk.liq_warn_pct;
    let hysteresis = s.risk.hysteresis_pct;
    for (key, p) in s.positions.iter() {
        let Some(d) = p.liq_distance_pct.filter(|d| d.is_finite()) else { continue };
        if !p.qty.is_finite() || p.qty == 0.0 { continue; }
        let rt = s.liq_rt.entry(key.clone()).or_insert(LiqRuntime { armed: true, last_fired_ms: 0 });
        let (fire, armed) = liq_decision(d, warn, hysteresis, rt.armed, rt.last_fired_ms, now);
        rt.armed = armed;
        if fire {
            rt.last_fired_ms = now;
            fired.push(LiqIntent { generation, symbol: key.0.clone(), side: key.1, distance: d,
                short: p.side == Side::Short || (p.side == Side::Both && p.qty < 0.0) });
        }
    }
    fired
}
pub fn collect_liq_risk(state: &AppState, generation: u64, now: i64) -> Vec<LiqIntent> {
    state.write_if_current(generation, |s| collect_liq_risk_inner(s, generation, now)).unwrap_or_default()
}
pub fn check_liq_risk(app: &AppHandle, generation: u64) {
    notify_liq(app, collect_liq_risk(&app.state::<AppState>(), generation, now_ms()));
}
fn notify_liq(app: &AppHandle, intents: Vec<LiqIntent>) {
    let state = app.state::<AppState>();
    for intent in intents {
        state.effect_if_current(intent.generation, || {
            let dir = if intent.short { "空" } else { "多" };
            let body = format!("{} {dir} 距强平 {:.1}%", intent.symbol, intent.distance);
            eprintln!("[liq] {body}");
            let _ = app.notification().builder().title("强平距离告警").body(&body).show();
            let _ = app.emit(EVT_LIQ, serde_json::json!({ "symbol": intent.symbol, "side": intent.side, "distance": intent.distance, "account_generation": intent.generation }));
        });
    }
}
fn liq_decision(distance: f64, warn: f64, hysteresis: f64, armed: bool, last_fired_ms: i64, now: i64) -> (bool, bool) {
    if distance > warn + hysteresis { return (false, true); }
    if !armed || distance > warn || now - last_fired_ms < LIQ_COOLDOWN_MS { return (false, armed); }
    (true, false)
}
/// 行情只更新派生值, TP/SL和仓位身份仍由原数据源拥有.
fn revalue(p: &mut PositionView, mark: f64) {
    let signed_qty = match p.side { Side::Long => p.qty.abs(), Side::Short => -p.qty.abs(), Side::Both => p.qty };
    p.mark_price = mark;
    p.upnl = (mark - p.entry_price) * signed_qty;
    let initial_margin = p.entry_price * p.qty.abs() / p.leverage;
    p.upnl_pct = if initial_margin > 0.0 { p.upnl / initial_margin * 100.0 } else { 0.0 };
    p.liq_distance_pct = p.liq_price.filter(|l| l.is_finite() && *l > 0.0 && mark.is_finite() && mark > 0.0)
        .map(|l| ((mark - l).abs() / mark) * 100.0);
}
#[allow(clippy::too_many_arguments)]
fn build_view(symbol: &str, side: Side, qty: f64, entry: f64, mark: f64, leverage: f64, liq_price: Option<f64>, margin_ratio: Option<f64>) -> PositionView {
    let mut view = PositionView { symbol: symbol.to_string(), side, qty, entry_price: entry, mark_price: mark,
        upnl: 0.0, upnl_pct: 0.0, leverage: if leverage.is_finite() && leverage > 0.0 { leverage } else { 1.0 },
        liq_price, liq_distance_pct: None, margin_ratio, tp_price: None, sl_price: None };
    revalue(&mut view, mark); view
}
fn finite_num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_str).and_then(|x| x.parse::<f64>().ok()).filter(|x| x.is_finite())
}
fn position_qty(v: &Value, field: &str, side: Side) -> Option<f64> {
    finite_num(v, field).map(|qty| if side == Side::Both { qty } else { qty.abs() })
}
fn keep_position(s: &mut Inner, key: &(String, Side), qty: f64) -> bool {
    if qty == 0.0 { s.positions.remove(key); s.liq_rt.remove(key); return false; }
    if key.1 == Side::Both && s.positions.get(key).is_some_and(|p| p.qty.is_sign_negative() != qty.is_sign_negative()) { s.liq_rt.remove(key); }
    true
}
pub fn apply_user_event(state: &AppState, generation: u64, v: &Value) -> bool {
    match v.get("e").and_then(Value::as_str) {
        Some("ACCOUNT_UPDATE") => {
            if let Some(list) = v.get("a").and_then(|a| a.get("P")).and_then(Value::as_array) { apply_position_updates(state, generation, list); }
            false
        }
        Some("listenKeyExpired") => state.write_if_current(generation, |s| {
            s.health.user_stream_ok = false; s.health.last_error = Some("listenKey 已过期, 正在重建".to_string());
        }).is_some(),
        _ => false,
    }
}
pub fn handle_user_event(app: &AppHandle, generation: u64, v: &Value) -> bool {
    let expired = apply_user_event(&app.state::<AppState>(), generation, v);
    if !expired { check_liq_risk(app, generation); }
    emit_if_current(app, generation); expired
}
fn apply_position_updates(state: &AppState, generation: u64, list: &[Value]) {
    let refresh_risk = state.write_if_current(generation, |s| {
        let mut changed = false;
        for p in list {
            let symbol = p.get("s").and_then(Value::as_str).unwrap_or_default();
            if symbol.is_empty() { continue; }
            let side = Side::from_position_side(p.get("ps").and_then(Value::as_str).unwrap_or("BOTH"));
            let Some(qty) = position_qty(p, "pa", side) else { continue };
            let entry = finite_num(p, "ep").filter(|e| *e > 0.0);
            if qty != 0.0 && entry.is_none() { continue; }
            let key = (symbol.to_string(), side);
            changed |= match s.positions.get(&key) {
                Some(old) => old.qty != qty || (qty != 0.0 && entry != Some(old.entry_price)),
                None => qty != 0.0,
            };
            if !keep_position(s, &key, qty) { continue; }
            let entry = entry.unwrap();
            let mark = s.markets.get(symbol).map(|m| m.mark_price).filter(|m| m.is_finite() && *m > 0.0)
                .or_else(|| s.positions.get(&key).map(|p| p.mark_price).filter(|m| m.is_finite() && *m > 0.0)).unwrap_or(entry);
            let view = s.positions.entry(key).or_insert_with(|| build_view(symbol, side, qty, entry, mark, 1.0, None, None));
            // 私有增量不拥有REST风险元数据, 也不能替它刷新成功时间.
            view.qty = qty; view.entry_price = entry; revalue(view, mark);
        }
        // 仓位变化不一定改变持仓与告警的订阅并集, 不能依靠订阅变化唤醒风险查询.
        if changed { s.position_freshness.error = Some("持仓已变更, 等待风险快照".into()); }
        changed
    }).unwrap_or(false);
    if refresh_risk { state.risk_wake.notify_one(); }
}
#[derive(Default)]
pub struct MarketIntents { pub alerts: Vec<alerts::AlertIntent>, pub liq: Vec<LiqIntent> }
fn notify_market(app: &AppHandle, intents: MarketIntents) { alerts::notify(app, intents.alerts); notify_liq(app, intents.liq); }
fn apply_mark(s: &mut Inner, generation: u64, symbol: &str, price: f64, now: i64) -> MarketIntents {
    let market = s.markets.entry(symbol.to_string()).or_insert_with(|| MarketView { symbol: symbol.to_string(), ..Default::default() });
    market.mark_price = price; market.freshness.success(now);
    for p in s.positions.values_mut().filter(|p| p.symbol == symbol) { revalue(p, price); }
    MarketIntents { alerts: alerts::collect_inner(s, generation, symbol, price, now), liq: collect_liq_risk_inner(s, generation, now) }
}
pub fn handle_market_event(app: &AppHandle, sub: &MarketSubscription, v: &Value) -> bool {
    if let Some(intents) = apply_market_event(&app.state::<AppState>(), sub, v) {
        notify_market(app, intents); emit_if_current(app, sub.generation); true
    } else { false }
}
pub fn apply_market_event(state: &AppState, sub: &MarketSubscription, v: &Value) -> Option<MarketIntents> {
    apply_market_event_at(state, sub, v, now_ms())
}
fn apply_market_event_at(state: &AppState, sub: &MarketSubscription, v: &Value, now: i64) -> Option<MarketIntents> {
    let stream = v.get("stream")?.as_str()?;
    let data = v.get("data")?;
    let symbol = data.get("s")?.as_str()?;
    if !stream.ends_with("@markPrice@1s") || !sub.symbols.iter().any(|s| s == symbol) { return None; }
    let price = finite_num(data, "p").filter(|p| *p > 0.0)?;
    state.write_if_subscription(sub, |s| {
        let intents = apply_mark(s, sub.generation, symbol, price, now);
        let m = s.markets.get_mut(symbol).expect("标记价已创建行情");
        if let Some(funding) = finite_num(data, "r") { m.funding_rate = funding; }
        if let Some(next) = data.get("T").and_then(Value::as_i64) { m.next_funding_at = next; }
        intents
    })
}
fn num(v: &Value, key: &str) -> f64 { finite_num(v, key).unwrap_or(0.0) }
pub async fn refresh_positions(app: &AppHandle, rest: &RestClient, session: &AccountSession) -> Result<(), String> {
    let creds = session.creds.as_ref().ok_or("会话没有凭据")?;
    let result: Result<(), String> = async {
        let v = rest.position_risk(creds).await?;
        apply_positions(&app.state::<AppState>(), session.generation, &v)?;
        check_liq_risk(app, session.generation); Ok(())
    }.await;
    if let Err(ref error) = result { app.state::<AppState>().write_if_current(session.generation, |s| s.position_freshness.error = Some(error.clone())); }
    result
}
pub fn apply_positions(state: &AppState, generation: u64, v: &Value) -> Result<(), String> {
    let arr = v.as_array().ok_or_else(|| "positionRisk 返回非数组".to_string())?;
    state.write_if_current(generation, |s| {
        let mut valid = true;
        for p in arr {
            let symbol = p.get("symbol").and_then(Value::as_str).unwrap_or_default();
            if symbol.is_empty() { valid = false; continue; }
            let side = Side::from_position_side(p.get("positionSide").and_then(Value::as_str).unwrap_or("BOTH"));
            let Some(qty) = position_qty(p, "positionAmt", side) else { valid = false; continue };
            let entry = finite_num(p, "entryPrice").filter(|e| *e > 0.0);
            if qty != 0.0 && entry.is_none() { valid = false; continue; }
            let key = (symbol.to_string(), side);
            if !keep_position(s, &key, qty) { continue; }
            let entry = entry.unwrap();
            let liq = finite_num(p, "liquidationPrice").filter(|v| *v > 0.0);
            let leverage = finite_num(p, "leverage").filter(|v| *v > 0.0).unwrap_or(1.0);
            valid &= finite_num(p, "liquidationPrice").is_some() && finite_num(p, "leverage").is_some();
            let mark = s.markets.get(symbol).map(|m| m.mark_price).filter(|m| m.is_finite() && *m > 0.0)
                .or_else(|| finite_num(p, "markPrice").filter(|m| *m > 0.0)).unwrap_or(entry);
            let mr = s.account.as_ref().map(|a| a.margin_ratio);
            let view = s.positions.entry(key).or_insert_with(|| build_view(symbol, side, qty, entry, mark, leverage, liq, mr));
            // TP/SL保留订单归属, 与阶段2一致.
            view.qty = qty; view.entry_price = entry; view.leverage = leverage; view.liq_price = liq; view.margin_ratio = mr;
            revalue(view, mark);
        }
        if valid { s.position_freshness.success(now_ms()); }
        else { s.position_freshness.error = Some("持仓风险响应包含无效或缺失字段".into()); }
    }).ok_or_else(|| "会话已变更".to_string())
}
pub async fn refresh_account(app: &AppHandle, rest: &RestClient, session: &AccountSession) -> Result<(), String> {
    let creds = session.creds.as_ref().ok_or("会话没有凭据")?;
    let result = async {
        let v = rest.account(creds).await?;
        apply_account(&app.state::<AppState>(), session.generation, &v)
    }.await;
    if let Err(ref error) = result { app.state::<AppState>().write_if_current(session.generation, |s| s.account_freshness.error = Some(error.clone())); }
    result
}
pub fn apply_account(state: &AppState, generation: u64, v: &Value) -> Result<(), String> {
    let getf = |key| finite_num(v, key).ok_or_else(|| format!("账户字段{key}无效"));
    let total_wallet_balance = getf("totalWalletBalance")?;
    let total_unrealized_pnl = getf("totalUnrealizedProfit")?;
    let total_maint_margin = getf("totalMaintMargin")?;
    let available_balance = getf("availableBalance")?;
    let total_margin_balance = getf("totalMarginBalance")?;
    let margin_ratio = if total_margin_balance > 0.0 { total_maint_margin / total_margin_balance * 100.0 } else { 0.0 };
    let account = AccountView { total_wallet_balance, total_unrealized_pnl, total_maint_margin, available_balance, total_margin_balance, margin_ratio };
    state.write_if_current(generation, |s| {
        s.account = Some(account); s.account_freshness.success(now_ms());
        for p in s.positions.values_mut() { p.margin_ratio = Some(margin_ratio); }
    }).ok_or_else(|| "会话已变更".to_string())
}
fn apply_rest_market(state: &AppState, generation: u64, symbol: &str, v: &Value, now: i64) -> Result<MarketIntents, String> {
    let mark = finite_num(v, "markPrice").filter(|m| *m > 0.0).ok_or("标记价无效")?;
    state.write_if_current(generation, |s| {
        let intents = apply_mark(s, generation, symbol, mark, now);
        let market = s.markets.get_mut(symbol).expect("标记价已创建行情");
        if let Some(funding) = finite_num(v, "lastFundingRate") { market.funding_rate = funding; }
        if let Some(next) = v.get("nextFundingTime").and_then(Value::as_i64) { market.next_funding_at = next; }
        intents
    }).ok_or_else(|| "会话已变更".to_string())
}
pub async fn refresh_market(app: &AppHandle, rest: &RestClient, generation: u64, symbols: &[String]) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut last_err = None;
    for symbol in symbols {
        // 排队期间WS可能已恢复, 发请求之前再次判断该symbol而不是全局消息时间.
        if !state.read(|s| s.markets.get(symbol).is_none_or(|m| m.freshness.stale_at(now_ms()))) { continue; }
        let result = match rest.premium_index(symbol).await {
            Ok(v) => apply_rest_market(&state, generation, symbol, &v, now_ms()),
            Err(error) => Err(error),
        };
        match result {
            Ok(intents) => notify_market(app, intents),
            Err(error) => {
                state.write_if_current(generation, |s| {
                    s.markets.entry(symbol.clone()).or_insert_with(|| MarketView { symbol: symbol.clone(), ..Default::default() }).freshness.error = Some(error.clone());
                });
                last_err = Some(error);
            }
        }
    }
    last_err.map_or(Ok(()), Err)
}
pub async fn refresh_ticker(app: &AppHandle, rest: &RestClient, generation: u64, symbols: &[String]) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut last_err = None;
    for symbol in symbols {
        match rest.ticker_24hr(symbol).await {
            Ok(t) => {
                let high = num(&t, "highPrice"); let low = num(&t, "lowPrice");
                let change_pct = num(&t, "priceChangePercent"); let quote_vol = num(&t, "quoteVolume");
                if high <= 0.0 { continue; }
                state.write_if_current(generation, |s| {
                    if let Some(m) = s.markets.get_mut(symbol) { m.high_24h = high; m.low_24h = low; m.change_pct_24h = change_pct; m.quote_volume_24h = quote_vol; }
                }).ok_or("会话已变更")?;
            }
            Err(e) => last_err = Some(e),
        }
    }
    last_err.map_or(Ok(()), Err)
}
pub async fn refresh_orders(app: &AppHandle, rest: &RestClient, session: &AccountSession, symbol: &str) -> Result<(), String> {
    let creds = session.creds.as_ref().ok_or("会话没有凭据")?;
    let generation = session.generation;
    let state = app.state::<AppState>();
    let v = rest.open_orders(symbol, creds).await?;
    let arr = v.as_array().ok_or_else(|| "openOrders 返回非数组".to_string())?;
    apply_orders(&state, generation, symbol, &v)?;
    state.effect_if_current(generation, || {
        let types: Vec<String> = arr.iter().map(|o| o.get("type").and_then(Value::as_str).unwrap_or("?").to_string()).collect();
        eprintln!("[orders] {symbol} 共 {} 单: {:?}", arr.len(), types);
    });
    if arr.is_empty() && state.effect_if_current(generation, || !ORDER_PROBE_DONE.swap(true, Ordering::Relaxed)).unwrap_or(false) {
        match rest.all_open_orders(creds).await {
            Ok(all) => { state.effect_if_current(generation, || { let n = all.as_array().map(Vec::len).unwrap_or(0); eprintln!("[orders] 全量挂单诊断: {n} 条"); }); }
            Err(e) => { state.effect_if_current(generation, || eprintln!("[orders] 全量挂单诊断失败: {e}")); }
        }
        match rest.all_orders(symbol, 10, creds).await {
            Ok(h) => { state.effect_if_current(generation, || {
                let list = h.as_array().cloned().unwrap_or_default();
                let rows: Vec<String> = list.iter().map(|o| format!("{} {:?}/{:?} stop={:?} closePos={:?}",
                    o.get("orderId").and_then(Value::as_i64).unwrap_or(0), o.get("type").and_then(Value::as_str).unwrap_or("?"),
                    o.get("status").and_then(Value::as_str).unwrap_or("?"), o.get("stopPrice").and_then(Value::as_str).unwrap_or("?"),
                    o.get("closePosition").and_then(Value::as_bool).unwrap_or(false))).collect();
                eprintln!("[orders] {symbol} 历史订单 {} 条: {:?}", list.len(), rows);
            }); }
            Err(e) => { state.effect_if_current(generation, || eprintln!("[orders] {symbol} 历史订单查询失败: {e}")); }
        }
    }
    Ok(())
}
fn apply_orders(state: &AppState, generation: u64, symbol: &str, v: &Value) -> Result<(), String> {
    let arr = v.as_array().ok_or("openOrders 返回非数组")?;
    let mut per_side: Vec<(Side, Option<f64>, Option<f64>)> = Vec::new();
    for o in arr {
        let kind = o.get("type").and_then(Value::as_str).ok_or("openOrders 订单类型缺失")?;
        let is_tp = kind.starts_with("TAKE_PROFIT"); let is_sl = kind.starts_with("STOP");
        if !is_tp && !is_sl { continue; }
        let stop = finite_num(o, "stopPrice").filter(|p| *p > 0.0).ok_or("openOrders 触发价无效")?;
        let side = Side::from_position_side(o.get("positionSide").and_then(Value::as_str).unwrap_or("BOTH"));
        let slot = match per_side.iter_mut().find(|(s, _, _)| *s == side) {
            Some(x) => x,
            None => { per_side.push((side, None, None)); per_side.last_mut().expect("刚插入") }
        };
        if is_tp { slot.1 = Some(stop); } else { slot.2 = Some(stop); }
    }
    state.write_if_current(generation, |s| {
        for (key, p) in s.positions.iter_mut() {
            if key.0 != symbol { continue; }
            let got = per_side.iter().find(|(side, _, _)| *side == key.1 || *side == Side::Both);
            p.tp_price = got.and_then(|x| x.1); p.sl_price = got.and_then(|x| x.2);
        }
    }).ok_or_else(|| "会话已变更".to_string())
}
#[cfg(test)]
mod tests;
