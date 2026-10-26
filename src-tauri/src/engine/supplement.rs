use crate::binance::RestClient;
use crate::chart::ChartToken;
use crate::engine;
use crate::freshness::{ACCOUNT_INTERVAL_MS, SLOW_INTERVAL_MS};
use crate::state::{AppState, now_ms, until_changed};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};

async fn periodic<F, Fut>(period_ms: i64, refresh: F)
where F: Fn() -> Fut, Fut: Future<Output = ()> {
    let mut timer = interval(Duration::from_millis(period_ms as u64));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop { timer.tick().await; refresh().await; }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source { Account, Market, Ticker, Orders }

async fn run_sources<F, Fut>(mut focus_rx: watch::Receiver<Option<ChartToken>>, refresh: F)
where F: Fn(Source) -> Fut, Fut: Future<Output = ()> {
    // join只包围四个常驻循环, 不在每轮等待最慢源; 每源最多一个在途请求.
    tokio::join!(
        periodic(ACCOUNT_INTERVAL_MS, || refresh(Source::Account)),
        periodic(1_000, || refresh(Source::Market)),
        periodic(SLOW_INTERVAL_MS, || refresh(Source::Ticker)),
        async {
            loop {
                focus_rx.borrow_and_update();
                let work = async {
                    refresh(Source::Orders).await;
                    tokio::time::sleep(Duration::from_millis(SLOW_INTERVAL_MS as u64)).await;
                };
                if until_changed(&mut focus_rx, work).await.is_none() && focus_rx.has_changed().is_err() { return; }
            }
        },
    );
}
fn stale_symbols(state: &AppState, now: i64) -> Vec<String> {
    let symbols = state.subscribe_symbols().borrow().symbols.clone();
    state.read(|s| symbols.into_iter().filter(|symbol| s.markets.get(symbol).is_none_or(|m| m.freshness.stale_at(now))).collect())
}
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
            let work = run_sources(state.subscribe_chart(), |source| {
                let (app, rest, session, state) = (&app, rest.as_ref(), &session, state.inner());
                async move {
                    while rest.blocked_ms() > 0 {
                        tokio::time::sleep(Duration::from_millis(rest.blocked_ms().max(1) as u64)).await;
                    }
                    let result = match source {
                        Source::Account => engine::refresh_account(app, rest, session).await,
                        Source::Market => engine::refresh_market(app, rest, session.generation, &stale_symbols(state, now_ms())).await,
                        Source::Ticker => {
                            let symbols = state.subscribe_symbols().borrow().symbols.clone();
                            engine::refresh_ticker(app, rest, session.generation, &symbols).await
                        }
                        Source::Orders => {
                            let focus = state.read(|s| s.chart.token.as_ref().map(|t| t.symbol.clone()));
                            if let Some(symbol) = focus { engine::refresh_orders(app, rest, session, &symbol).await } else { Ok(()) }
                        }
                    };
                    if let Err(error) = result { engine::set_error(app, session.generation, Some(error)); }
                    engine::emit_if_current(app, session.generation);
                }
            });
            until_changed(&mut creds_rx, work).await;
            if creds_rx.has_changed().is_err() { return; }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AlertLine, MarketView};
    use std::sync::Mutex;
    use tokio::time::Instant;

    #[tokio::test(start_paused = true)]
    async fn slow_three_symbol_statistics_do_not_delay_account_cadence() {
        let (_focus_tx, focus_rx) = watch::channel(None);
        let accounts = Arc::new(Mutex::new(Vec::new()));
        let statistics = Arc::new(Mutex::new(Vec::new()));
        let start = Instant::now();
        let (a, t) = (accounts.clone(), statistics.clone());
        let worker = tokio::spawn(async move {
            run_sources(focus_rx, |source| {
                let (a, t) = (a.clone(), t.clone());
                async move {
                    if source == Source::Account { a.lock().unwrap().push(start.elapsed().as_secs()); }
                    if source == Source::Ticker {
                        for _ in 0..3 {
                            t.lock().unwrap().push(start.elapsed().as_secs());
                            tokio::time::sleep(Duration::from_secs(12)).await;
                        }
                    }
                }
            }).await;
        });
        tokio::task::yield_now().await;
        for _ in 0..36 { tokio::time::advance(Duration::from_secs(1)).await; tokio::task::yield_now().await; }
        assert_eq!(*accounts.lock().unwrap(), [0, 12, 24, 36]);
        assert_eq!(&statistics.lock().unwrap()[..3], [0, 12, 24]);
        worker.abort();
    }
    #[tokio::test(start_paused = true)]
    async fn focus_change_wakes_orders_and_account_change_cancels_all_work() {
        let (focus_tx, focus_rx) = watch::channel(None);
        let (session_tx, mut session_rx) = watch::channel(0);
        let orders = Arc::new(Mutex::new(0));
        let count = orders.clone();
        let worker = tokio::spawn(async move {
            until_changed(&mut session_rx, run_sources(focus_rx, |source| {
                let count = count.clone();
                async move { if source == Source::Orders { *count.lock().unwrap() += 1; } }
            })).await
        });
        tokio::task::yield_now().await;
        assert_eq!(*orders.lock().unwrap(), 1);
        focus_tx.send_replace(Some(ChartToken { account_generation: 1, chart_generation: 1, symbol: "X".into(), interval: "1m".into() }));
        tokio::task::yield_now().await;
        assert_eq!(*orders.lock().unwrap(), 2, "焦点变化不等待10秒定时周期");
        session_tx.send_replace(1);
        assert!(worker.await.unwrap().is_none());
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(*orders.lock().unwrap(), 2);
    }
    #[test]
    fn healthy_symbol_skipped_and_orphan_enabled_alert_included() {
        let state = AppState::new();
        state.set_creds(Some(crate::binance::Creds { key: "fake".into(), secret: "fake".into() }));
        state.write(|s| {
            s.alerts = vec![
                AlertLine { id: "live".into(), symbol: "LIVE".into(), price: 1.0, enabled: true },
                AlertLine { id: "closed".into(), symbol: "CLOSED".into(), price: 1.0, enabled: true },
                AlertLine { id: "off".into(), symbol: "OFF".into(), price: 1.0, enabled: false },
            ];
            let mut m = MarketView { symbol: "LIVE".into(), ..Default::default() };
            m.freshness.success(10_000); s.markets.insert("LIVE".into(), m);
        });
        assert_eq!(stale_symbols(&state, 10_001), ["CLOSED"]);
        assert_eq!(stale_symbols(&state, 20_001), ["CLOSED", "LIVE"]);
    }
}
