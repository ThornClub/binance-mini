use super::*;
use crate::binance::Creds;

fn login(state: &AppState, name: &str) -> AccountSession {
    state.set_creds(Some(Creds { key: name.into(), secret: "fake".into() })); state.session()
}
fn account(wallet: &str) -> Value {
    serde_json::json!({"totalWalletBalance":wallet,"totalUnrealizedProfit":"0","totalMaintMargin":"1","availableBalance":"100","totalMarginBalance":"100"})
}
#[test]
fn liq_distance_is_measured_from_mark_price() {
    let p = build_view("BTCUSDT", Side::Long, 1.0, 100.0, 100.0, 10.0, Some(80.0), None);
    assert!((p.liq_distance_pct.unwrap() - 20.0).abs() < 1e-9);
    let s = build_view("BTCUSDT", Side::Short, 1.0, 100.0, 100.0, 10.0, Some(120.0), None);
    assert!((s.liq_distance_pct.unwrap() - 20.0).abs() < 1e-9);
    let none = build_view("BTCUSDT", Side::Long, 1.0, 100.0, 100.0, 10.0, None, None);
    assert!(none.liq_distance_pct.is_none());
}
#[test]
fn upnl_follows_side_and_leverage() {
    let long = build_view("X", Side::Long, 2.0, 100.0, 110.0, 10.0, None, None);
    assert!((long.upnl - 20.0).abs() < 1e-9); assert!((long.upnl_pct - 100.0).abs() < 1e-9);
    let short = build_view("X", Side::Short, 2.0, 100.0, 110.0, 10.0, None, None);
    assert!((short.upnl + 20.0).abs() < 1e-9);
}
#[test]
fn liq_alert_fires_once_per_breach() {
    let now = 1_000_000;
    let (fire, armed) = liq_decision(9.0, 10.0, 2.0, true, 0, now); assert!(fire && !armed);
    let (fire, armed) = liq_decision(8.0, 10.0, 2.0, false, now, now + 1000); assert!(!fire && !armed);
}
#[test]
fn liq_alert_rearms_only_above_hysteresis() {
    let (fire, armed) = liq_decision(11.0, 10.0, 2.0, false, 0, 1); assert!(!fire && !armed);
    let (_, armed) = liq_decision(12.1, 10.0, 2.0, false, 0, 1); assert!(armed);
    let (fire, _) = liq_decision(9.5, 10.0, 2.0, true, 0, LIQ_COOLDOWN_MS + 1); assert!(fire);
}
#[test]
fn liq_alert_respects_cooldown() {
    let now = 10_000_000;
    let (fire, _) = liq_decision(9.0, 10.0, 2.0, true, now, now + LIQ_COOLDOWN_MS - 1); assert!(!fire);
    let (fire, _) = liq_decision(9.0, 10.0, 2.0, true, now, now + LIQ_COOLDOWN_MS); assert!(fire);
}
#[test]
fn actual_rest_ws_error_and_risk_apply_paths_reject_late_a() {
    let state = AppState::new(); let a = login(&state, "A");
    let positions = serde_json::json!([{"symbol":"X","positionSide":"LONG","positionAmt":"1","entryPrice":"100","markPrice":"100","liquidationPrice":"95"}]);
    apply_positions(&state, a.generation, &positions).unwrap();
    let sub = state.subscribe_symbols().borrow().clone();
    let intents = collect_liq_risk(&state, a.generation, 1_000_000); assert_eq!(intents.len(), 1);
    state.set_creds(None); let b = login(&state, "B");
    apply_account(&state, b.generation, &account("222")).unwrap();
    let before = serde_json::to_value(state.snapshot()).unwrap();
    assert!(apply_positions(&state, a.generation, &positions).is_err());
    assert!(apply_account(&state, a.generation, &account("999")).is_err());
    apply_user_event(&state, a.generation, &serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"LONG","pa":"1","ep":"100"}]}}));
    assert!(apply_market_event(&state, &sub, &serde_json::json!({"stream":"x@markPrice@1s","data":{"s":"X","p":"999"}})).is_none());
    assert!(!apply_error(&state, a.generation, Some("迟到错误".into())));
    assert!(collect_liq_risk(&state, a.generation, 2_000_000).is_empty());
    assert!(state.effect_if_current(intents[0].generation, || panic!("迟到强平通知")).is_none());
    assert_eq!(serde_json::to_value(state.snapshot()).unwrap(), before);
    assert!(state.subscribe_symbols().borrow().symbols.is_empty());
}
#[test]
fn expired_event_marks_stream_invalid_and_requests_immediate_rebuild() {
    let state = AppState::new(); let a = login(&state, "A");
    state.write_if_current(a.generation, |s| s.health.user_stream_ok = true);
    let event = serde_json::json!({"e":"listenKeyExpired","listenKey":"FAKE_LISTEN_KEY"});
    assert!(apply_user_event(&state, a.generation, &event));
    assert!(!state.snapshot().health.user_stream_ok);
    assert!(!serde_json::to_string(&state.snapshot()).unwrap().contains("FAKE_LISTEN_KEY"));
    login(&state, "B"); assert!(!apply_user_event(&state, a.generation, &event));
    assert!(state.snapshot().health.last_error.is_none());
}
#[test]
fn both_short_rest_keeps_sign_profit_and_subscription() {
    let state = AppState::new(); let session = login(&state, "A");
    apply_positions(&state, session.generation, &serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":"-2","entryPrice":"100","markPrice":"90","leverage":"10"}])).unwrap();
    let p = state.snapshot().positions.remove(0);
    assert_eq!(p.upnl, 20.0); assert_eq!(p.upnl_pct, 100.0); assert_eq!(p.qty, -2.0); assert_eq!(p.side, Side::Both);
    assert_eq!(state.subscribe_symbols().borrow().symbols, ["X"]);
}
#[test]
fn account_update_preserves_order_owned_tp_sl() {
    let state = AppState::new(); let session = login(&state, "A");
    apply_positions(&state, session.generation, &serde_json::json!([{"symbol":"X","positionSide":"LONG","positionAmt":"1","entryPrice":"100","markPrice":"100","leverage":"10","liquidationPrice":"80"}])).unwrap();
    state.write(|s| { let p = s.positions.get_mut(&("X".into(), Side::Long)).unwrap(); p.tp_price = Some(120.0); p.sl_price = Some(90.0); });
    apply_user_event(&state, session.generation, &serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"LONG","pa":"2","ep":"105"}]}}));
    let p = state.snapshot().positions.remove(0);
    assert_eq!((p.tp_price, p.sl_price), (Some(120.0), Some(90.0)));
    assert_eq!((p.qty, p.entry_price, p.leverage, p.liq_price), (2.0, 105.0, 10.0, Some(80.0)));
}
#[test]
fn close_then_reopen_does_not_inherit_liquidation_cooldown() {
    let state = AppState::new(); let session = login(&state, "A");
    let position = serde_json::json!([{"symbol":"X","positionSide":"LONG","positionAmt":"1","entryPrice":"100","markPrice":"100","liquidationPrice":"95"}]);
    apply_positions(&state, session.generation, &position).unwrap();
    assert_eq!(collect_liq_risk(&state, session.generation, 1_000_000).len(), 1);
    apply_user_event(&state, session.generation, &serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"LONG","pa":"0","ep":"0"}]}}));
    assert!(state.snapshot().positions.is_empty());
    apply_positions(&state, session.generation, &position).unwrap();
    assert_eq!(collect_liq_risk(&state, session.generation, 1_000_001).len(), 1);
}
#[test]
fn rest_and_private_updates_cover_all_four_position_directions() {
    for (side, raw_qty, expected_qty, expected_pnl) in [("BOTH","2",2.0,-20.0),("BOTH","-2",-2.0,20.0),("LONG","2",2.0,-20.0),("SHORT","-2",2.0,20.0)] {
        let state = AppState::new(); let session = login(&state, "A");
        let row = serde_json::json!([{"symbol":"X","positionSide":side,"positionAmt":raw_qty,"entryPrice":"100","markPrice":"90","leverage":"10"}]);
        apply_positions(&state, session.generation, &row).unwrap(); let p = state.snapshot().positions.remove(0);
        assert_eq!((p.qty,p.upnl,p.upnl_pct),(expected_qty,expected_pnl,expected_pnl*5.0));
        let event = serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":side,"pa":raw_qty,"ep":"100"}]}});
        apply_user_event(&state, session.generation, &event); assert_eq!(state.snapshot().positions[0].mark_price,90.0);
        apply_rest_market(&state, session.generation,"X",&serde_json::json!({"markPrice":"90"}),1_000_000).unwrap();
        apply_user_event(&state, session.generation, &event); let p = state.snapshot().positions.remove(0);
        assert_eq!((p.qty,p.upnl,p.upnl_pct),(expected_qty,expected_pnl,expected_pnl*5.0));
        assert_eq!(p.side,Side::from_position_side(side)); assert_eq!(state.subscribe_symbols().borrow().symbols,["X"]);
        apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":side,"positionAmt":"0"}])).unwrap();
        assert!(state.snapshot().positions.is_empty()); assert!(state.subscribe_symbols().borrow().symbols.is_empty()); assert!(state.read(|s| s.liq_rt.is_empty()));
    }
}
#[test]
fn invalid_quantity_is_not_close_or_a_new_position() {
    let state = AppState::new(); let session = login(&state,"A");
    let row = serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":"-2","entryPrice":"100","markPrice":"100","liquidationPrice":"105"}]);
    apply_positions(&state,session.generation,&row).unwrap(); collect_liq_risk(&state,session.generation,1_000_000);
    let before = serde_json::to_value(state.snapshot().positions).unwrap();
    for qty in [Value::Null,serde_json::json!(""),serde_json::json!("bad"),serde_json::json!("NaN"),serde_json::json!("inf")] {
        apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":qty,"entryPrice":"100"}])).unwrap();
        apply_user_event(&state,session.generation,&serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"BOTH","pa":qty,"ep":"100"},{"s":"Y","pa":qty,"ep":"100"}]}}));
        assert_eq!(serde_json::to_value(state.snapshot().positions).unwrap(),before);
        assert_eq!(state.read(|s| s.liq_rt.len()),1); assert!(collect_liq_risk(&state,session.generation,1_000_001).is_empty());
    }
}
#[test]
fn only_mark_prices_trigger_risk_and_lines_in_one_commit_and_deduplicate() {
    for rest_first in [true,false] {
        let state = AppState::new(); let session = login(&state,"A");
        apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":"LONG","positionAmt":"2","entryPrice":"100","markPrice":"120","liquidationPrice":"95"}])).unwrap();
        state.write(|s| s.alerts.push(crate::state::AlertLine{id:"line".into(),symbol:"X".into(),price:110.0,enabled:true}));
        let sub=state.subscribe_symbols().borrow().clone();
        let sample=|price:&str,rest:bool,now| {
            if rest { apply_rest_market(&state,session.generation,"X",&serde_json::json!({"markPrice":price}),now).unwrap() }
            else { apply_market_event_at(&state,&sub,&serde_json::json!({"stream":"x@markPrice@1s","data":{"s":"X","p":price}}),now).unwrap() }
        };
        let first=sample("120",rest_first,1_000_000); assert!(first.liq.is_empty() && first.alerts.is_empty());
        let revision=state.snapshot().revision; let fired=sample("100",!rest_first,1_000_001);
        assert_eq!(state.snapshot().revision,revision+1); assert_eq!(fired.liq.len(),1); assert_eq!(fired.alerts.len(),1);
        assert_eq!(fired.liq[0].generation,session.generation); assert_eq!(fired.alerts[0].generation,session.generation);
        assert_eq!(state.snapshot().positions[0].liq_distance_pct,Some(5.0));
        let repeat=sample("99",rest_first,1_000_002); assert!(repeat.liq.is_empty() && repeat.alerts.is_empty());
        assert!(collect_liq_risk(&state,session.generation,1_000_003).is_empty()); state.set_creds(None);
        assert!(state.effect_if_current(fired.liq[0].generation,||panic!("旧强平通知")).is_none());
        assert!(state.effect_if_current(fired.alerts[0].generation,||panic!("旧价格通知")).is_none());
        let before=state.snapshot().revision;
        assert!(apply_rest_market(&state,session.generation,"X",&serde_json::json!({"markPrice":"100"}),1_000_004).is_err());
        assert_eq!(state.snapshot().revision,before);
    }
}
#[test]
fn invalid_mark_prices_do_not_revalue_or_advance_health() {
    let state=AppState::new(); let session=login(&state,"A");
    apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":"LONG","positionAmt":"1","entryPrice":"100","markPrice":"100"}])).unwrap();
    let sub=state.subscribe_symbols().borrow().clone(); let before=serde_json::to_value(state.snapshot()).unwrap();
    for price in ["","bad","NaN","inf","0","-1"] {
        assert!(apply_rest_market(&state,session.generation,"X",&serde_json::json!({"markPrice":price}),1_000_000).is_err());
        assert!(apply_market_event_at(&state,&sub,&serde_json::json!({"stream":"x@markPrice@1s","data":{"s":"X","p":price}}),1_000_000).is_none());
        assert_eq!(serde_json::to_value(state.snapshot()).unwrap(),before);
    }
}
#[test]
fn rest_ws_and_private_interleaving_preserves_orders_until_successful_empty_list() {
    let state=AppState::new(); let session=login(&state,"A");
    let row=serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":"-2","entryPrice":"100","markPrice":"100","leverage":"10","liquidationPrice":"120"}]);
    apply_positions(&state,session.generation,&row).unwrap();
    apply_orders(&state,session.generation,"X",&serde_json::json!([{"type":"TAKE_PROFIT_MARKET","positionSide":"BOTH","stopPrice":"80"},{"type":"STOP_MARKET","positionSide":"BOTH","stopPrice":"110"}])).unwrap();
    let sub=state.subscribe_symbols().borrow().clone();
    apply_rest_market(&state,session.generation,"X",&serde_json::json!({"markPrice":"90"}),1_000_000).unwrap();
    apply_market_event_at(&state,&sub,&serde_json::json!({"stream":"x@markPrice@1s","data":{"s":"X","p":"91"}}),1_000_001).unwrap();
    apply_user_event(&state,session.generation,&serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"BOTH","pa":"-3","ep":"100"}]}}));
    apply_positions(&state,session.generation,&row).unwrap();
    for invalid in [serde_json::json!({"code":-1}),serde_json::json!([{"type":"STOP_MARKET","stopPrice":"NaN"}]),serde_json::json!([null])] {
        assert!(apply_orders(&state,session.generation,"X",&invalid).is_err()); let p=state.snapshot().positions.remove(0);
        assert_eq!((p.tp_price,p.sl_price),(Some(80.0),Some(110.0))); assert_eq!(p.mark_price,91.0);
    }
    apply_orders(&state,session.generation,"X",&serde_json::json!([])).unwrap();
    let p=state.snapshot().positions.remove(0); assert_eq!((p.tp_price,p.sl_price),(None,None));
}
#[test]
fn both_flip_and_rest_close_reset_only_that_position_lifecycle() {
    for via_rest in [false,true] {
        let state=AppState::new(); let session=login(&state,"A");
        let rows=serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":"1","entryPrice":"100","markPrice":"100","liquidationPrice":"95"},{"symbol":"Y","positionSide":"LONG","positionAmt":"1","entryPrice":"100","markPrice":"100","liquidationPrice":"95"}]);
        apply_positions(&state,session.generation,&rows).unwrap(); assert_eq!(collect_liq_risk(&state,session.generation,1_000_000).len(),2);
        if via_rest { apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":"-1","entryPrice":"100","markPrice":"100","liquidationPrice":"105"}])).unwrap(); }
        else { apply_user_event(&state,session.generation,&serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"BOTH","pa":"-1","ep":"100"}]}})); }
        let intents=collect_liq_risk(&state,session.generation,1_000_001);
        assert_eq!(intents.len(),1); assert_eq!(intents[0].symbol,"X"); assert!(intents[0].short);
        apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":"BOTH","positionAmt":"0"}])).unwrap();
        assert!(!state.read(|s| s.liq_rt.contains_key(&("X".into(),Side::Both))));
        apply_positions(&state,session.generation,&rows).unwrap(); assert_eq!(collect_liq_risk(&state,session.generation,1_000_002).len(),1);
        let next=login(&state,"B"); apply_positions(&state,next.generation,&rows).unwrap(); assert_eq!(collect_liq_risk(&state,next.generation,1_000_003).len(),2);
    }
}
#[test]
fn hedge_order_fields_and_lifecycle_remain_separate() {
    let state=AppState::new(); let session=login(&state,"A");
    apply_positions(&state,session.generation,&serde_json::json!([{"symbol":"X","positionSide":"LONG","positionAmt":"2","entryPrice":"100","markPrice":"100","leverage":"10","liquidationPrice":"95"},{"symbol":"X","positionSide":"SHORT","positionAmt":"-2","entryPrice":"100","markPrice":"100","leverage":"10","liquidationPrice":"105"}])).unwrap();
    apply_orders(&state,session.generation,"X",&serde_json::json!([{"type":"TAKE_PROFIT_MARKET","positionSide":"LONG","stopPrice":"120"},{"type":"STOP_MARKET","positionSide":"LONG","stopPrice":"96"},{"type":"TAKE_PROFIT_MARKET","positionSide":"SHORT","stopPrice":"80"},{"type":"STOP_MARKET","positionSide":"SHORT","stopPrice":"104"}])).unwrap();
    assert_eq!(collect_liq_risk(&state,session.generation,1_000_000).len(),2); let sub=state.subscribe_symbols().borrow().clone();
    apply_market_event_at(&state,&sub,&serde_json::json!({"stream":"x@markPrice@1s","data":{"s":"X","p":"101"}}),1_000_001).unwrap();
    apply_rest_market(&state,session.generation,"X",&serde_json::json!({"markPrice":"100"}),1_000_002).unwrap();
    state.read(|s| {
        let long=&s.positions[&("X".into(),Side::Long)]; let short=&s.positions[&("X".into(),Side::Short)];
        assert_eq!((long.tp_price,long.sl_price),(Some(120.0),Some(96.0))); assert_eq!((short.tp_price,short.sl_price),(Some(80.0),Some(104.0)));
    });
    apply_user_event(&state,session.generation,&serde_json::json!({"e":"ACCOUNT_UPDATE","a":{"P":[{"s":"X","ps":"LONG","pa":"0"}]}}));
    assert_eq!(state.snapshot().positions.len(),1); assert_eq!(state.subscribe_symbols().borrow().symbols,["X"]);
    assert!(state.read(|s| s.liq_rt.contains_key(&("X".into(),Side::Short)) && !s.liq_rt.contains_key(&("X".into(),Side::Long))));
}
#[test]
fn source_freshness_does_not_cross_or_accept_invalid_account() {
    let state=AppState::new(); let session=login(&state,"A");
    apply_account(&state,session.generation,&account("100")).unwrap();
    let before=state.snapshot().account_freshness.last_success_ms;
    apply_rest_market(&state,session.generation,"X",&serde_json::json!({"markPrice":"100"}),before+100_000).unwrap();
    assert_eq!(state.snapshot().account_freshness.last_success_ms,before);
    assert!(state.snapshot().account_freshness.stale_at(before+100_000));
    assert!(!state.snapshot().markets[0].freshness.stale_at(before+100_000));
    assert!(apply_account(&state,session.generation,&serde_json::json!({"totalWalletBalance":"NaN"})).is_err());
    assert_eq!(state.snapshot().account_freshness.last_success_ms,before);
}
