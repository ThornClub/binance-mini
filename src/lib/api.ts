import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { AlertLine, Candle, Snapshot } from "./types";
import { EMPTY_SNAPSHOT } from "./types";

let current: Snapshot = EMPTY_SNAPSHOT;
const listeners = new Set<() => void>();

function notify() {
  for (const l of listeners) l();
}

export function subscribeSnapshot(fn: () => void) {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

export function getSnapshot(): Snapshot {
  return current;
}

let started = false;

/** 连接 Rust 事件源, 只建立一次. */
export async function initBridge() {
  if (started) return;
  started = true;

  await listen<Snapshot>("state://snapshot", (e) => {
    current = e.payload;
    notify();
  });

  await listen<{ symbol: string; price: number }>("state://alert", (e) => {
    playAlertSound();
    lastAlert = e.payload;
    notify();
  });

  current = await invoke<Snapshot>("get_snapshot");
  notify();
}

export let lastAlert: { symbol: string; price: number } | null = null;

// 全程只用一个 AudioContext: 每次告警新建一个会累积到浏览器上限后静默失败.
let audioCtx: AudioContext | null = null;

/// WebView2 的自动播放策略会挂起没有用户手势的 AudioContext, 表现为"没声音".
/// 用户第一次点窗口 (拖拽/切合约/开设置) 时提前解锁, 之后告警才能出声.
function unlockAudio() {
  try {
    if (!audioCtx) audioCtx = new AudioContext();
    if (audioCtx.state === "suspended") void audioCtx.resume();
  } catch {
    /* 无音频设备时忽略 */
  }
}
window.addEventListener("pointerdown", unlockAudio, { once: true });
window.addEventListener("keydown", unlockAudio, { once: true });

function playAlertSound() {
  try {
    unlockAudio();
    if (!audioCtx) return;
    const octx = audioCtx;
    const osc = octx.createOscillator();
    const gain = octx.createGain();
    osc.frequency.value = 880;
    gain.gain.value = 0.08;
    osc.connect(gain).connect(octx.destination);
    osc.start();
    osc.stop(octx.currentTime + 0.18);
  } catch {
    /* 提示音失败不影响主流程 */
  }
}

export const api = {
  hasCredentials: () => invoke<boolean>("has_credentials"),
  saveCredentials: (apiKey: string, apiSecret: string) =>
    invoke<void>("save_credentials", { apiKey, apiSecret }),
  clearCredentials: () => invoke<void>("clear_credentials"),
  selfCheck: () => invoke<string>("self_check"),
  klines: (symbol: string, interval = "1m", limit = 120) =>
    invoke<Candle[]>("get_klines", { symbol, interval, limit }),
  alerts: () => invoke<AlertLine[]>("get_alerts"),
  addAlert: (symbol: string, price: number) =>
    invoke<AlertLine[]>("add_alert", { symbol, price }),
  removeAlert: (id: string) => invoke<AlertLine[]>("remove_alert", { id }),
  toggleWindow: () => invoke<void>("toggle_window"),
  hideWindow: () => invoke<void>("hide_window"),
  setAlwaysOnTop: (on: boolean) => invoke<void>("set_always_on_top", { on }),
  quit: () => invoke<void>("quit_app"),
  setFocus: (symbol: string | null) => invoke<void>("set_focus", { symbol }),
  autostartEnabled: () => invoke<boolean>("autostart_enabled"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
};
