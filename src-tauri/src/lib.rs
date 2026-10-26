mod binance;
mod chart;
mod commands;
mod engine;
mod freshness;
mod state;

use state::AppState;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 单实例必须第一个注册: 第二个进程要在建窗口和拉流之前就退出,
    // 否则两个实例各带一套 WebView2 + WS 连接, 内存直接翻倍.
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
                app.state::<AppState>().risk_wake.notify_one();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed && shortcut == &toggle_shortcut() {
                        commands::toggle_window(app.clone());
                    }
                })
                .build(),
        )
        .manage(AppState::new())
        // 所有命令和后台任务共享同一进程冷却窗口, 账户切换不重建客户端.
        .manage(Arc::new(binance::RestClient::new()))
        .setup(|app| {
            let handle = app.handle().clone();
            commands::restore_geometry(&handle);
            load_settings(&handle);
            setup_tray(app)?;
            // 热键被别的程序占用时不能阻断启动, 托盘菜单仍可显隐.
            if let Err(e) = app.global_shortcut().register(toggle_shortcut()) {
                eprintln!("[init] 全局快捷键 Ctrl+Alt+B 注册失败: {e}");
            }

            binance::user_ws::spawn(handle.clone());
            binance::market_ws::spawn(handle.clone());
            binance::kline_ws::spawn(handle.clone());
            engine::tick::spawn(handle.clone());
            engine::supplement::spawn(handle.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                    commands::save_geometry(window.app_handle());
                }
                WindowEvent::Focused(true) => {
                    window.app_handle().state::<AppState>().risk_wake.notify_one();
                }
                WindowEvent::CloseRequested { api, .. } => {
                    // 关闭按钮只隐藏, 真正退出走托盘菜单, 避免上班时误关.
                    api.prevent_close();
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::has_credentials,
            commands::save_credentials,
            commands::clear_credentials,
            commands::self_check,
            commands::get_snapshot,
            commands::get_klines,
            commands::get_alerts,
            commands::set_alerts,
            commands::add_alert,
            commands::remove_alert,
            commands::set_risk,
            commands::toggle_window,
            commands::hide_window,
            commands::set_always_on_top,
            commands::quit_app,
            commands::set_focus,
            commands::clear_focus,
            commands::autostart_enabled,
            commands::set_autostart,
        ])
        .run(tauri::generate_context!())
        .expect("启动主程序失败");
}

/// 全局显隐热键: Ctrl+Alt+B. 上班时不用离开键盘去找托盘图标.
fn toggle_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyB)
}

fn load_settings(app: &tauri::AppHandle) {
    let st = app.state::<AppState>();
    // 启动诊断: 没加载到凭证时所有网络任务都会静默空转, 必须能从日志看出来.
    match commands::load_creds() {
        Some(creds) => {
            eprintln!("[init] 已加载 API 凭证");
            st.set_creds(Some(creds));
        }
        None => eprintln!("[init] 未加载到 API 凭证, 网络任务将空转"),
    }
    if let Ok(store) = commands::settings_store(app) {
        if let Some(v) = store.get(commands::STORE_ALERTS)
            && let Ok(alerts) = serde_json::from_value::<Vec<state::AlertLine>>(v.clone())
        {
            st.write(|s| s.alerts = alerts);
        }
        if let Some(value) = store.get(commands::STORE_RISK) {
            commands::apply_saved_risk(&st, &value);
        }
    }
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示 / 隐藏", true, None::<&str>)?;
    let top = MenuItem::with_id(app, "top", "切换置顶", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &top, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("币安盯盘")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => commands::toggle_window(app.clone()),
            "top" => {
                if let Some(w) = app.get_webview_window("main") {
                    let on = w.is_always_on_top().unwrap_or(true);
                    let _ = w.set_always_on_top(!on);
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                commands::toggle_window(tray.app_handle().clone());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}
