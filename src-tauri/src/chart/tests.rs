use super::*;
use crate::binance::Creds;
use crate::state::AlertLine;

fn state() -> AppState {
    let s = AppState::new();
    s.set_creds(Some(Creds { key: "fake".into(), secret: "fake".into() })); s
}
fn live(token: &ChartToken, t: i64, n: u64, closed: bool) -> Value {
    serde_json::json!({"e":"kline","s":token.symbol,"k":{"s":token.symbol,"i":token.interval,"t":t,"o":"100","h":"110","l":"90","c":"105","n":n,"x":closed}})
}
#[test]
fn all_intervals_and_invalid_payloads_use_product_parser() {
    let s=state();
    for interval in ["1m","15m","1h","1d"] {
        let token=s.set_focus("BTCUSDT".into(),interval.into()).unwrap();
        let epoch=begin_connection(&s,&token).unwrap();
        assert!(apply_live(&s,&token,epoch,&live(&token,60_000,3,false),1000));
        assert_eq!(s.snapshot().chart.candle.unwrap().trades,3);
        let mut invalid=live(&token,120_000,4,false); invalid["k"]["c"]=serde_json::json!("NaN");
        let before=s.snapshot().revision;
        assert!(!apply_live(&s,&token,epoch,&invalid,2000));
        assert_eq!(s.snapshot().revision,before);
        invalid=live(&token,120_000,4,false); invalid["k"]["i"]=serde_json::json!("5m");
        assert!(parse_live(&invalid,&token).is_none());
    }
    assert!(s.set_focus("BTCUSDT".into(),"5m".into()).is_err());
    assert!(s.set_focus("BTCUSDT/stream?x".into(),"1m".into()).is_err());
}
#[test]
fn history_trade_count_and_confirmed_closed_only() {
    let rows=parse_history(&serde_json::json!([[60000,"100","110","90","105","0",119999,"0",3],[120000,"100","110","90","101","0",179999,"0",4]])).unwrap();
    assert_eq!(rows[0].trades,3); assert!(rows[0].closed); assert!(!rows[1].closed);
    assert!(parse_history(&serde_json::json!([[60000,"100","90","90","105","0",119999,"0",3]])).is_err());
    assert!(parse_history(&serde_json::json!([])).is_err());
    assert!(parse_history(&serde_json::json!([[60000,"100","110","90","105"]])).is_err());
}
#[test]
fn terminal_state_and_versions_never_regress() {
    let s=state(); let token=s.set_focus("X".into(),"1h".into()).unwrap(); let epoch=begin_connection(&s,&token).unwrap();
    assert!(apply_live(&s,&token,epoch,&live(&token,60000,10,true),1000));
    assert!(!apply_live(&s,&token,epoch,&live(&token,60000,9,true),2000));
    assert!(!apply_live(&s,&token,epoch,&live(&token,60000,10,false),2000));
    assert_eq!(s.snapshot().chart.freshness.last_success_ms,1000);
    assert!(s.snapshot().chart.candle.unwrap().closed);
    assert!(apply_live(&s,&token,epoch,&live(&token,120000,1,false),3000));
}
#[test]
fn a_b_a_late_history_live_error_and_cleanup_cannot_touch_new_a() {
    let s=state(); let a=s.set_focus("A".into(),"1m".into()).unwrap(); let ae=begin_connection(&s,&a).unwrap();
    s.set_focus("B".into(),"1h".into()).unwrap();
    let new_a=s.set_focus("A".into(),"1m".into()).unwrap(); let epoch=begin_connection(&s,&new_a).unwrap();
    s.clear_focus(&a); assert_eq!(s.snapshot().chart.token,Some(new_a.clone()));
    assert!(!apply_live(&s,&a,ae,&live(&a,60000,10,false),2000));
    connection_result(&s,&a,ae,Err("旧错误".into()));
    assert!(finish_history(&s,a,ae,1,Err("旧历史错误".into()),2000).is_err());
    assert!(s.snapshot().chart.freshness.error.is_none());
    assert!(apply_live(&s,&new_a,epoch,&live(&new_a,60000,10,false),3000));
}
#[test]
fn focus_and_interval_changes_do_not_publish_risk_subscription() {
    let s=state();
    s.write(|i| i.alerts.push(AlertLine{id:"x".into(),symbol:"X".into(),price:1.0,enabled:true}));
    let mut marks=s.subscribe_symbols(); marks.borrow_and_update();
    let a=s.set_focus("X".into(),"1m".into()).unwrap();
    let b=s.set_focus("X".into(),"15m".into()).unwrap();
    s.clear_focus(&a);
    assert_eq!(s.snapshot().chart.token,Some(b));
    assert!(!marks.has_changed().unwrap());
}
#[test]
fn failed_recovery_not_healed_by_live_and_old_epoch_history_rejected() {
    let s=state(); let token=s.set_focus("X".into(),"1d".into()).unwrap(); let old=begin_connection(&s,&token).unwrap();
    let epoch=connection_result(&s,&token,old,Ok(())).unwrap();
    s.write_if_chart(&token,Some(epoch),|c| c.request_id=2);
    assert!(finish_history(&s,token.clone(),old,1,Ok(vec![]),1000).is_err());
    assert!(finish_history(&s,token.clone(),epoch,2,Err("补洞失败".into()),1000).is_err());
    apply_live(&s,&token,epoch,&live(&token,60000,2,false),2000);
    assert_eq!(s.snapshot().chart.freshness.error.as_deref(),Some("补洞失败"));
    assert!(!s.snapshot().chart.history_ready);
    s.write_if_chart(&token,Some(epoch),|c| { c.request_id=3; c.connected=true; });
    finish_history(&s,token,epoch,3,Ok(vec![]),3000).unwrap();
    assert!(s.snapshot().chart.history_ready);
    assert_eq!(s.snapshot().chart.status,ChartStatus::Live);
}
#[tokio::test]
async fn pending_chart_work_and_backoff_cancel_on_target_watch() {
    let s=state(); let a=s.set_focus("A".into(),"1m".into()).unwrap();
    let mut rx=s.subscribe_chart(); rx.borrow_and_update();
    s.set_focus("B".into(),"15m".into()).unwrap();
    assert!(until_changed(&mut rx,std::future::pending::<()>()).await.is_none());
    rx.borrow_and_update(); s.clear_focus(&a);
    assert!(!rx.has_changed().unwrap());
    let current=s.snapshot().chart.token.unwrap(); s.clear_focus(&current);
    assert!(until_changed(&mut rx,tokio::time::sleep(std::time::Duration::from_secs(60))).await.is_none());
}

#[test]
fn handshake_success_requires_post_handshake_history_for_all_intervals() {
    for (interval, width) in [("1m", 60_000), ("15m", 900_000), ("1h", 3_600_000), ("1d", 86_400_000)] {
        let s = state();
        let token = s.set_focus("X".into(), interval.into()).unwrap();
        let attempt = begin_connection(&s, &token).unwrap();
        s.write_if_chart(&token, Some(attempt), |chart| chart.request_id = 1);
        let early = Candle { t: width, o: 100.0, h: 110.0, l: 90.0, c: 100.0, trades: 1, closed: false };
        finish_history(&s, token.clone(), attempt, 1, Ok(vec![early.clone()]), 1_000).unwrap();
        connection_result(&s, &token, attempt, Ok(()));
        let connected = s.snapshot().chart;
        assert!(connected.reconnect_epoch > attempt, "握手成功必须使握手前历史过期");
        assert!(!connected.history_ready);
        assert_eq!(connected.status, ChartStatus::Recovering);
        let epoch = connected.reconnect_epoch;
        assert!(finish_history(&s, token.clone(), attempt, 1, Ok(vec![early.clone()]), 2_000).is_err());
        assert!(apply_live(&s, &token, epoch, &live(&token, width * 2, 2, false), 2_001));
        assert_eq!(s.snapshot().chart.status, ChartStatus::Recovering);
        s.write_if_chart(&token, Some(epoch), |chart| chart.request_id = 2);
        assert!(finish_history(&s, token.clone(), epoch, 2, Err("补洞失败".into()), 2_002).is_err());
        assert!(s.snapshot().chart.freshness.stale_at(2_003));
        let closed = Candle { trades: 8, closed: true, c: 109.0, ..early };
        let next = Candle { t: width * 2, closed: false, ..closed.clone() };
        s.write_if_chart(&token, Some(epoch), |chart| chart.request_id = 3);
        let history = finish_history(&s, token.clone(), epoch, 3, Ok(vec![closed, next]), 3_000).unwrap();
        assert_eq!(history.rows[0].trades, 8);
        assert!(history.rows[0].closed);
        assert_eq!(s.snapshot().chart.status, ChartStatus::Live);
    }
}
#[tokio::test(start_paused = true)]
async fn failed_attempts_do_not_invalidate_eight_second_history() {
    let s = state();
    let token = s.set_focus("BTCUSDT".into(), "1m".into()).unwrap();
    let epoch = begin_connection(&s, &token).unwrap();
    s.write_if_chart(&token, Some(epoch), |chart| chart.request_id = 1);
    let slow_history = async {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        finish_history(&s, token.clone(), epoch, 1, Ok(vec![]), 10_000).unwrap();
    };
    let attempts = async {
        for delay in [2, 4, 8] {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            connection_result(&s, &token, epoch, Err("握手失败".into()));
            assert_eq!(begin_connection(&s, &token), Some(epoch), "失败握手不能作废在途REST");
        }
    };
    tokio::join!(slow_history, attempts);
    assert!(s.snapshot().chart.history_ready);
    assert!(!s.snapshot().chart.freshness.stale_at(26_000), "数据期限必须覆盖备用刷新与请求预算");
}
#[test]
fn handshake_without_valid_frame_is_not_live() {
    let s = state();
    let token = s.set_focus("BTCUSDT".into(), "1m".into()).unwrap();
    let attempt = begin_connection(&s, &token).unwrap();
    let epoch = connection_result(&s, &token, attempt, Ok(())).unwrap();
    s.write_if_chart(&token, Some(epoch), |chart| chart.request_id = 1);
    finish_history(&s, token, epoch, 1, Ok(vec![]), 10_000).unwrap();
    assert_ne!(s.snapshot().chart.status, ChartStatus::Live, "握手不能证明有效行情已到达");
}
#[test]
fn disconnected_history_success_is_fresh_two_seconds_later() {
    let s = state();
    let token = s.set_focus("BTCUSDT".into(), "1m".into()).unwrap();
    let epoch = begin_connection(&s, &token).unwrap();
    connection_result(&s, &token, epoch, Err("K线连接失败: 测试断线".into()));
    s.write_if_chart(&token, Some(epoch), |chart| chart.request_id = 1);
    let rows = parse_history(&serde_json::json!([
        [60000, "100", "110", "90", "105", "0", 119999, "0", 3]
    ])).unwrap();
    finish_history(&s, token, epoch, 1, Ok(rows), 10_000).unwrap();
    let chart = s.snapshot().chart;
    assert_eq!(chart.freshness.last_success_ms, 10_000);
    assert!(!chart.connected);
    assert_eq!(chart.status, ChartStatus::Polling);
    assert_eq!(chart.connection_error.as_deref(), Some("K线连接失败: 测试断线"));
    assert_eq!(chart.last_live_ms, 0);
    assert!(!chart.freshness.stale_at(12_000), "REST刚成功2秒, 连接失败不能把有效数据标成过期");
    assert!(!chart.freshness.stale_at(27_999), "正常备用周期加请求预算和调度误差内不闪烁");
    assert!(chart.freshness.stale_at(28_001), "真正超出备用刷新预算仍要过期");
}
