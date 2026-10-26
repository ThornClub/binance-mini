use crate::binance::{Creds, RestClient};
use crate::chart::{self, ChartHistory, ChartToken};
use crate::engine;
use crate::state::{AlertLine, AppState, RiskSettings, Snapshot, until_changed};
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;

const KEYRING_SERVICE: &str = "binance-mini";
pub const STORE_FILE: &str = "settings.json";
pub const STORE_ALERTS: &str = "alerts";
pub const STORE_WINDOW: &str = "window";

pub const STORE_RISK: &str = "risk";
// settings.json 的读改写和发布串行化; 不持 inner 锁做磁盘 IO, 不跨 await.
static SETTINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub fn settings_store(app: &AppHandle) -> Result<Arc<tauri_plugin_store::Store<tauri::Wry>>, String> {
    // 失败后的缓存回滚不能被后台自动保存抢先落盘, 所有写入均显式保存.
    tauri_plugin_store::StoreBuilder::new(app, STORE_FILE).disable_auto_save().build()
        .map_err(|e| format!("打开设置失败: {e}"))
}

fn persist_value_with(
    value: serde_json::Value,
    mut replace: impl FnMut(Option<serde_json::Value>) -> Option<serde_json::Value>,
    save: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let previous = replace(Some(value));
    if let Err(error) = save() {
        // 恢复 store 缓存, 防止之后的窗口几何保存把被拒绝的设置一并写入.
        replace(previous);
        return Err(error);
    }
    Ok(())
}

fn persist_setting(app: &AppHandle, key: &str, value: serde_json::Value) -> Result<(), String> {
    let store = settings_store(app)?;
    persist_value_with(value, |next| {
        let previous = store.get(key);
        match next {
            Some(v) => store.set(key, v),
            None => { store.delete(key); }
        }
        previous
    }, || store.save().map_err(|e| format!("保存设置失败: {e}")))
}

fn entry(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| credential_error(&e))
}

// 不显示系统库的原始错误对象, 部分变体可能附带原始凭据数据.
fn credential_error(error: &keyring::Error) -> String {
    match error {
        keyring::Error::NoEntry => "系统凭据不存在",
        keyring::Error::NoStorageAccess(_) => "系统凭据访问被拒绝",
        keyring::Error::NoDefaultStore => "系统凭据库不可用",
        _ => "系统凭据操作失败",
    }.to_string()
}

pub fn load_creds() -> Option<Creds> {
    let key = entry("api-key").ok()?.get_password().ok()?;
    let secret = entry("api-secret").ok()?.get_password().ok()?;
    if key.is_empty() || secret.is_empty() {
        return None;
    }
    Some(Creds { key, secret })
}

#[tauri::command]
pub fn has_credentials(app: AppHandle) -> bool {
    app.state::<AppState>().session().creds.is_some()
}

/// 保存凭证前先用真实接口做一次权限自检, 避免把没权限的 Key 存进去之后一直报错.
#[tauri::command]
pub async fn save_credentials(
    app: AppHandle,
    api_key: String,
    api_secret: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let (op, mut op_rx) = state.begin_credential_op();
    let creds = Creds {
        key: api_key.trim().to_string(),
        secret: api_secret.trim().to_string(),
    };
    if creds.key.len() < 20 || creds.secret.len() < 20 {
        return Err("API Key 或 Secret 长度不对, 请重新从币安复制".to_string());
    }
    let rest = app.state::<Arc<RestClient>>().inner().clone();
    let validation = until_changed(&mut op_rx, async {
        rest.sync_time().await?;
        rest.position_risk(&creds).await?;
        Ok::<(), String>(())
    }).await.ok_or("凭据操作已被更新的操作取消")?;

    // keyring 与内存提交只占用短同步边界, 不跨网络验证持锁.
    finish_save(&state, op, creds, validation, |creds| {
        entry("api-key")?.set_password(&creds.key).map_err(|e| credential_error(&e))?;
        entry("api-secret")?.set_password(&creds.secret).map_err(|e| credential_error(&e))?;
        Ok(())
    })?;
    engine::emit(&app);
    Ok(())
}

fn finish_save(
    state: &AppState,
    op: u64,
    creds: Creds,
    validation: Result<(), String>,
    persist: impl FnOnce(&Creds) -> Result<(), String>,
) -> Result<(), String> {
    state.commit_credentials(op, || {
        validation?;
        persist(&creds)?;
        state.set_creds(Some(creds));
        Ok(())
    })
}

fn clear_credentials_with(
    state: &AppState,
    op: u64,
    mut delete: impl FnMut(&str) -> Result<(), keyring::Error>,
) -> Result<(), String> {
    state.commit_credentials(op, || {
        // 删除失败也不能重新启用内存会话, 两个条目都必须尝试删除.
        state.set_creds(None);
        let mut errors = Vec::new();
        for user in ["api-key", "api-secret"] {
            match delete(user) {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => errors.push(format!("{user}: {}", credential_error(&e))),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("内存会话已停用, 系统凭据删除失败: {}", errors.join("; ")))
        }
    })
}

#[tauri::command]
pub fn clear_credentials(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let (op, _) = state.begin_credential_op();
    let result = clear_credentials_with(&state, op, |user| {
        keyring::Entry::new(KEYRING_SERVICE, user)?.delete_credential()
    });
    engine::emit(&app);
    result
}

/// 持仓读取自检: positionRisk成功不能证明交易/提现权限已关闭, 仍须用户在交易所侧核查.
#[tauri::command]
pub async fn self_check(app: AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let mut rx = state.subscribe_creds();
    let session = rx.borrow_and_update().clone();
    let creds = session.creds.as_ref().ok_or("还没有保存 API Key")?;
    let rest = app.state::<Arc<RestClient>>().inner().clone();
    let result = until_changed(&mut rx, async {
        rest.sync_time().await?;
        let v = rest.position_risk(creds).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        let active = arr
            .iter()
            .filter(|p| {
                p.get("positionAmt")
                    .and_then(|x| x.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|q| q.is_finite() && q != 0.0)
                    .unwrap_or(false)
            })
            .count();
        // 自检只使用捕获的内存会话, 不从 keyring 重新载入已停用的凭据.
        engine::apply_positions(&state, session.generation, &v)?;
        engine::check_liq_risk(&app, session.generation);
        engine::refresh_account(&app, &rest, &session).await?;
        engine::emit_if_current(&app, session.generation);
        Ok::<String, String>(format!(
            "持仓读取成功: 返回 {} 条持仓记录, 其中 {} 条有仓位. 未审计交易/提现权限, 请在交易所侧确认关闭",
            arr.len(),
            active
        ))
    }).await.ok_or("会话已变更")?;
    state.effect_if_current(session.generation, || result).ok_or("会话已变更")?
}

#[tauri::command]
pub fn get_snapshot(app: AppHandle) -> Snapshot {
    app.state::<AppState>().snapshot()
}

#[tauri::command]
pub async fn get_klines(
    app: AppHandle,
    token: ChartToken,
    reconnect_epoch: u64,
    request_id: u64,
    limit: u32,
) -> Result<ChartHistory, String> {
    let state = app.state::<AppState>();
    let rest = app.state::<Arc<RestClient>>().inner().clone();
    let generation = token.account_generation;
    let result = chart::history(&state, &rest, token, reconnect_epoch, request_id, limit).await;
    engine::emit_if_current(&app, generation);
    result
}

#[tauri::command]
pub fn get_alerts(app: AppHandle) -> Vec<AlertLine> {
    app.state::<AppState>().read(|s| s.alerts.clone())
}

fn update_alerts_with(
    state: &AppState,
    change: impl FnOnce(&mut Vec<AlertLine>),
    persist: impl FnOnce(&[AlertLine]) -> Result<(), String>,
) -> Result<Vec<AlertLine>, String> {
    let _guard = SETTINGS_LOCK.lock();
    let mut alerts = state.read(|s| s.alerts.clone());
    change(&mut alerts);
    let mut ids = std::collections::HashSet::new();
    if alerts.iter().any(|a| a.id.is_empty() || a.symbol.trim().is_empty() || !a.price.is_finite() || a.price <= 0.0 || !ids.insert(a.id.as_str())) {
        return Err("告警必须具有唯一标识、合约与有效正价格".into());
    }
    persist(&alerts)?;
    state.write(|s| {
        // 四个业务字段完全未变才保留方向与冷却; 删除和改动均回收运行态.
        let previous = &s.alerts;
        s.alert_rt.retain(|id, _| {
            alerts.iter().find(|a| &a.id == id).is_some_and(|next| previous.iter().any(|old| old == next))
        });
        s.alerts = alerts.clone();
    });
    Ok(alerts)
}

fn persist_alerts(app: &AppHandle, alerts: &[AlertLine]) -> Result<(), String> {
    let value = serde_json::to_value(alerts).map_err(|e| format!("序列化告警失败: {e}"))?;
    persist_setting(app, STORE_ALERTS, value)
}

#[tauri::command]
pub fn set_alerts(app: AppHandle, alerts: Vec<AlertLine>) -> Result<(), String> {
    update_alerts_with(&app.state::<AppState>(), |current| *current = alerts, |next| persist_alerts(&app, next))?;
    engine::emit(&app);
    Ok(())
}

fn add_alert_with(
    state: &AppState,
    symbol: String,
    price: f64,
    persist: impl FnOnce(&[AlertLine]) -> Result<(), String>,
) -> Result<Vec<AlertLine>, String> {
    if !(price.is_finite() && price > 0.0) {
        return Err("价格必须是正数".to_string());
    }
    update_alerts_with(state, |alerts| {
        let base = format!("{}-{}", symbol, crate::state::now_ms());
        let mut id = base.clone();
        let mut suffix = 0;
        while alerts.iter().any(|a| a.id == id) {
            suffix += 1;
            id = format!("{base}-{suffix}");
        }
        alerts.push(AlertLine { id, symbol, price, enabled: true });
    }, persist)
}

#[tauri::command]
pub fn add_alert(app: AppHandle, symbol: String, price: f64) -> Result<Vec<AlertLine>, String> {
    let alerts = add_alert_with(&app.state::<AppState>(), symbol, price, |next| persist_alerts(&app, next))?;
    engine::emit(&app);
    Ok(alerts)
}

#[tauri::command]
pub fn remove_alert(app: AppHandle, id: String) -> Result<Vec<AlertLine>, String> {
    let alerts = update_alerts_with(&app.state::<AppState>(), |alerts| alerts.retain(|a| a.id != id), |next| persist_alerts(&app, next))?;
    engine::emit(&app);
    Ok(alerts)
}

fn set_risk_with(
    state: &AppState,
    liq_warn_pct: f64,
    persist: impl FnOnce(&RiskSettings) -> Result<(), String>,
) -> Result<RiskSettings, String> {
    if !liq_warn_pct.is_finite() {
        return Err("阈值不是有效数字".to_string());
    }
    let _guard = SETTINGS_LOCK.lock();
    let mut risk = state.read(|s| s.risk.clone());
    let pct = liq_warn_pct.clamp(1.0, 50.0);
    if risk.liq_warn_pct == pct {
        return Ok(risk);
    }
    risk.liq_warn_pct = pct;
    persist(&risk)?;
    state.write(|s| {
        s.risk = risk.clone();
        s.liq_rt.clear();
    });
    Ok(risk)
}

/// 调整强平距离告警阈值. 单位百分比, 限制在 1% 到 50% 之间.
#[tauri::command]
pub fn set_risk(app: AppHandle, liq_warn_pct: f64) -> Result<RiskSettings, String> {
    let risk = set_risk_with(&app.state::<AppState>(), liq_warn_pct, |risk| {
        persist_setting(&app, STORE_RISK, serde_json::json!({ "liq_warn_pct": risk.liq_warn_pct }))
    })?;
    engine::emit(&app);
    Ok(risk)
}

pub fn apply_saved_risk(state: &AppState, value: &serde_json::Value) {
    if let Some(pct) = value.get("liq_warn_pct").and_then(serde_json::Value::as_f64).filter(|v| v.is_finite()) {
        state.write(|s| s.risk.liq_warn_pct = pct.clamp(1.0, 50.0));
    }
}

#[tauri::command]
pub fn toggle_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let visible = w.is_visible().unwrap_or(true);
        if visible {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
            app.state::<AppState>().risk_wake.notify_one();
        }
    }
}

#[tauri::command]
pub fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[tauri::command]
pub fn set_always_on_top(app: AppHandle, on: bool) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_always_on_top(on);
    }
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn set_focus(app: AppHandle, symbol: String, interval: String) -> Result<ChartToken, String> {
    let token = app.state::<AppState>().set_focus(symbol, interval)?;
    engine::emit_if_current(&app, token.account_generation);
    Ok(token)
}

#[tauri::command]
pub fn clear_focus(app: AppHandle, token: ChartToken) -> Result<(), String> {
    app.state::<AppState>().clear_focus(&token);
    engine::emit_if_current(&app, token.account_generation);
    Ok(())
}

#[tauri::command]
pub fn autostart_enabled(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    let r = if on { launcher.enable() } else { launcher.disable() };
    r.map_err(|e| format!("开机自启设置失败: {e}"))
}

/// 拖动窗口时 Moved/Resized 会高频触发, 去抖到 400ms 落一次盘.
static SAVE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn save_geometry(app: &AppHandle) {
    let seq = SAVE_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if SAVE_GEN.load(std::sync::atomic::Ordering::Relaxed) == seq {
            write_geometry(&app);
        }
    });
}

fn write_geometry(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let (Ok(pos), Ok(size)) = (w.outer_position(), w.outer_size()) else {
        return;
    };
    let _guard = SETTINGS_LOCK.lock();
    if let Err(e) = persist_setting(app, STORE_WINDOW, serde_json::json!({
        "x": pos.x, "y": pos.y, "w": size.width, "h": size.height
    })) {
        eprintln!("[settings] {e}");
    }
}

pub fn restore_geometry(app: &AppHandle) {
    let Ok(store) = settings_store(app) else {
        return;
    };
    let Some(v) = store.get(STORE_WINDOW) else {
        return;
    };
    let (Some(x), Some(y), Some(w), Some(h)) = (
        v.get("x").and_then(|x| x.as_i64()),
        v.get("y").and_then(|y| y.as_i64()),
        v.get("w").and_then(|x| x.as_u64()),
        v.get("h").and_then(|x| x.as_u64()),
    ) else {
        return;
    };
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_size(tauri::PhysicalSize::new(w as u32, h as u32));
        // 换过显示器/改过分辨率时, 旧坐标可能落在屏幕外, 钳回主屏右上角.
        let (mut px, mut py) = (x as i32, y as i32);
        if let Ok(Some(m)) = win.primary_monitor() {
            let (mx, my) = (m.position().x, m.position().y);
            let (mw, mh) = (m.size().width as i32, m.size().height as i32);
            let (ww, wh) = (w as i32, h as i32);
            let visible = px + ww > mx + 40
                && px < mx + mw - 40
                && py + wh > my + 40
                && py < my + mh - 40;
            if !visible {
                px = mx + mw - ww - 24;
                py = my + 24;
            }
        }
        let _ = win.set_position(tauri::PhysicalPosition::new(px, py));
    }
}

#[cfg(test)]
mod tests;
