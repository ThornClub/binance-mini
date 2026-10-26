use super::*;

fn fake_creds(key: &str) -> Creds { Creds { key: key.into(), secret: "FAKE_SECRET".into() } }
#[test]
fn clear_missing_is_success_but_access_failure_is_reported_and_session_stays_disabled() {
    let state = AppState::new();
    state.set_creds(Some(fake_creds("A")));
    let (op, _) = state.begin_credential_op();
    clear_credentials_with(&state, op, |_| Err(keyring::Error::NoEntry)).unwrap();
    assert!(state.session().creds.is_none());
    state.set_creds(Some(fake_creds("B")));
    let (op, _) = state.begin_credential_op();
    let mut attempts = Vec::new();
    let result = clear_credentials_with(&state, op, |user| {
        attempts.push(user.to_string());
        Err(keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("FAKE_SECRET"))))
    }).unwrap_err();
    assert_eq!(attempts, ["api-key", "api-secret"]);
    assert!(result.contains("api-key") && result.contains("api-secret") && result.contains("拒绝"));
    assert!(!result.contains("FAKE_SECRET"));
    assert!(state.session().creds.is_none());
}
#[tokio::test]
async fn late_validation_cannot_overwrite_clear_or_new_save() {
    let state = Arc::new(AppState::new());
    let (a, _) = state.begin_credential_op();
    let (release, wait) = tokio::sync::oneshot::channel();
    let worker_state = state.clone();
    let old_save = tokio::spawn(async move {
        wait.await.unwrap();
        finish_save(&worker_state, a, fake_creds("A"), Ok(()), |_| panic!("迟到写入 keyring"))
    });
    let (clear, _) = state.begin_credential_op();
    clear_credentials_with(&state, clear, |_| Ok(())).unwrap();
    let (b, _) = state.begin_credential_op();
    finish_save(&state, b, fake_creds("B"), Ok(()), |_| Ok(())).unwrap();
    release.send(()).unwrap();
    assert!(old_save.await.unwrap().is_err());
    assert_eq!(state.session().creds.as_ref().unwrap().key, "B");
}
#[test]
fn failed_validation_never_calls_storage_or_changes_session() {
    let state = AppState::new();
    let (op, _) = state.begin_credential_op();
    assert!(finish_save(&state, op, fake_creds("A"), Err("验证失败".into()), |_| panic!("不应保存")).is_err());
    assert!(state.session().creds.is_none());
}
fn line(id: &str, price: f64) -> AlertLine { AlertLine { id: id.into(), symbol: "X".into(), price, enabled: true } }
#[test]
fn alert_changes_preserve_unchanged_crossing_and_cooldown_and_collect_deleted_state() {
    let state = AppState::new();
    update_alerts_with(&state, |a| a.push(line("keep", 100.0)), |_| Ok(())).unwrap();
    engine::alerts::collect(&state, 0, "X", 99.0, 1_000_000);
    assert_eq!(engine::alerts::collect(&state, 0, "X", 101.0, 1_000_001).len(), 1);
    update_alerts_with(&state, |a| a.push(line("edit", 200.0)), |_| Ok(())).unwrap();
    engine::alerts::collect(&state, 0, "X", 201.0, 1_000_002);
    update_alerts_with(&state, |a| a[1].price = 250.0, |_| Ok(())).unwrap();
    state.read(|s| {
        assert_eq!((s.alert_rt["keep"].last_side, s.alert_rt["keep"].last_fired_ms), (1, 1_000_001));
        assert!(!s.alert_rt.contains_key("edit"));
    });
    assert!(engine::alerts::collect(&state, 0, "X", 99.0, 1_000_003).is_empty());
    update_alerts_with(&state, |a| a.retain(|a| a.id == "keep"), |_| Ok(())).unwrap();
    assert!(!state.read(|s| s.alert_rt.contains_key("edit")));
    assert_eq!(engine::alerts::collect(&state, 0, "X", 101.0, 1_300_002).len(), 1);
}
#[test]
fn each_changed_alert_field_resets_only_its_own_runtime() {
    for field in ["id", "symbol", "price", "enabled"] {
        let state = AppState::new();
        update_alerts_with(&state, |a| a.extend([line("keep", 100.0), line("edit", 200.0)]), |_| Ok(())).unwrap();
        engine::alerts::collect(&state, 0, "X", 99.0, 1_000_000);
        update_alerts_with(&state, |a| match field {
            "id" => a[1].id = "new".into(), "symbol" => a[1].symbol = "Y".into(),
            "price" => a[1].price = 201.0, "enabled" => a[1].enabled = false, _ => unreachable!(),
        }, |_| Ok(())).unwrap();
        assert!(state.read(|s| s.alert_rt.contains_key("keep") && !s.alert_rt.contains_key("edit")));
    }
}
#[test]
fn settings_failures_do_not_publish_config_or_reset_runtime() {
    let state = AppState::new();
    update_alerts_with(&state, |a| a.push(line("keep", 100.0)), |_| Ok(())).unwrap();
    engine::alerts::collect(&state, 0, "X", 99.0, 1_000_000);
    state.write(|s| { s.liq_rt.insert(("X".into(), crate::state::Side::Long), crate::state::LiqRuntime { armed: false, last_fired_ms: 123 }); });
    let before = state.snapshot().revision;
    assert_eq!(update_alerts_with(&state, |a| a.clear(), |_| Err("存储失败".into())).unwrap_err(), "存储失败");
    assert_eq!(set_risk_with(&state, 20.0, |_| Err("存储失败".into())).unwrap_err(), "存储失败");
    assert_eq!(state.snapshot().revision, before);
    state.read(|s| {
        assert_eq!(s.alerts, [line("keep", 100.0)]);
        assert_eq!(s.alert_rt["keep"].last_side, -1);
        assert_eq!(s.risk.liq_warn_pct, 10.0);
        assert_eq!(s.liq_rt[&("X".into(), crate::state::Side::Long)].last_fired_ms, 123);
    });
}
#[test]
fn failed_store_save_restores_previous_cache_including_absent_keys() {
    use std::cell::RefCell;
    for initial in [None, Some(serde_json::json!({"liq_warn_pct":10.0}))] {
        let cache = RefCell::new(initial.clone());
        let result = persist_value_with(serde_json::json!({"liq_warn_pct":20.0}), |next| cache.replace(next), || {
            assert_eq!(cache.borrow().as_ref().unwrap()["liq_warn_pct"], 20.0);
            Err("磁盘保存失败".into())
        });
        assert_eq!(result.unwrap_err(), "磁盘保存失败");
        assert_eq!(*cache.borrow(), initial);
    }
}
#[test]
fn risk_clamps_returns_saved_value_and_same_effective_threshold_keeps_runtime() {
    let state = AppState::new();
    for (input, expected) in [(0.0, 1.0), (100.0, 50.0), (12.5, 12.5)] {
        let mut stored = None;
        let confirmed = set_risk_with(&state, input, |risk| {
            assert_ne!(state.snapshot().risk.liq_warn_pct, expected);
            stored = Some(serde_json::json!({"liq_warn_pct":risk.liq_warn_pct})); Ok(())
        }).unwrap();
        assert_eq!(confirmed.liq_warn_pct, expected);
        assert_eq!(confirmed.hysteresis_pct, 2.0);
        let restarted = AppState::new();
        let serialized = serde_json::to_string(&stored.unwrap()).unwrap();
        apply_saved_risk(&restarted, &serde_json::from_str(&serialized).unwrap());
        assert_eq!(restarted.snapshot().risk.liq_warn_pct, expected);
        state.write(|s| { s.liq_rt.insert(("X".into(), crate::state::Side::Long), crate::state::LiqRuntime { armed:false, last_fired_ms:123 }); });
        set_risk_with(&state, input, |_| panic!("相同有效值无需重存")).unwrap();
        assert_eq!(state.read(|s| s.liq_rt[&("X".into(), crate::state::Side::Long)].last_fired_ms), 123);
    }
    for input in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] { assert!(set_risk_with(&state, input, |_| panic!("无效值不得保存")).is_err()); }
}
#[test]
fn concurrent_alert_additions_do_not_lose_updates_or_duplicate_ids() {
    let state = Arc::new(AppState::new());
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2).map(|_| {
        let state = state.clone(); let barrier = barrier.clone();
        std::thread::spawn(move || { barrier.wait(); add_alert_with(&state, "X".into(), 100.0, |_| Ok(())).unwrap(); })
    }).collect();
    barrier.wait();
    for worker in workers { worker.join().unwrap(); }
    state.read(|s| { assert_eq!(s.alerts.len(), 2); assert_ne!(s.alerts[0].id, s.alerts[1].id); });
}
#[test]
fn invalid_alert_settings_never_reach_storage() {
    let state = AppState::new();
    for lines in [vec![line("x", f64::NAN)], vec![line("x", 0.0)], vec![line("x", 1.0), line("x", 2.0)]] {
        assert!(update_alerts_with(&state, |a| *a = lines, |_| panic!("无效告警不得保存")).is_err());
    }
    assert!(state.read(|s| s.alerts.is_empty()));
}
