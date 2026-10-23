pub mod alerts;
pub mod tick;

use crate::binance::{Creds, RestClient};
use crate::state::{AccountView, AppState, Candle, MarketView, PositionView, Side, now_ms};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

pub const EVT_SNAPSHOT: &str = "state://snapshot";

/// 全量挂单诊断每次启动只跑一次, 避免把权重 40 的请求带进轮询.
static ORDER_PROBE_DONE: AtomicBool = AtomicBool::new(false);

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

/// 按标记价本地重算未实现盈亏. 交易所不会在每次标记价变动时推送盈亏,
/// 不自己算就会一直显示建仓那一刻的数字.
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
}

pub fn handle_market_event(app: &AppHandle, v: &Value) {
    let stream = v.get("stream").and_then(Value::as_str).unwrap_or_default();
    {
        static FIRST_MARKET: std::sync::Once = std::sync::Once::new();
        FIRST_MARKET.call_once(|| {
            eprintln!("[market] 首帧: {}", &stream.chars().take(40).collect::<String>())
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

    let mut tp: Option<f64> = None;
    let mut sl: Option<f64> = None;
    for o in &arr {
        let kind = o.get("type").and_then(Value::as_str).unwrap_or("");
        let stop = o
            .get("stopPrice")
            .and_then(Value::as_str)
            .and_then(|x| x.parse::<f64>().ok())
            .filter(|p| *p > 0.0);
        let Some(stop) = stop else { continue };
        if kind.starts_with("TAKE_PROFIT") {
            tp = Some(stop);
        } else if kind.starts_with("STOP") {
            sl = Some(stop);
        }
    }

    let state = app.state::<AppState>();
    state.write(|s| {
        for (key, p) in s.positions.iter_mut() {
            if key.0 == symbol {
                p.tp_price = tp;
                p.sl_price = sl;
            }
        }
    });
    Ok(())
}
