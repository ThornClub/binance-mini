use crate::state::{AlertRuntime, AppState, now_ms};
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

/// 按"穿越"判定: 只有从一侧跨到另一侧才算触发, 持续在某一侧不算.
pub fn evaluate(app: &AppHandle, symbol: &str, price: f64) {
    if price <= 0.0 {
        return;
    }
    let state = app.state::<AppState>();
    let mut fired: Vec<(String, f64)> = Vec::new();

    state.write(|s| {
        for alert in s.alerts.iter().filter(|a| a.enabled && a.symbol == symbol) {
            let rt = s
                .alert_rt
                .entry(alert.id.clone())
                .or_insert_with(AlertRuntime::default);
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
            let cooled = now_ms() - rt.last_fired_ms > COOLDOWN_MS;
            rt.last_side = cur;
            if crossed && cooled {
                rt.last_fired_ms = now_ms();
                fired.push((alert.id.clone(), alert.price));
            }
        }
    });

    for (id, line) in fired {
        let body = format!("{} 穿越 {:.6}", symbol, line);
        let _ = app
            .notification()
            .builder()
            .title("币安盯盘")
            .body(&body)
            .show();
        let _ = app.emit(
            EVT_ALERT,
            serde_json::json!({ "id": id, "symbol": symbol, "price": line }),
        );
    }
}
