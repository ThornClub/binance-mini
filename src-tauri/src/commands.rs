use crate::binance::{Creds, RestClient};
use crate::engine;
use crate::state::{AlertLine, AppState, Candle, Snapshot};
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_store::StoreExt;

const KEYRING_SERVICE: &str = "binance-mini";
pub const STORE_FILE: &str = "settings.json";
pub const STORE_ALERTS: &str = "alerts";
pub const STORE_WINDOW: &str = "window";

fn entry(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| format!("系统凭据库不可用: {e}"))
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
pub fn has_credentials() -> bool {
    load_creds().is_some()
}

/// 保存凭证前先用真实接口做一次权限自检, 避免把没权限的 Key 存进去之后一直报错.
#[tauri::command]
pub async fn save_credentials(
    app: AppHandle,
    api_key: String,
    api_secret: String,
) -> Result<(), String> {
    let creds = Creds {
        key: api_key.trim().to_string(),
        secret: api_secret.trim().to_string(),
    };
    if creds.key.len() < 20 || creds.secret.len() < 20 {
        return Err("API Key 或 Secret 长度不对, 请重新从币安复制".to_string());
    }

    let rest = RestClient::new();
    rest.sync_time().await?;
    rest.position_risk(&creds).await?;

    entry("api-key")?
        .set_password(&creds.key)
        .map_err(|e| format!("写入系统凭据失败: {e}"))?;
    entry("api-secret")?
        .set_password(&creds.secret)
        .map_err(|e| format!("写入系统凭据失败: {e}"))?;

    app.state::<AppState>().set_creds(Some(creds));
    engine::emit(&app);
    Ok(())
}

#[tauri::command]
pub fn clear_credentials(app: AppHandle) -> Result<(), String> {
    for user in ["api-key", "api-secret"] {
        if let Ok(e) = entry(user) {
            let _ = e.delete_credential();
        }
    }
    app.state::<AppState>().set_creds(None);
    engine::emit(&app);
    Ok(())
}

/// 权限自检: 真实调用一次 positionRisk, 把结论用中文打出来.
/// 这是唯一能确认"只读 Key 到底能不能读持仓"的方法.
#[tauri::command]
pub async fn self_check(app: AppHandle) -> Result<String, String> {
    let creds = load_creds().ok_or("还没有保存 API Key")?;
    let rest = RestClient::new();
    rest.sync_time().await?;
    let v = rest.position_risk(&creds).await?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let active = arr
        .iter()
        .filter(|p| {
            p.get("positionAmt")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|q| q.abs() > 0.0)
                .unwrap_or(false)
        })
        .count();

    // 顺手刷一次真实数据, 让界面立刻有内容.
    let _ = crate::engine::refresh_positions(&app, &rest, &creds).await;
    let _ = crate::engine::refresh_account(&app, &rest, &creds).await;

    Ok(format!(
        "权限正常: 返回 {} 条持仓记录, 其中 {} 条有仓位",
        arr.len(),
        active
    ))
}

#[tauri::command]
pub fn get_snapshot(app: AppHandle) -> Snapshot {
    app.state::<AppState>().snapshot()
}

#[tauri::command]
pub async fn get_klines(
    symbol: String,
    interval: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Candle>, String> {
    let rest = KLINE_CLIENT.get_or_init(RestClient::new);
    // 兜底限流: 上限 500, 前端只按需要的根数取.
    let limit = limit.unwrap_or(120).clamp(20, 500);
    let v = rest
        .klines(&symbol, interval.as_deref().unwrap_or("1m"), limit)
        .await?;
    let arr = v.as_array().ok_or_else(|| "K 线返回非数组".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for row in arr {
        let Some(items) = row.as_array() else {
            continue;
        };
        let f = |i: usize| {
            items
                .get(i)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let t = items.first().and_then(|v| v.as_i64()).unwrap_or(0);
        out.push(Candle {
            t,
            o: f(1),
            h: f(2),
            l: f(3),
            c: f(4),
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn get_alerts(app: AppHandle) -> Vec<AlertLine> {
    app.state::<AppState>().read(|s| s.alerts.clone())
}

#[tauri::command]
pub fn set_alerts(app: AppHandle, alerts: Vec<AlertLine>) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.write(|s| {
        // 线的价格可能被改过, 旧判定状态必须清掉, 否则会漏触发或误触发.
        for a in &alerts {
            s.alert_rt.remove(&a.id);
        }
        s.alerts = alerts.clone();
    });

    if let Ok(store) = app.store(STORE_FILE) {
        store.set(
            STORE_ALERTS,
            serde_json::to_value(&alerts).unwrap_or_default(),
        );
        let _ = store.save();
    }
    state.recompute_symbols();
    engine::emit(&app);
    Ok(())
}

#[tauri::command]
pub fn add_alert(app: AppHandle, symbol: String, price: f64) -> Result<Vec<AlertLine>, String> {
    if !(price.is_finite() && price > 0.0) {
        return Err("价格必须是正数".to_string());
    }
    let mut alerts = get_alerts(app.clone());
    alerts.push(AlertLine {
        id: format!("{}-{}", symbol, crate::state::now_ms()),
        symbol,
        price,
        enabled: true,
    });
    set_alerts(app.clone(), alerts.clone())?;
    Ok(alerts)
}

#[tauri::command]
pub fn remove_alert(app: AppHandle, id: String) -> Result<Vec<AlertLine>, String> {
    let mut alerts = get_alerts(app.clone());
    alerts.retain(|a| a.id != id);
    set_alerts(app.clone(), alerts.clone())?;
    Ok(alerts)
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

/// 记录前端当前聚焦的合约, K 线只刷它.
#[tauri::command]
pub fn set_focus(app: AppHandle, symbol: Option<String>) {
    let state = app.state::<AppState>();
    state.write(|s| s.focus_symbol = symbol);
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
    if let Ok(store) = app.store(STORE_FILE) {
        store.set(
            STORE_WINDOW,
            serde_json::json!({
                "x": pos.x, "y": pos.y, "w": size.width, "h": size.height
            }),
        );
        let _ = store.save();
    }
}

pub fn restore_geometry(app: &AppHandle) {
    let Ok(store) = app.store(STORE_FILE) else {
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

/// K 线接口复用同一个客户端, 避免每次切周期都重新握手 TLS.
static KLINE_CLIENT: std::sync::OnceLock<RestClient> = std::sync::OnceLock::new();
