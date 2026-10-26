use crate::binance::Creds;
use crate::chart::{ChartToken, ChartView, valid_target};
use crate::freshness::{Freshness, ACCOUNT_INTERVAL_MS, MARK_MAX_AGE_MS, REQUEST_BUDGET_MS};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, watch};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Side { Long, Short, Both }
impl Side {
    pub fn from_position_side(s: &str) -> Side {
        match s { "LONG" => Side::Long, "SHORT" => Side::Short, _ => Side::Both }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct PositionView {
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub entry_price: f64,
    pub mark_price: f64,
    pub upnl: f64,
    pub upnl_pct: f64,
    pub leverage: f64,
    pub liq_price: Option<f64>,
    pub liq_distance_pct: Option<f64>,
    pub margin_ratio: Option<f64>,
    pub tp_price: Option<f64>,
    pub sl_price: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct MarketView {
    pub symbol: String,
    pub mark_price: f64,
    pub funding_rate: f64,
    pub next_funding_at: i64,
    pub high_24h: f64,
    pub low_24h: f64,
    pub change_pct_24h: f64,
    pub quote_volume_24h: f64,
    pub freshness: Freshness,
}
impl Default for MarketView {
    fn default() -> Self {
        Self { symbol: String::new(), mark_price: 0.0, funding_rate: 0.0, next_funding_at: 0,
            high_24h: 0.0, low_24h: 0.0, change_pct_24h: 0.0, quote_volume_24h: 0.0,
            freshness: Freshness::new(MARK_MAX_AGE_MS) }
    }
}
#[derive(Clone, Debug, Serialize, Default)]
pub struct AccountView {
    pub total_wallet_balance: f64,
    pub total_unrealized_pnl: f64,
    pub total_maint_margin: f64,
    pub available_balance: f64,
    pub total_margin_balance: f64,
    pub margin_ratio: f64,
}
#[derive(Clone, Debug, Serialize, Default)]
pub struct HealthView {
    pub user_stream_ok: bool,
    pub market_stream_ok: bool,
    pub started_at_ms: i64,
    pub last_error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AlertLine { pub id: String, pub symbol: String, pub price: f64, pub enabled: bool }
#[derive(Clone, Debug, Default)]
pub struct AlertRuntime { pub last_side: i8, pub last_fired_ms: i64 }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskSettings { pub liq_warn_pct: f64, pub hysteresis_pct: f64 }
impl Default for RiskSettings {
    fn default() -> Self { Self { liq_warn_pct: 10.0, hysteresis_pct: 2.0 } }
}
#[derive(Clone, Debug, Default)]
pub struct LiqRuntime { pub armed: bool, pub last_fired_ms: i64 }
#[derive(Clone, Debug, Default)]
pub struct AccountSession { pub generation: u64, pub creds: Option<Creds> }
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarketSubscription { pub generation: u64, pub revision: u64, pub symbols: Vec<String> }
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub revision: u64,
    pub account_generation: u64,
    pub positions: Vec<PositionView>,
    pub markets: Vec<MarketView>,
    pub account: Option<AccountView>,
    pub health: HealthView,
    pub position_freshness: Freshness,
    pub account_freshness: Freshness,
    pub chart: ChartView,
    pub risk: RiskSettings,
}
pub struct Inner {
    session: AccountSession,
    revision: u64,
    subscription: MarketSubscription,
    chart_generation: u64,
    pub positions: HashMap<(String, Side), PositionView>,
    pub markets: HashMap<String, MarketView>,
    pub account: Option<AccountView>,
    pub health: HealthView,
    pub position_freshness: Freshness,
    pub account_freshness: Freshness,
    pub chart: ChartView,
    pub alerts: Vec<AlertLine>,
    pub alert_rt: HashMap<String, AlertRuntime>,
    pub risk: RiskSettings,
    pub liq_rt: HashMap<(String, Side), LiqRuntime>,
}
impl Default for Inner {
    fn default() -> Self {
        Self {
            session: AccountSession::default(), revision: 0, subscription: MarketSubscription::default(), chart_generation: 0,
            positions: HashMap::new(), markets: HashMap::new(), account: None,
            health: HealthView { started_at_ms: now_ms(), ..Default::default() },
            position_freshness: Freshness::new(60_000 + REQUEST_BUDGET_MS),
            account_freshness: Freshness::new(ACCOUNT_INTERVAL_MS + REQUEST_BUDGET_MS), chart: ChartView::default(),
            alerts: Vec::new(), alert_rt: HashMap::new(), risk: RiskSettings::default(), liq_rt: HashMap::new(),
        }
    }
}
pub struct AppState {
    inner: RwLock<Inner>,
    symbols_tx: watch::Sender<MarketSubscription>,
    creds_tx: watch::Sender<AccountSession>,
    chart_tx: watch::Sender<Option<ChartToken>>,
    pub risk_wake: Notify,
    credential_op: Mutex<u64>,
    credential_op_tx: watch::Sender<u64>,
    effects: Mutex<()>,
}
impl AppState {
    pub fn new() -> Self {
        let (symbols_tx, _) = watch::channel(MarketSubscription::default());
        let (creds_tx, _) = watch::channel(AccountSession::default());
        let (chart_tx, _) = watch::channel(None);
        let (credential_op_tx, _) = watch::channel(0);
        Self { inner: RwLock::new(Inner::default()), symbols_tx, creds_tx, chart_tx, risk_wake: Notify::new(),
            credential_op: Mutex::new(0), credential_op_tx, effects: Mutex::new(()) }
    }
    pub fn read<R>(&self, f: impl FnOnce(&Inner) -> R) -> R { f(&self.inner.read()) }
    pub fn write<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut s = self.inner.write();
        let result = f(&mut s); self.changed(&mut s); result
    }
    pub fn write_if_current<R>(&self, generation: u64, f: impl FnOnce(&mut Inner) -> R) -> Option<R> {
        let mut s = self.inner.write();
        if s.session.generation != generation { return None; }
        let result = f(&mut s); self.changed(&mut s); Some(result)
    }
    pub fn write_if_subscription<R>(&self, sub: &MarketSubscription, f: impl FnOnce(&mut Inner) -> R) -> Option<R> {
        let mut s = self.inner.write();
        if s.subscription.generation != sub.generation || s.subscription.revision != sub.revision { return None; }
        let result = f(&mut s); self.changed(&mut s); Some(result)
    }
    pub fn write_if_chart<R>(&self, token: &ChartToken, epoch: Option<u64>, f: impl FnOnce(&mut ChartView) -> R) -> Option<R> {
        let mut s = self.inner.write();
        if s.session.generation != token.account_generation || s.chart.token.as_ref() != Some(token) ||
            epoch.is_some_and(|e| s.chart.reconnect_epoch != e) { return None; }
        let result = f(&mut s.chart); self.changed(&mut s); Some(result)
    }
    pub fn set_focus(&self, symbol: String, interval: String) -> Result<ChartToken, String> {
        if !valid_target(&symbol, &interval) { return Err("图表合约或周期无效".into()); }
        let mut s = self.inner.write();
        if s.session.creds.is_none() { return Err("会话没有凭据".into()); }
        s.chart_generation += 1;
        let token = ChartToken { account_generation: s.session.generation, chart_generation: s.chart_generation, symbol, interval };
        s.chart = ChartView { token: Some(token.clone()), ..Default::default() };
        self.changed(&mut s);
        self.chart_tx.send_replace(Some(token.clone()));
        Ok(token)
    }
    pub fn clear_focus(&self, token: &ChartToken) {
        let mut s = self.inner.write();
        if s.chart.token.as_ref() != Some(token) { return; }
        s.chart = ChartView::default();
        self.changed(&mut s);
        self.chart_tx.send_replace(None);
    }
    /// 同步副作用与会话切换串行, 不跨网络等待持锁.
    pub fn effect_if_current<R>(&self, generation: u64, f: impl FnOnce() -> R) -> Option<R> {
        let _gate = self.effects.lock();
        if self.session().generation != generation { return None; }
        Some(f())
    }
    pub fn session(&self) -> AccountSession { self.read(|s| s.session.clone()) }
    pub fn subscribe_symbols(&self) -> watch::Receiver<MarketSubscription> { self.symbols_tx.subscribe() }
    pub fn subscribe_creds(&self) -> watch::Receiver<AccountSession> { self.creds_tx.subscribe() }
    pub fn subscribe_chart(&self) -> watch::Receiver<Option<ChartToken>> { self.chart_tx.subscribe() }
    pub fn begin_credential_op(&self) -> (u64, watch::Receiver<u64>) {
        let mut op = self.credential_op.lock(); *op += 1;
        self.credential_op_tx.send_replace(*op); (*op, self.credential_op_tx.subscribe())
    }
    pub fn commit_credentials<R>(&self, op: u64, commit: impl FnOnce() -> Result<R, String>) -> Result<R, String> {
        let current = self.credential_op.lock();
        if *current != op { return Err("凭据操作已被更新的操作取消".to_string()); }
        commit()
    }
    pub fn set_creds(&self, creds: Option<Creds>) {
        let _gate = self.effects.lock();
        let mut s = self.inner.write();
        s.session = AccountSession { generation: s.session.generation + 1, creds };
        s.positions.clear(); s.markets.clear(); s.account = None;
        s.chart = ChartView::default(); s.alert_rt.clear(); s.liq_rt.clear();
        s.position_freshness = Freshness::new(60_000 + REQUEST_BUDGET_MS);
        s.account_freshness = Freshness::new(ACCOUNT_INTERVAL_MS + REQUEST_BUDGET_MS);
        s.health = HealthView { started_at_ms: now_ms(), ..Default::default() };
        self.changed(&mut s);
        self.chart_tx.send_replace(None);
        self.creds_tx.send_replace(s.session.clone());
    }
    fn changed(&self, s: &mut Inner) {
        s.revision += 1;
        let mut symbols = Vec::new();
        if s.session.creds.is_some() {
            symbols.extend(s.positions.values().filter(|p| p.qty.is_finite() && p.qty != 0.0).map(|p| p.symbol.clone()));
            symbols.extend(s.alerts.iter().filter(|a| a.enabled).map(|a| a.symbol.clone()));
            symbols.sort(); symbols.dedup();
        }
        if s.subscription.generation != s.session.generation || s.subscription.symbols != symbols {
            s.subscription = MarketSubscription { generation: s.session.generation, revision: s.subscription.revision + 1, symbols };
            self.symbols_tx.send_replace(s.subscription.clone());
            // 私有流新建仓位时唤醒原本60秒的空仓风险轮询.
            self.risk_wake.notify_one();
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.read(|s| {
            let mut positions: Vec<PositionView> = s.positions.values().cloned().collect();
            positions.sort_by(|a, b| b.upnl.abs().partial_cmp(&a.upnl.abs()).unwrap_or(std::cmp::Ordering::Equal));
            let mut markets: Vec<MarketView> = s.markets.values().cloned().collect();
            markets.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            Snapshot { revision: s.revision, account_generation: s.session.generation, positions, markets,
                account: s.account.clone(), health: s.health.clone(), position_freshness: s.position_freshness.clone(),
                account_freshness: s.account_freshness.clone(), chart: s.chart.clone(), risk: s.risk.clone() }
        })
    }
}
pub async fn until_changed<T, F: Future>(rx: &mut watch::Receiver<T>, work: F) -> Option<F::Output> {
    tokio::select! { biased; _ = rx.changed() => None, result = work => Some(result) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn creds(name: &str) -> Creds { Creds { key: name.into(), secret: "test-secret".into() } }
    #[test]
    fn position_side_maps_to_side() {
        assert_eq!(Side::from_position_side("LONG"), Side::Long);
        assert_eq!(Side::from_position_side("SHORT"), Side::Short);
        assert_eq!(Side::from_position_side("BOTH"), Side::Both);
        assert_eq!(Side::from_position_side(""), Side::Both);
    }
    #[test]
    fn risk_defaults_are_sane() {
        let r = RiskSettings::default();
        assert!(r.liq_warn_pct > 0.0 && r.liq_warn_pct < 50.0); assert!(r.hysteresis_pct > 0.0);
    }
    #[test]
    fn fresh_state_has_no_positions() {
        let snap = AppState::new().snapshot(); assert!(snap.positions.is_empty()); assert!(snap.account.is_none());
    }
    #[test]
    fn old_session_cannot_write_notify_or_publish_after_clear_and_b() {
        let state = AppState::new(); state.set_creds(Some(creds("A"))); let a = state.session();
        let token = state.set_focus("BTCUSDT".into(), "1m".into()).unwrap();
        state.write(|s| {
            s.alerts.push(AlertLine { id: "line".into(), symbol: "BTCUSDT".into(), price: 1.0, enabled: true });
            s.alert_rt.insert("line".into(), AlertRuntime::default());
            s.liq_rt.insert(("BTCUSDT".into(), Side::Long), LiqRuntime::default());
        });
        let old_sub = state.subscribe_symbols().borrow().clone(); state.set_creds(None);
        assert!(state.subscribe_symbols().borrow().symbols.is_empty()); state.set_creds(Some(creds("B")));
        let before = state.snapshot();
        assert!(state.write_if_current(a.generation, |s| { s.account = Some(AccountView::default()); s.health.last_error = Some("旧错误".into()); s.markets.insert("OLD".into(), MarketView::default()); }).is_none());
        assert!(state.write_if_subscription(&old_sub, |_| panic!("旧订阅不能写入")).is_none());
        assert!(state.write_if_chart(&token, None, |_| panic!("旧图表不能写入")).is_none());
        assert!(state.effect_if_current(a.generation, || panic!("旧通知不能发送")).is_none());
        assert_eq!(state.snapshot().revision, before.revision);
        state.read(|s| { assert!(s.alert_rt.is_empty() && s.liq_rt.is_empty()); assert!(s.chart.token.is_none()); assert_eq!(s.alerts.len(), 1); });
        assert_ne!(old_sub.generation, state.subscribe_symbols().borrow().generation);
    }
    #[test]
    fn latest_credential_operation_wins_without_running_stale_io() {
        let state = AppState::new(); let (a, _) = state.begin_credential_op(); let (clear, _) = state.begin_credential_op();
        state.commit_credentials(clear, || { state.set_creds(None); Ok(()) }).unwrap();
        let (b, _) = state.begin_credential_op(); state.commit_credentials(b, || { state.set_creds(Some(creds("B"))); Ok(()) }).unwrap();
        assert!(state.commit_credentials(a, || -> Result<(), String> { panic!("过期 keyring 写入") }).is_err());
        assert_eq!(state.session().creds.unwrap().key, "B");
    }
    #[tokio::test]
    async fn pending_work_and_backoff_cancel_without_waiting() {
        let state = AppState::new(); let mut rx = state.subscribe_creds(); state.set_creds(Some(creds("A")));
        assert!(until_changed(&mut rx, std::future::pending::<()>()).await.is_none()); state.set_creds(None);
        assert!(until_changed(&mut rx, tokio::time::sleep(std::time::Duration::from_secs(60))).await.is_none());
    }
    #[test]
    fn same_symbols_new_subscription_rejects_old_revision() {
        let state = AppState::new(); state.set_creds(Some(creds("A"))); let old = state.subscribe_symbols().borrow().clone();
        state.write(|s| s.alerts.push(AlertLine { id: "x".into(), symbol: "X".into(), price: 1.0, enabled: true }));
        state.write(|s| s.alerts.clear()); assert_eq!(old.symbols, state.subscribe_symbols().borrow().symbols);
        assert!(state.write_if_subscription(&old, |_| ()).is_none());
    }
}
