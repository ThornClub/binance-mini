use serde::Serialize;

pub const REQUEST_BUDGET_MS: i64 = 12_000;
pub const MARK_MAX_AGE_MS: i64 = 5_000;
pub const CHART_REFRESH_INTERVAL_MS: i64 = 5_000;
// 包含备用REST周期、请求预算及前端1秒调度误差, 不把流活跃期限用作数据超龄期限.
pub const CHART_MAX_AGE_MS: i64 = CHART_REFRESH_INTERVAL_MS + REQUEST_BUDGET_MS + 1_000;
pub const ACCOUNT_INTERVAL_MS: i64 = 12_000;
pub const SLOW_INTERVAL_MS: i64 = 10_000;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Freshness {
    pub last_success_ms: i64,
    pub max_age_ms: i64,
    pub error: Option<String>,
}
impl Freshness {
    pub fn new(max_age_ms: i64) -> Self { Self { max_age_ms, ..Default::default() } }
    pub fn success(&mut self, now: i64) { self.last_success_ms = now; self.error = None; }
    pub fn stale_at(&self, now: i64) -> bool {
        self.last_success_ms <= 0 || self.error.is_some() || now - self.last_success_ms > self.max_age_ms
    }
}
pub fn risk_interval_ms(visible: bool, has_positions: bool, high_weight: bool) -> i64 {
    let base = if !has_positions { 60_000 } else if visible { 1_000 } else { 30_000 };
    base * if high_weight { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_sources_and_hidden_budget() {
        let mut mark = Freshness::new(MARK_MAX_AGE_MS);
        let mut account = Freshness::new(ACCOUNT_INTERVAL_MS + REQUEST_BUDGET_MS);
        account.success(1_000);
        mark.success(40_000);
        assert!(!mark.stale_at(40_001));
        assert!(account.stale_at(40_001));
        let mut risk = Freshness::new(risk_interval_ms(false, true, false) + REQUEST_BUDGET_MS);
        risk.success(1_000);
        assert!(!risk.stale_at(31_000));
        assert!(risk.stale_at(43_001));
        assert_eq!(risk_interval_ms(true, true, false), 1_000);
        assert_eq!(risk_interval_ms(true, false, false), 60_000);
        assert_eq!(risk_interval_ms(false, true, true), 60_000);
    }
}
