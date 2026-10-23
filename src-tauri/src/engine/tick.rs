use crate::binance::RestClient;
use crate::engine;
use crate::state::AppState;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// positionRisk 轮询. 强平价只能从 REST 拿到, 所以必须轮询;
/// 但频率跟随窗口可见性与是否有仓位, 避免空转吃限流.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let rest = RestClient::new();
        let mut creds_rx = state.subscribe_creds();
        let mut calibrated = false;
        let mut account_counter = 0u32;
        // 首轮用短间隔: 建仓后 60 秒才刷到价格/费率, 观感像卡住了.
        let mut first_round = true;
        let mut slow_counter = 0u32;

        loop {
            let creds = creds_rx.borrow().clone();
            let Some(creds) = creds else {
                calibrated = false;
                if creds_rx.changed().await.is_err() {
                    return;
                }
                continue;
            };

            if !calibrated {
                let _ = rest.sync_time().await;
                calibrated = true;
            }

            let has_positions = state.read(|s| !s.positions.is_empty());
            let visible = app
                .get_webview_window("main")
                .and_then(|w| w.is_visible().ok())
                .unwrap_or(true);

            let base = if !has_positions {
                60
            } else if visible {
                // 行情推送被节点掐断时的兜底节奏: 1 秒一轮, 权重约 420/分钟, 上限 2400.
                1
            } else {
                30
            };
            // 权重逼近上限时自动翻倍间隔, 避免把 2400/分钟的额度打满.
            let secs = if rest.used_weight() > 2000 {
                base * 2
            } else {
                base
            };

            // REST 失败必须上报, 否则断网/代理不通时界面只是静默显示空白.
            // 行情流拿不到推送时靠 REST 兜底刷新标记价与资金费率.
            let (symbols, focus) = state.read(|s| {
                let mut v: Vec<String> = s.positions.values().map(|p| p.symbol.clone()).collect();
                v.sort();
                v.dedup();
                let focus = s.focus_symbol.clone().or_else(|| v.first().cloned());
                (v, focus)
            });

            // 限流冷却期内不发请求, 也不刷错误, 安静等冷却结束.
            if rest.blocked_ms() > 0 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            if let Err(e) = engine::refresh_market(&app, &rest, &symbols).await {
                engine::set_error(&app, Some(e));
            }
            // 24 小时统计 10 秒一次就够, 不必跟着 1 秒节奏走.
            slow_counter = slow_counter.wrapping_add(1);
            // 启动头几轮先补齐 24h 数据, 之后回到 10 秒一次.
            if slow_counter <= 3 || slow_counter % 10 == 1 {
                if let Err(e) = engine::refresh_ticker(&app, &rest, &symbols).await {
                    engine::set_error(&app, Some(e));
                }
            }
            // 止盈止损挂在订单上, 10 秒拉一次就够.
            if slow_counter % 10 == 2 {
                if let Some(sym) = focus.as_deref() {
                    if let Err(e) = engine::refresh_orders(&app, &rest, &creds, sym).await {
                        engine::set_error(&app, Some(e));
                    }
                }
            }
            // 只有前端正在看的合约需要每轮刷最后一根 K 线.
            if let Some(sym) = focus.as_deref() {
                if let Err(e) = engine::refresh_last_candle(&app, &rest, sym).await {
                    engine::set_error(&app, Some(e));
                }
            }

            let pos_ok = match engine::refresh_positions(&app, &rest, &creds).await {
                Ok(()) => true,
                Err(e) => {
                    engine::set_error(&app, Some(e));
                    false
                }
            };
            account_counter = account_counter.wrapping_add(1);
            if account_counter % 12 == 1 {
                if let Err(e) = engine::refresh_account(&app, &rest, &creds).await {
                    engine::set_error(&app, Some(e));
                }
            }

            if pos_ok {
                engine::touch_rest(&app);
            }
            // 一轮只推一次快照, 前端不用为同一秒的三次 emit 重渲染三次.
            engine::emit(&app);

            let secs = if first_round {
                first_round = false;
                3
            } else {
                secs
            };

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                changed = creds_rx.changed() => {
                    calibrated = false;
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });
}
