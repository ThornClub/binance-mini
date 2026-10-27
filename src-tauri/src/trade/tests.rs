use super::*;
use crate::binance::Creds;
use crate::state::Side;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn creds() -> Creds { Creds { key: "fake-key".into(), secret: "FAKE_SECRET".into() } }

fn filters() -> Filters { Filters { step: 0.001, min_notional: 5.0, status: "TRADING".into() } }

fn risk(position_side: &str, amount: &str) -> Value {
    json!([{ "symbol": "ZECUSDT", "positionSide": position_side, "positionAmt": amount,
        "markPrice": "100", "entryPrice": "100", "liquidationPrice": "0", "leverage": "10" }])
}

/// 假交易所: 记录每条请求行, 按路径回放固定响应.
async fn fake_exchange(exchange_info: Value, risk: Value, order: Value)
    -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let line = text.lines().next().unwrap_or_default().to_string();
            let body = if line.contains("/fapi/v1/exchangeInfo") { exchange_info.clone() }
                else if line.contains("/fapi/v2/positionRisk") { risk.clone() }
                else if line.contains("/fapi/v1/order") { order.clone() }
                else { json!({ "serverTime": 1 }) };
            log.lock().unwrap().push(line);
            let payload = serde_json::to_string(&body).unwrap();
            let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                payload.len(), payload);
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (base, seen, server)
}

fn post_lines(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().unwrap().iter().filter(|l| l.starts_with("POST /fapi/v1/order")).cloned().collect()
}

#[test]
fn quantity_rounds_down_to_step_never_up() {
    assert_eq!(round_down_qty(1.2399, 0.001), 1.239);
    assert_eq!(round_down_qty(0.0003, 0.0001), 0.0003);
    assert_eq!(round_down_qty(2.9, 1.0), 2.0);
    assert_eq!(round_down_qty(0.0, 0.001), 0.0);
    assert_eq!(round_down_qty(f64::NAN, 0.001), 0.0);
    assert_eq!(round_down_qty(1.0, 0.0), 0.0);
    // 恰好整数倍时不能因浮点误差放大量.
    assert_eq!(round_down_qty(0.3, 0.1), 0.3);
    assert!(round_down_qty(0.2999999999, 0.1) <= 0.2999999999);
}

#[test]
fn filters_require_market_lot_size_and_min_notional() {
    let v = json!({ "symbols": [{ "symbol": "ZECUSDT", "status": "TRADING", "filters": [
        { "filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1000" },
        { "filterType": "MARKET_LOT_SIZE", "stepSize": "0.01", "minQty": "0.01", "maxQty": "100" },
        { "filterType": "MIN_NOTIONAL", "notional": "5" }
    ] }] });
    let parsed = parse_filters(&v, "ZECUSDT").unwrap();
    assert_eq!(parsed.step, 0.01, "市价单必须优先使用 MARKET_LOT_SIZE");
    assert_eq!(parsed.min_notional, 5.0);
    assert_eq!(parsed.status, "TRADING");

    let missing = json!({ "symbols": [{ "symbol": "ZECUSDT", "status": "TRADING", "filters": [
        { "filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1" }
    ] }] });
    assert!(parse_filters(&missing, "ZECUSDT").is_err(), "缺少名义值时不能猜测");
    assert!(parse_filters(&missing, "OTHERUSDT").is_err(), "找不到合约必须拒绝");
}

#[test]
fn plan_rejects_invalid_requests_before_any_order() {
    let f = filters();
    for (requested, why) in [(0.0, "零"), (f64::NAN, "NaN"), (5.0, "超持仓")] {
        assert!(plan_close("ZECUSDT", 2.0, "BOTH".into(), requested, 100.0, &f).is_err(), "{why}");
    }
    // 低于最小名义值: 0.001 * 100 = 0.1 USDT.
    let error = plan_close("ZECUSDT", 2.0, "BOTH".into(), 0.001, 100.0, &f).unwrap_err();
    assert!(error.contains("名义值"), "{error}");
    // 步长取整为 0.
    assert!(plan_close("ZECUSDT", 2.0, "BOTH".into(), 0.0004, 100.0,
        &Filters { step: 0.001, ..f.clone() }).is_err());
    // 非交易状态.
    assert!(plan_close("ZECUSDT", 2.0, "BOTH".into(), 1.0, 100.0,
        &Filters { status: "SETTLING".into(), ..f.clone() }).is_err());
    // 没有标记价无法校验名义值, 不能放行.
    assert!(plan_close("ZECUSDT", 2.0, "BOTH".into(), 1.0, 0.0, &f).is_err());
}

#[test]
fn find_position_matches_requested_side_only() {
    let both = risk("BOTH", "-2.5");
    assert_eq!(find_position(&both, "ZECUSDT", Side::Both).unwrap(), ("BOTH".into(), -2.5));
    assert!(find_position(&both, "ZECUSDT", Side::Long).is_err(), "单向持仓不能按双向方向平仓");
    let hedge = json!([{ "symbol": "ZECUSDT", "positionSide": "LONG", "positionAmt": "3", "markPrice": "100" }]);
    assert_eq!(find_position(&hedge, "ZECUSDT", Side::Long).unwrap(), ("LONG".into(), 3.0));
    assert!(find_position(&hedge, "ZECUSDT", Side::Short).is_err());
    assert!(find_position(&json!([]), "ZECUSDT", Side::Both).is_err());
}

#[tokio::test]
async fn unidirectional_close_posts_reduce_only_sell_without_position_side() {
    let info = json!({ "symbols": [{ "symbol": "ZECUSDT", "status": "TRADING", "filters": [
        { "filterType": "MARKET_LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1000" },
        { "filterType": "MIN_NOTIONAL", "notional": "5" }] }] });
    let order = json!({ "orderId": 42, "status": "FILLED", "executedQty": "1.000", "avgPrice": "101.5" });
    let (base, seen, server) = fake_exchange(info, risk("BOTH", "2.5"), order).await;
    let rest = RestClient::new().with_test_base(base);
    let state = AppState::new();
    let c = creds();

    let plan = prepare(&state, &rest, &c, "ZECUSDT", Side::Both, 1.0).await.unwrap();
    assert_eq!(plan.order_side, "SELL");
    assert!(plan.reduce_only);
    assert_eq!(plan.qty_text, "1.000");
    assert_eq!(plan.max_qty, 2.5);

    let value = rest.new_market_order(&c, &order_params(&plan, "mini-1")).await.unwrap();
    assert_eq!(value.get("orderId").and_then(Value::as_i64), Some(42));
    let posts = post_lines(&seen);
    assert_eq!(posts.len(), 1, "一次平仓只能发一笔市价单");
    let body = &posts[0];
    assert!(body.contains("side=SELL"), "{body}");
    assert!(body.contains("type=MARKET"), "{body}");
    assert!(body.contains("quantity=1.000"), "{body}");
    assert!(body.contains("reduceOnly=true"), "{body}");
    assert!(!body.contains("positionSide"), "单向模式不能带 positionSide: {body}");
    server.abort();
}

#[tokio::test]
async fn hedge_short_close_posts_buy_with_position_side() {
    let info = json!({ "symbols": [{ "symbol": "ZECUSDT", "status": "TRADING", "filters": [
        { "filterType": "MARKET_LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1000" },
        { "filterType": "MIN_NOTIONAL", "notional": "5" }] }] });
    let order = json!({ "orderId": 7, "status": "NEW", "executedQty": "0", "avgPrice": "0" });
    let (base, seen, server) = fake_exchange(info, risk("SHORT", "-1.2345"), order).await;
    let rest = RestClient::new().with_test_base(base);
    let state = AppState::new();
    let c = creds();

    let plan = prepare(&state, &rest, &c, "ZECUSDT", Side::Short, 1.2345).await.unwrap();
    assert_eq!(plan.order_side, "BUY");
    assert!(!plan.reduce_only);
    assert_eq!(plan.position_side, "SHORT");
    assert_eq!(plan.qty_text, "1.234", "空仓数量必须按步长向下取整");

    rest.new_market_order(&c, &order_params(&plan, "mini-2")).await.unwrap();
    let posts = post_lines(&seen);
    let body = &posts[0];
    assert!(body.contains("side=BUY"), "{body}");
    assert!(body.contains("positionSide=SHORT"), "{body}");
    assert!(!body.contains("reduceOnly"), "双向模式不能带 reduceOnly: {body}");
    server.abort();
}

#[tokio::test]
async fn rejected_previews_never_send_an_order() {
    let info = json!({ "symbols": [{ "symbol": "ZECUSDT", "status": "TRADING", "filters": [
        { "filterType": "MARKET_LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1000" },
        { "filterType": "MIN_NOTIONAL", "notional": "50" }] }] });
    let order = json!({ "orderId": 1, "status": "FILLED", "executedQty": "1", "avgPrice": "100" });
    let (base, seen, server) = fake_exchange(info, risk("BOTH", "2.5"), order).await;
    let rest = RestClient::new().with_test_base(base);
    let state = AppState::new();
    let c = creds();

    // 名义值 100 < 最小 50 之外的情况: 1 单位 = 100 USDT 达标, 这里用 0.1 (10 USDT) 触发拒绝.
    assert!(prepare(&state, &rest, &c, "ZECUSDT", Side::Both, 0.1).await.is_err());
    assert!(prepare(&state, &rest, &c, "ZECUSDT", Side::Both, 9.0).await.is_err(), "超过持仓必须拒绝");
    assert!(prepare(&state, &rest, &c, "ZECUSDT", Side::Short, 1.0).await.is_err(), "方向不符必须拒绝");
    assert!(post_lines(&seen).is_empty(), "被拒绝的预览不能发出任何下单请求");
    server.abort();
}

#[tokio::test]
async fn filters_are_cached_but_never_reused_after_fetch_failure() {
    let info = json!({ "symbols": [{ "symbol": "ZECUSDT", "status": "TRADING", "filters": [
        { "filterType": "MARKET_LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1000" },
        { "filterType": "MIN_NOTIONAL", "notional": "5" }] }] });
    let (base, seen, server) = fake_exchange(info, risk("BOTH", "2.5"), json!({})).await;
    let rest = RestClient::new().with_test_base(base.clone());
    let state = AppState::new();
    let c = creds();
    prepare(&state, &rest, &c, "ZECUSDT", Side::Both, 1.0).await.unwrap();
    let info_calls = seen.lock().unwrap().iter().filter(|l| l.contains("exchangeInfo")).count();
    prepare(&state, &rest, &c, "ZECUSDT", Side::Both, 1.0).await.unwrap();
    let again = seen.lock().unwrap().iter().filter(|l| l.contains("exchangeInfo")).count();
    assert_eq!(info_calls, 1);
    assert_eq!(again, 1, "TTL 内必须复用过滤规则, 不重复打交易所");
    server.abort();

    // 服务端已关闭: 缓存过期后必须报错, 不能悄悄用旧规则.
    let expired = RestClient::new().with_test_base(base);
    let state2 = AppState::new();
    state2.store_filters("ZECUSDT", filters(), 0);
    let fresh_but_stale = state2.cached_filters("ZECUSDT", FILTERS_TTL_MS + 1, FILTERS_TTL_MS);
    assert!(fresh_but_stale.is_none(), "超过 TTL 的规则不能用作下单依据");
    let _ = expired;
}
