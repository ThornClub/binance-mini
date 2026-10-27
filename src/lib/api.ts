import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AlertLine, ChartHistory, ChartToken, ClosePlan, CloseResult, KlineInterval, RiskSettings, Side, Snapshot } from "./types";
import { EMPTY_SNAPSHOT } from "./types";
import { chartFeed } from "./chart-session";

let current: Snapshot = EMPTY_SNAPSHOT;
const listeners = new Set<() => void>();
function notify() { for (const listener of listeners) listener(); }
function applySnapshot(next: Snapshot) {
  // 先缓存同会话每一根live, 再做快照版本门禁, 避免跨线程投递次序漏根.
  chartFeed.ingest(next.chart, next.account_generation, next.revision);
  if (next.revision < current.revision) return;
  if (next.account_generation !== current.account_generation) { lastAlert = null; lastLiqAlert = null; }
  current = next;
  notify();
}
export function subscribeSnapshot(fn: () => void) {
  listeners.add(fn);
  return () => { listeners.delete(fn); };
}
export function getSnapshot(): Snapshot { return current; }

let initialization: Promise<void> | null = null;
/** 共享同一个完成状态; 失败清理已成功注册的部分监听, 下一次显式调用可重试. */
export function initBridge(): Promise<void> {
  if (initialization) return initialization;
  // 先安装共享Promise再执行listen, 同步抛错也不会把失败Promise永久缓存.
  initialization = Promise.resolve().then(async () => {
    const unlisten: UnlistenFn[] = [];
    let active = true;
    try {
      unlisten.push(await listen<Snapshot>("state://snapshot", (e) => { if (active) applySnapshot(e.payload); }));
      unlisten.push(await listen<{ symbol: string; price: number; account_generation: number }>("state://alert", (e) => {
        if (!active || e.payload.account_generation !== current.account_generation) return;
        playAlertSound(); lastAlert = e.payload; notify();
      }));
      unlisten.push(await listen<{ symbol: string; side: string; distance: number; account_generation: number }>("state://liq", (e) => {
        if (!active || e.payload.account_generation !== current.account_generation) return;
        playAlertSound(); lastLiqAlert = e.payload; notify();
      }));
      applySnapshot(await invoke<Snapshot>("get_snapshot"));
    } catch (error) {
      active = false;
      for (const stop of unlisten.reverse()) { try { stop(); } catch { /* 继续清理其余监听. */ } }
      initialization = null;
      throw error;
    }
  });
  return initialization;
}

export let lastAlert: { symbol: string; price: number } | null = null;
export let lastLiqAlert: { symbol: string; side: string; distance: number } | null = null;
let audioCtx: AudioContext | null = null;
function unlockAudio() {
  try {
    if (!audioCtx) audioCtx = new AudioContext();
    if (audioCtx.state === "suspended") void audioCtx.resume();
  } catch { /* 无音频设备时忽略. */ }
}
window.addEventListener("pointerdown", unlockAudio, { once: true });
window.addEventListener("keydown", unlockAudio, { once: true });
function playAlertSound() {
  try {
    unlockAudio();
    if (!audioCtx) return;
    const osc = audioCtx.createOscillator();
    const gain = audioCtx.createGain();
    osc.frequency.value = 880;
    gain.gain.value = 0.08;
    osc.connect(gain).connect(audioCtx.destination);
    osc.start(); osc.stop(audioCtx.currentTime + 0.18);
  } catch { /* 提示音失败不影响主流程. */ }
}

// 仅串行短订阅命令, 历史请求不占队列, B不必等待A的慢REST.
let focusQueue: Promise<unknown> = Promise.resolve();
function focusCommand<T>(command: string, args: Record<string, unknown>): Promise<T> {
  const result = focusQueue.then(() => invoke<T>(command, args));
  focusQueue = result.catch(() => {});
  return result;
}
export const api = {
  hasCredentials: () => invoke<boolean>("has_credentials"),
  saveCredentials: (apiKey: string, apiSecret: string) => invoke<void>("save_credentials", { apiKey, apiSecret }),
  clearCredentials: () => invoke<void>("clear_credentials"),
  selfCheck: () => invoke<string>("self_check"),
  klines: (token: ChartToken, reconnectEpoch: number, requestId: number, limit: number) =>
    invoke<ChartHistory>("get_klines", { token, reconnectEpoch, requestId, limit }),
  setFocus: (symbol: string, interval: KlineInterval) => focusCommand<ChartToken>("set_focus", { symbol, interval }),
  clearFocus: (token: ChartToken) => focusCommand<void>("clear_focus", { token }),
  alerts: () => invoke<AlertLine[]>("get_alerts"),
  addAlert: (symbol: string, price: number) => invoke<AlertLine[]>("add_alert", { symbol, price }),
  removeAlert: (id: string) => invoke<AlertLine[]>("remove_alert", { id }),
  setRisk: (liqWarnPct: number) => invoke<RiskSettings>("set_risk", { liqWarnPct }),
  // 平仓是真实写操作: 预览只读校验, 发单由后端重新读持仓后再决定数量.
  closePreview: (symbol: string, side: Side, qty: number) => invoke<ClosePlan>("close_preview", { symbol, side, qty }),
  closePosition: (symbol: string, side: Side, qty: number) => invoke<CloseResult>("close_position", { symbol, side, qty }),
  toggleWindow: () => invoke<void>("toggle_window"),
  hideWindow: () => invoke<void>("hide_window"),
  setAlwaysOnTop: (on: boolean) => invoke<void>("set_always_on_top", { on }),
  quit: () => invoke<void>("quit_app"),
  autostartEnabled: () => invoke<boolean>("autostart_enabled"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
};
