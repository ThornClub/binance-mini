use crate::state::{AppState, Inner};
use tauri::Manager;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

pub const EVT_ALERT: &str = "state://alert";
/// 同一条线触发后的冷却时间, 防止价格在线上抖动时连续轰炸.
const COOLDOWN_MS: i64 = 5 * 60 * 1000;

fn side_of(price: f64, line: f64) -> i8 {
    if price > line {
        1
    } else if price < line {
        -1
    } else {
        0
    }
}

pub struct AlertIntent {
    pub generation: u64,
    pub id: String,
    pub symbol: String,
    pub price: f64,
}

/// 调用者已持有会话提交锁, 此处不再次加锁或执行通知 IO.
pub fn collect_inner(s: &mut Inner, generation: u64, symbol: &str, price: f64, now: i64) -> Vec<AlertIntent> {
    if !price.is_finite() || price <= 0.0 {
        return Vec::new();
    }
    let mut fired = Vec::new();
    for alert in s.alerts.iter().filter(|a| a.enabled && a.symbol == symbol && a.price.is_finite() && a.price > 0.0) {
        let rt = s.alert_rt.entry(alert.id.clone()).or_default();
        let cur = side_of(price, alert.price);
        if cur == 0 {
            continue;
        }
        if rt.last_side == 0 {
            // 首次见到这条线, 只记录当前侧, 不触发. 否则启动瞬间会把已越过的线全弹一遍.
            rt.last_side = cur;
            continue;
        }
        let crossed = rt.last_side != cur;
        let cooled = now - rt.last_fired_ms > COOLDOWN_MS;
        rt.last_side = cur;
        if crossed && cooled {
            rt.last_fired_ms = now;
            fired.push(AlertIntent {
                generation,
                id: alert.id.clone(),
                symbol: symbol.to_string(),
                price: alert.price,
            });
        }
    }
    fired
}

#[cfg(test)]
pub fn collect(state: &AppState, generation: u64, symbol: &str, price: f64, now: i64) -> Vec<AlertIntent> {
    state.write_if_current(generation, |s| collect_inner(s, generation, symbol, price, now)).unwrap_or_default()
}

pub fn notify(app: &AppHandle, intents: Vec<AlertIntent>) {
    let state = app.state::<AppState>();
    for intent in intents {
        state.effect_if_current(intent.generation, || {
            let body = format!("{} 穿越 {:.6}", intent.symbol, intent.price);
            let _ = app
                .notification()
                .builder()
                .title("币安盯盘")
                .body(&body)
                .show();
            let _ = app.emit(
                EVT_ALERT,
                serde_json::json!({ "id": intent.id, "symbol": intent.symbol, "price": intent.price, "account_generation": intent.generation }),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{binance::Creds, state::AlertLine};

    #[test]
    fn delayed_alert_intent_and_price_cannot_affect_new_session() {
        let state = AppState::new();
        state.set_creds(Some(Creds { key: "A".into(), secret: "fake".into() }));
        let generation = state.session().generation;
        state.write(|s| s.alerts.push(AlertLine { id: "x".into(), symbol: "X".into(), price: 100.0, enabled: true }));
        assert!(collect(&state, generation, "X", 99.0, 1_000_000).is_empty());
        let intents = collect(&state, generation, "X", 101.0, 1_000_001);
        assert_eq!(intents.len(), 1);
        state.set_creds(None);
        state.set_creds(Some(Creds { key: "B".into(), secret: "fake".into() }));
        assert!(collect(&state, generation, "X", 99.0, 2_000_000).is_empty());
        assert!(state.read(|s| s.alert_rt.is_empty()));
        assert!(state.effect_if_current(intents[0].generation, || panic!("旧通知 IO")).is_none());
    }

    #[test]
    fn first_equal_crossing_and_strict_cooldown_keep_existing_semantics() {
        let state = AppState::new();
        state.write(|s| s.alerts.push(AlertLine { id: "x".into(), symbol: "X".into(), price: 100.0, enabled: true }));
        let now = 1_000_000;
        assert!(collect(&state, 0, "X", 100.0, now).is_empty());
        assert!(collect(&state, 0, "X", 99.0, now).is_empty());
        assert!(collect(&state, 0, "X", 100.0, now).is_empty());
        assert_eq!(collect(&state, 0, "X", 101.0, now).len(), 1);
        assert!(collect(&state, 0, "X", 101.0, now + 1).is_empty());
        assert!(collect(&state, 0, "X", 99.0, now + COOLDOWN_MS).is_empty());
        assert_eq!(collect(&state, 0, "X", 101.0, now + COOLDOWN_MS + 1).len(), 1);
    }

    #[test]
    fn invalid_prices_do_not_change_crossing_state() {
        let state = AppState::new();
        state.write(|s| s.alerts.push(AlertLine { id: "x".into(), symbol: "X".into(), price: 100.0, enabled: true }));
        collect(&state, 0, "X", 99.0, 1_000_000);
        for price in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert!(collect(&state, 0, "X", price, 2_000_000).is_empty());
        }
        assert_eq!(state.read(|s| s.alert_rt["x"].last_side), -1);
        assert_eq!(collect(&state, 0, "X", 101.0, 2_000_000).len(), 1);
    }
}
