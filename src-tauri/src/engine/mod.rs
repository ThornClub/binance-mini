pub mod alerts;
pub mod tick;

use crate::binance::{Creds, RestClient};
use crate::state::{
    AccountView, AppState, Candle, LiqRuntime, MarketView, PositionView, Side, now_ms,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

pub const EVT_SNAPSHOT: &str = "state://snapshot";

/// 全量挂单诊断每次启动只跑一次, 避免把权重 40 的请求带进轮询.
static ORDER_PROBE_DONE: AtomicBool = AtomicBool::new(false);

pub const EVT_LIQ: &str = "state://liq";
/// 同一仓位告警后的最短间隔, 和迟滞一起防轰炸.
pub const LIQ_COOLDOWN_MS: i64 = 3 * 60 * 1000;

pub fn emit(app: &AppHandle) {
    let state = app.state::<AppState>();
    let snap = state.snapshot();
    let _ = app.emit(EVT_SNAPSHOT, snap);
}

pub fn set_stream_state(app: &AppHandle, ok: bool, err: Option<String>) {
    let state = app.state::<AppState>();
    state.write(|s| {
        s.health.user_stream_ok = ok;
        if ok {
            s.health.last_msg_ms = now_ms();
            s.health.last_error = None;
        } else if let Some(e) = err {
            s.health.last_error = Some(e);
        }
    });
    emit(app);
}

pub fn set_market_state(app: &AppHandle, ok: bool, err: Option<String>) {
    let state = app.state::<AppState>();
    state.write(|s| {
        s.health.market_stream_ok = ok;
        if let Some(e) = err {
            s.health.last_error = Some(e);
        } else if ok {
            // 行情流恢复正常时把旧错误清掉, 否则界面会一直挂着上一次的断流提示.
            s.health.last_error = None;
        }
    });
    emit(app);
}

pub fn set_error(app: &AppHandle, err: Option<String>) {
    // 出错时同时写 stderr: 从终端启动时能直接看到网络或代理失败原因.
    if let Some(msg) = err.as_deref() {
        eprintln!("[error] {msg}");
    }
    let state = app.state::<AppState>();
    state.write(|s| s.health.last_error = err);
    emit(app);
}

/// REST 兜底成功时打点, 让"陈旧"判定不只看 WebSocket 推送.
pub fn touch_rest(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.write(|s| s.health.last_rest_ms = now_ms());
}

pub fn touch_market(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.write(|s| {
        let t = now_ms();
        s.health.last_msg_ms = t;
        s.health.last_market_msg_ms = t;
    });
}

pub fn market_idle_ms(app: &AppHandle) -> i64 {
    let state = app.state::<AppState>();
    let last = state.read(|s| s.health.last_market_msg_ms);
    now_ms() - last
}

/// 检查各仓位距强平的距离, 跌破阈值就通知 + 响声.
///
/// 判定用"从安全区跌进阈值"而不是"当前低于阈值": 价格贴着阈值来回磨时,
/// 后者会每轮都弹一次. 距离回升到 warn + hysteresis 以上才重新武装.
pub fn check_liq_risk(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut fired: Vec<(String, Side, f64)> = Vec::new();

    state.write(|s| {
        let warn = s.risk.liq_warn_pct;
        let hysteresis = s.risk.hysteresis_pct;
        let now = now_ms();
        for (key, p) in s.positions.iter() {
            let Some(d) = p.liq_distance_pct else { continue };
            let rt = s.liq_rt.entry(key.clone()).or_insert(LiqRuntime {
                armed: true,
                last_fired_ms: 0,
            });
            let (fire, armed) = liq_decision(d, warn, hysteresis, rt.armed, rt.last_fired_ms, now);
            rt.armed = armed;
            if fire {
                rt.last_fired_ms = now;
                fired.push((key.0.clone(), key.1, d));
            }
        }
    });

    for (symbol, side, distance) in fired {
        let dir = match side {
            Side::Long => "多",
            Side::Short => "空",
            Side::Both => "",
        };
        let body = format!("{symbol} {dir} 距强平 {distance:.1}%");
        eprintln!("[liq] {body}");
        let _ = app
            .notification()
            .builder()
            .title("强平距离告警")
            .body(&body)
            .show();
        let _ = app.emit(
            EVT_LIQ,
            serde_json::json!({ "symbol": symbol, "side": side, "distance": distance }),
        );
    }
}

/// 强平告警的纯判定, 从 IO 里拆出来单独测.
///
/// 返回 (是否触发, 触发后是否应处于武装状态):
/// - 距离回到 warn + hysteresis 以上才重新武装, 否则价格贴着阈值会反复弹
/// - 冷却期内的重复跌破直接吞掉
fn liq_decision(
    distance: f64,
    warn: f64,
    hysteresis: f64,
    armed: bool,
    last_fired_ms: i64,
    now: i64,
) -> (bool, bool) {
    if distance > warn + hysteresis {
        return (false, true);
    }
    if !armed || distance > warn || now - last_fired_ms < LIQ_COOLDOWN_MS {
        return (false, armed);
    }
    (true, false)
}

/// 按标记价本地重算未实现盈亏. 交易所不会在每次标记价变动时推送盈亏,
/// 不自己算就会一直显示建仓那一刻的数字.
// 行情快照字段多, 拆结构体反而要在三处调用点重复组装.
#[allow(clippy::too_many_arguments)]
fn build_view(
    symbol: &str,
    side: Side,
    qty: f64,
    entry: f64,
    mark: f64,
    leverage: f64,
    liq_price: Option<f64>,
    margin_ratio: Option<f64>,
) -> PositionView {
    let dir = match side {
        Side::Long | Side::Both => 1.0,
        Side::Short => -1.0,
    };
    let upnl = (mark - entry) * qty * dir;
    let lev = if leverage > 0.0 { leverage } else { 1.0 };
    let initial_margin = entry * qty / lev;
    let upnl_pct = if initial_margin > 0.0 {
        upnl / initial_margin * 100.0
    } else {
        0.0
    };
    let liq_distance_pct = liq_price
        .filter(|l| *l > 0.0 && mark > 0.0)
        .map(|l| ((mark - l).abs() / mark) * 100.0);
    PositionView {
        symbol: symbol.to_string(),
        side,
        qty,
        entry_price: entry,
        mark_price: mark,
        upnl,
        upnl_pct,
        leverage: lev,
        liq_price,
        liq_distance_pct,
        margin_ratio,
        tp_price: None,
        sl_price: None,
    }
}

pub fn handle_user_event(app: &AppHandle, v: &Value) {
    match v.get("e").and_then(Value::as_str) {
        Some("ACCOUNT_UPDATE") => {
            if let Some(list) = v
                .get("a")
                .and_then(|a| a.get("P"))
                .and_then(Value::as_array)
            {
                apply_position_updates(app, list);
            }
        }
        Some("listenKeyExpired") => {
            set_error(app, Some("listenKey 已过期, 正在重建".to_string()));
        }
        _ => {}
    }
}

fn apply_position_updates(app: &AppHandle, list: &[Value]) {
    let state = app.state::<AppState>();
    let mut changed = false;
    state.write(|s| {
        for p in list {
            let symbol = p
                .get("s")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if symbol.is_empty() {
                continue;
            }
            let side =
                Side::from_position_side(p.get("ps").and_then(Value::as_str).unwrap_or("BOTH"));
            let qty = p
                .get("pa")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
                .abs();
            let entry = p
                .get("ep")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            let key = (symbol.clone(), side);

            if qty <= 0.0 {
                s.positions.remove(&key);
                changed = true;
                continue;
            }

            let mark = s
                .markets
                .get(&symbol)
                .map(|m| m.mark_price)
                .filter(|m| *m > 0.0)
                .unwrap_or(entry);
            let prev = s.positions.get(&key);
            let leverage = prev.map(|x| x.leverage).unwrap_or(1.0);
            let liq = prev.and_then(|x| x.liq_price);
            let mr = prev.and_then(|x| x.margin_ratio);
            s.positions.insert(
                key,
                build_view(&symbol, side, qty, entry, mark, leverage, liq, mr),
            );
            changed = true;
        }
    });
    if changed {
        state.recompute_symbols();
        emit(app);
    }
    // 私有流推来的仓位没带强平价, 但强平距离会随标记价变化, 每帧都要重新判一次.
    check_liq_risk(app);
}

pub fn handle_market_event(app: &AppHandle, v: &Value) {
    let stream = v.get("stream").and_then(Value::as_str).unwrap_or_default();
    {
        static FIRST_MARKET: std::sync::Once = std::sync::Once::new();
        FIRST_MARKET.call_once(|| {
            eprintln!("[market] 首帧: {}", stream.chars().take(40).collect::<String>())
        });
    }
    let Some(data) = v.get("data") else { return };

    if stream.contains("markPrice") {
        let symbol = data
            .get("s")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if symbol.is_empty() {
            return;
        }
        let price = data
            .get("p")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let funding = data
            .get("r")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let next_funding = data.get("T").and_then(Value::as_i64).unwrap_or(0);

        let state = app.state::<AppState>();
        state.write(|s| {
            let m = s
                .markets
                .entry(symbol.clone())
                .or_insert_with(|| MarketView {
                    symbol: symbol.clone(),
                    ..Default::default()
                });
            m.mark_price = price;
            m.funding_rate = funding;
            m.next_funding_at = next_funding;

            // 标记价变了, 该合约所有方向的盈亏都要重算.
            let keys: Vec<(String, Side)> = s
                .positions
                .keys()
                .filter(|(sym, _)| *sym == symbol)
                .cloned()
                .collect();
            for key in keys {
                if let Some(p) = s.positions.get(&key) {
                    let mut updated = build_view(
                        &p.symbol,
                        p.side,
                        p.qty,
                        p.entry_price,
                        price,
                        p.leverage,
                        p.liq_price,
                        p.margin_ratio,
                    );
                    updated.tp_price = p.tp_price;
                    updated.sl_price = p.sl_price;
                    s.positions.insert(key, updated);
                }
            }
        });

        alerts::evaluate(app, &symbol, price);
        emit(app);
        return;
    }

    if stream.contains("kline") {
        let symbol = data
            .get("s")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(k) = data.get("k") else { return };
        let candle = Candle {
            t: k.get("t").and_then(Value::as_i64).unwrap_or(0),
            o: num(k, "o"),
            h: num(k, "h"),
            l: num(k, "l"),
            c: num(k, "c"),
        };
        let state = app.state::<AppState>();
        state.write(|s| {
            if let Some(m) = s.markets.get_mut(&symbol) {
                m.last_candle = Some(candle);
            }
        });
        emit(app);
    }
}

fn num(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(|x| x.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// 用 REST 全量刷新持仓. positionRisk 是唯一带强平价的接口, 必须靠它.
pub async fn refresh_positions(
    app: &AppHandle,
    rest: &RestClient,
    creds: &Creds,
) -> Result<(), String> {
    let v = rest.position_risk(creds).await?;
    let arr = v
        .as_array()
        .ok_or_else(|| "positionRisk 返回非数组".to_string())?;
    let state = app.state::<AppState>();
    let mut changed = false;
    state.write(|s| {
        for p in arr {
            let symbol = p
                .get("symbol")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if symbol.is_empty() {
                continue;
            }
            let side = Side::from_position_side(
                p.get("positionSide")
                    .and_then(Value::as_str)
                    .unwrap_or("BOTH"),
            );
            let qty = p
                .get("positionAmt")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
                .abs();
            let key = (symbol.clone(), side);
            if qty <= 0.0 {
                if s.positions.remove(&key).is_some() {
                    changed = true;
                }
                continue;
            }
            let entry = p
                .get("entryPrice")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            let liq = p
                .get("liquidationPrice")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| *v > 0.0);
            let leverage = p
                .get("leverage")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0);
            let mark = s
                .markets
                .get(&symbol)
                .map(|m| m.mark_price)
                .filter(|m| *m > 0.0)
                .unwrap_or_else(|| {
                    p.get("markPrice")
                        .and_then(Value::as_str)
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(entry)
                });
            let mr = s.account.as_ref().map(|a| a.margin_ratio);
            let mut view = build_view(&symbol, side, qty, entry, mark, leverage, liq, mr);
            // 止盈止损价只能从挂单里读到, 1 秒一轮的 positionRisk 不能把它们覆盖成空.
            if let Some(old) = s.positions.get(&key) {
                view.tp_price = old.tp_price;
                view.sl_price = old.sl_price;
            }
            s.positions.insert(key, view);
            changed = true;
        }
    });
    if changed {
        state.recompute_symbols();
    }
    check_liq_risk(app);
    Ok(())
}

pub async fn refresh_account(
    app: &AppHandle,
    rest: &RestClient,
    creds: &Creds,
) -> Result<(), String> {
    let v = rest.account(creds).await?;
    let getf = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .and_then(|x| x.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let total_wallet_balance = getf("totalWalletBalance");
    let total_unrealized_pnl = getf("totalUnrealizedProfit");
    let total_maint_margin = getf("totalMaintMargin");
    let available_balance = getf("availableBalance");
    let total_margin_balance = getf("totalMarginBalance");
    let margin_ratio = if total_margin_balance > 0.0 {
        total_maint_margin / total_margin_balance * 100.0
    } else {
        0.0
    };
    let account = AccountView {
        total_wallet_balance,
        total_unrealized_pnl,
        total_maint_margin,
        available_balance,
        total_margin_balance,
        margin_ratio,
    };
    let state = app.state::<AppState>();
    state.write(|s| s.account = Some(account));
    // 保证金率变化会影响持仓行显示, 同步写回.
    let mr = margin_ratio;
    state.write(|s| {
        let updates: Vec<((String, Side), PositionView)> = s
            .positions
            .iter()
            .map(|(k, p)| {
                let mut p2 = p.clone();
                p2.margin_ratio = Some(mr);
                (k.clone(), p2)
            })
            .collect();
        for (k, p) in updates {
            s.positions.insert(k, p);
        }
    });
    Ok(())
}

/// 行情流拿不到数据时(网络被拦)用 REST 兜底: 标记价, 资金费率, 下次结算时间, 24 小时统计.
/// 只写 markets 里已有订阅的合约, 不主动扩张订阅集.
pub async fn refresh_market(app: &AppHandle, rest: &RestClient, symbols: &[String]) -> Result<(), String> {
    if symbols.is_empty() {
        return Ok(());
    }
    let state = app.state::<AppState>();
    let mut last_err = None;
    for symbol in symbols {
        match rest.premium_index(symbol).await {
            Ok(v) => {
                let mark = v
                    .get("markPrice")
                    .and_then(Value::as_str)
                    .and_then(|x| x.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let funding = v
                    .get("lastFundingRate")
                    .and_then(Value::as_str)
                    .and_then(|x| x.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let next_funding = v.get("nextFundingTime").and_then(Value::as_i64).unwrap_or(0);
                state.write(|s| {
                    let m = s
                        .markets
                        .entry(symbol.clone())
                        .or_insert_with(|| MarketView {
                            symbol: symbol.clone(),
                            ..Default::default()
                        });
                    if mark > 0.0 {
                        m.mark_price = mark;
                    }
                    m.funding_rate = funding;
                    m.next_funding_at = next_funding;
                    // 标记价变了要重算该合约的浮动盈亏, 否则数字会停在 WS 断开那一刻.
                    let keys: Vec<(String, Side)> = s
                        .positions
                        .keys()
                        .filter(|(sym, _)| sym == symbol)
                        .cloned()
                        .collect();
                    for key in keys {
                        if let Some(p) = s.positions.get(&key) {
                            let updated = build_view(
                                &p.symbol,
                                p.side,
                                p.qty,
                                p.entry_price,
                                if mark > 0.0 { mark } else { p.mark_price },
                                p.leverage,
                                p.liq_price,
                                p.margin_ratio,
                            );
                            s.positions.insert(key, updated);
                        }
                    }
                });
                // 推送被掐断时告警只能靠 REST 的标记价触发, 否则价格告警永远不会响.
                if mark > 0.0 {
                    alerts::evaluate(app, symbol, mark);
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// 行情流不可用时用 REST 拉最近两根 1 分钟 K 线, 让迷你图最后一根跟着走.
/// 只刷前端正在看的那一个合约, 避免每轮的请求量随仓位数量线性增长.
pub async fn refresh_last_candle(app: &AppHandle, rest: &RestClient, symbol: &str) -> Result<(), String> {
    let v = rest.klines(symbol, "1m", 2).await?;
    let Some(row) = v.as_array().and_then(|a| a.last()) else {
        return Ok(());
    };
    let Some(cols) = row.as_array() else {
        return Ok(());
    };
    let num = |i: usize| {
        cols.get(i)
            .and_then(|x| x.as_str())
            .and_then(|x| x.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let candle = Candle {
        t: cols.first().and_then(Value::as_i64).unwrap_or(0),
        o: num(1),
        h: num(2),
        l: num(3),
        c: num(4),
    };
    let state = app.state::<AppState>();
    state.write(|s| {
        if let Some(m) = s.markets.get_mut(symbol) {
            m.last_candle = Some(candle);
        }
    });
    Ok(())
}

/// 24 小时统计刷新节奏比标记价慢: 每 10 轮(约 10 秒)一次, 省权重.
pub async fn refresh_ticker(app: &AppHandle, rest: &RestClient, symbols: &[String]) -> Result<(), String> {
    if symbols.is_empty() {
        return Ok(());
    }
    let state = app.state::<AppState>();
    let mut last_err = None;
    for symbol in symbols {
        match rest.ticker_24hr(symbol).await {
            Ok(t) => {
                let high = num(&t, "highPrice");
                let low = num(&t, "lowPrice");
                let change_pct = num(&t, "priceChangePercent");
                let quote_vol = num(&t, "quoteVolume");
                if high <= 0.0 {
                    continue;
                }
                state.write(|s| {
                    if let Some(m) = s.markets.get_mut(symbol) {
                        m.high_24h = high;
                        m.low_24h = low;
                        m.change_pct_24h = change_pct;
                        m.quote_volume_24h = quote_vol;
                    }
                });
            }
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// 从当前挂单里解析止盈/止损触发价, 写回对应持仓, 供图表画线.
pub async fn refresh_orders(
    app: &AppHandle,
    rest: &RestClient,
    creds: &Creds,
    symbol: &str,
) -> Result<(), String> {
    let v = rest.open_orders(symbol, creds).await?;
    // 非数组说明拿到的是错误体, 必须报出去, 不能当成"没有挂单".
    let arr = v
        .as_array()
        .cloned()
        .ok_or_else(|| "openOrders 返回非数组".to_string())?;
    {
        let types: Vec<String> = arr
            .iter()
            .map(|o| o.get("type").and_then(Value::as_str).unwrap_or("?").to_string())
            .collect();
        eprintln!("[orders] {symbol} 共 {} 单: {:?}", arr.len(), types);
    }

    // 启动后第一次查到空列表时做一轮诊断 (各跑一次, 不进轮询):
    // 分不清"这个合约没挂单"还是"挂单在别处"时, 这两条日志是唯一证据.
    if arr.is_empty() && !ORDER_PROBE_DONE.swap(true, Ordering::Relaxed) {
        match rest.all_open_orders(creds).await {
            Ok(all) => {
                let n = all.as_array().map(Vec::len).unwrap_or(0);
                eprintln!("[orders] 全量挂单诊断: {n} 条");
            }
            Err(e) => eprintln!("[orders] 全量挂单诊断失败: {e}"),
        }
        // 历史订单一并看一眼: 有条件委托但已是 CANCELED/FILLED 时, 这里能看出来.

        match rest.all_orders(symbol, 10, creds).await {
            Ok(h) => {
                let list = h.as_array().cloned().unwrap_or_default();
                let rows: Vec<String> = list
                    .iter()
                    .map(|o| {
                        format!(
                            "{} {:?}/{:?} stop={:?} closePos={:?}",
                            o.get("orderId").and_then(Value::as_i64).unwrap_or(0),
                            o.get("type").and_then(Value::as_str).unwrap_or("?"),
                            o.get("status").and_then(Value::as_str).unwrap_or("?"),
                            o.get("stopPrice").and_then(Value::as_str).unwrap_or("?"),
                            o.get("closePosition").and_then(Value::as_bool).unwrap_or(false),
                        )
                    })
                    .collect();
                eprintln!("[orders] {symbol} 历史订单 {} 条: {:?}", list.len(), rows);
            }
            Err(e) => eprintln!("[orders] {symbol} 历史订单查询失败: {e}"),
        }
    }

    // 按订单的 positionSide 归位: 双向持仓下同一合约的 LONG 与 SHORT 是两条独立仓位,
    // 不区分就会把多单的止损写到空单那一行上.
    let mut per_side: Vec<(Side, Option<f64>, Option<f64>)> = Vec::new();
    for o in &arr {
        let kind = o.get("type").and_then(Value::as_str).unwrap_or("");
        let stop = o
            .get("stopPrice")
            .and_then(Value::as_str)
            .and_then(|x| x.parse::<f64>().ok())
            .filter(|p| *p > 0.0);
        let Some(stop) = stop else { continue };
        let is_tp = kind.starts_with("TAKE_PROFIT");
        let is_sl = kind.starts_with("STOP");
        if !is_tp && !is_sl {
            continue;
        }
        let side = Side::from_position_side(
            o.get("positionSide")
                .and_then(Value::as_str)
                .unwrap_or("BOTH"),
        );
        let slot = match per_side.iter_mut().find(|(s, _, _)| *s == side) {
            Some(x) => x,
            None => {
                per_side.push((side, None, None));
                per_side.last_mut().expect("刚插入")
            }
        };
        if is_tp {
            slot.1 = Some(stop);
        } else {
            slot.2 = Some(stop);
        }
    }

    let state = app.state::<AppState>();
    state.write(|s| {
        for (key, p) in s.positions.iter_mut() {
            if key.0 != symbol {
                continue;
            }
            // 单向模式 (BOTH) 的挂单不区分方向, 直接套到该合约唯一的那条仓位上.
            let got = per_side
                .iter()
                .find(|(side, _, _)| *side == key.1 || *side == Side::Both);
            p.tp_price = got.and_then(|x| x.1);
            p.sl_price = got.and_then(|x| x.2);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liq_distance_is_measured_from_mark_price() {
        // 做多, 标记 100, 强平 80 -> 距离 20%
        let p = build_view("BTCUSDT", Side::Long, 1.0, 100.0, 100.0, 10.0, Some(80.0), None);
        assert!((p.liq_distance_pct.unwrap() - 20.0).abs() < 1e-9);
        // 做空时强平价在上方, 距离同样取绝对值
        let s = build_view("BTCUSDT", Side::Short, 1.0, 100.0, 100.0, 10.0, Some(120.0), None);
        assert!((s.liq_distance_pct.unwrap() - 20.0).abs() < 1e-9);
        // 没有强平价时不给距离, 前端显示 "--"
        let none = build_view("BTCUSDT", Side::Long, 1.0, 100.0, 100.0, 10.0, None, None);
        assert!(none.liq_distance_pct.is_none());
    }

    #[test]
    fn upnl_follows_side_and_leverage() {
        // 多: (110-100)*2 = 20; 保证金 100*2/10 = 20 -> 收益率 100%
        let long = build_view("X", Side::Long, 2.0, 100.0, 110.0, 10.0, None, None);
        assert!((long.upnl - 20.0).abs() < 1e-9);
        assert!((long.upnl_pct - 100.0).abs() < 1e-9);
        // 空: 方向相反
        let short = build_view("X", Side::Short, 2.0, 100.0, 110.0, 10.0, None, None);
        assert!((short.upnl + 20.0).abs() < 1e-9);
    }

    #[test]
    fn liq_alert_fires_once_per_breach() {
        let now = 1_000_000;
        // 首次跌破 10% -> 触发, 并解除武装
        let (fire, armed) = liq_decision(9.0, 10.0, 2.0, true, 0, now);
        assert!(fire && !armed);
        // 仍在阈值内且未武装 -> 不重复触发
        let (fire, armed) = liq_decision(8.0, 10.0, 2.0, false, now, now + 1000);
        assert!(!fire && !armed);
    }

    #[test]
    fn liq_alert_rearms_only_above_hysteresis() {
        // 回到 11% 还在迟滞带里, 不重新武装
        let (fire, armed) = liq_decision(11.0, 10.0, 2.0, false, 0, 1);
        assert!(!fire && !armed);
        // 回到 12.1% 以上才重新武装
        let (_, armed) = liq_decision(12.1, 10.0, 2.0, false, 0, 1);
        assert!(armed);
        // 重新武装后再跌破会再次触发 (冷却已过)
        let (fire, _) = liq_decision(9.5, 10.0, 2.0, true, 0, LIQ_COOLDOWN_MS + 1);
        assert!(fire);
    }

    #[test]
    fn liq_alert_respects_cooldown() {
        let now = 10_000_000;
        // 刚报过, 冷却期内再跌破不重复弹
        let (fire, _) = liq_decision(9.0, 10.0, 2.0, true, now, now + LIQ_COOLDOWN_MS - 1);
        assert!(!fire);
        // 冷却结束后允许再次触发
        let (fire, _) = liq_decision(9.0, 10.0, 2.0, true, now, now + LIQ_COOLDOWN_MS);
        assert!(fire);
    }
}
