use crate::binance::RestClient;
use crate::engine;
use crate::freshness::{REQUEST_BUDGET_MS, risk_interval_ms};
use crate::state::{AppState, until_changed};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::sync::Notify;

async fn poll_loop<F, Fut>(wake: &Notify, mut cycle: F)
where F: FnMut() -> Fut, Fut: Future<Output = Duration> {
    loop {
        let delay = cycle().await;
        tokio::select! {
            _ = wake.notified() => {},
            _ = tokio::time::sleep(delay) => {},
        }
    }
}
/// 风险循环不再等待行情、统计、账户或图表REST; 并发数固定, 不逐合约spawn.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let rest = app.state::<Arc<RestClient>>().inner().clone();
        let mut creds_rx = state.subscribe_creds();
        loop {
            let session = creds_rx.borrow_and_update().clone();
            if session.creds.is_none() {
                if creds_rx.changed().await.is_err() { return; }
                continue;
            }
            let work = async {
                let _ = rest.sync_time().await;
                poll_loop(&state.risk_wake, || async {
                    let visible = app.get_webview_window("main").and_then(|w| w.is_visible().ok()).unwrap_or(true);
                    let period = risk_interval_ms(visible, state.read(|s| !s.positions.is_empty()), rest.used_weight() > 2000);
                    state.write_if_current(session.generation, |s| s.position_freshness.max_age_ms = period + REQUEST_BUDGET_MS);
                    if rest.blocked_ms() > 0 { return Duration::from_millis(rest.blocked_ms().max(1) as u64); }
                    if let Err(error) = engine::refresh_positions(&app, &rest, &session).await {
                        engine::set_error(&app, session.generation, Some(error));
                    }
                    engine::emit_if_current(&app, session.generation);
                    Duration::from_millis(period as u64)
                }).await;
            };
            until_changed(&mut creds_rx, work).await;
            if creds_rx.has_changed().is_err() { return; }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test(start_paused = true)]
    async fn slow_supplement_does_not_block_risk_and_foreground_wakes_sleep() {
        let wake = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_wake = wake.clone(); let worker_calls = calls.clone();
        let worker = tokio::spawn(async move {
            poll_loop(&worker_wake, || async {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                Duration::from_secs(30)
            }).await
        });
        let slow = tokio::spawn(std::future::pending::<()>());
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        wake.notify_one(); tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(!slow.is_finished());
        worker.abort(); slow.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn opening_existing_alert_symbol_wakes_risk_without_reconnecting_market() {
        let state = Arc::new(AppState::new());
        state.set_creds(Some(crate::binance::Creds { key: "fake".into(), secret: "fake".into() }));
        state.write(|s| {
            s.alerts.push(crate::state::AlertLine { id: "x".into(), symbol: "X".into(), price: 100.0, enabled: true });
            s.position_freshness.success(1_000);
        });
        state.risk_wake.notified().await;
        let subscription = state.subscribe_symbols().borrow().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_state = state.clone();
        let worker_calls = calls.clone();
        let worker = tokio::spawn(async move {
            poll_loop(&worker_state.risk_wake, || async {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                Duration::from_secs(60)
            }).await;
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        engine::apply_user_event(&state, state.session().generation, &serde_json::json!({
            "e": "ACCOUNT_UPDATE", "a": { "P": [{ "s": "X", "ps": "LONG", "pa": "1", "ep": "100" }] }
        }));
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "已有行情订阅也必须唤醒新仓位风险查询");
        assert_eq!(*state.subscribe_symbols().borrow(), subscription);
        assert!(state.snapshot().position_freshness.stale_at(1_001));
        assert!(state.snapshot().positions[0].liq_price.is_none());
        worker.abort();
    }
}
