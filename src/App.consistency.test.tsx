import { StrictMode, act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT, type PositionView, type Snapshot } from "./lib/types";
import type { ChartLine } from "./components/MiniKline";
const mocks = vi.hoisted(() => ({
  snapshot: null as Snapshot | null,
  listeners: new Set<() => void>(),
  chart: vi.fn(), init: vi.fn(),
  api: { hasCredentials: vi.fn(), alerts: vi.fn(), autostartEnabled: vi.fn(), removeAlert: vi.fn() },
}));
vi.mock("./lib/api", () => ({
  api: mocks.api, initBridge: mocks.init, getSnapshot: () => mocks.snapshot ?? EMPTY_SNAPSHOT,
  subscribeSnapshot: (fn: () => void) => { mocks.listeners.add(fn); return () => mocks.listeners.delete(fn); },
}));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: vi.fn() }));
vi.mock("./components/MiniKline", () => ({ default: (props: unknown) => { mocks.chart(props); return null; } }));
import App from "./App";
let host: HTMLDivElement, root: Root;
const fresh = () => ({ last_success_ms: Date.now(), max_age_ms: 5000, error: null });
function position(side: PositionView["side"], entry: number): PositionView {
  return { symbol: "XUSDT", side, qty: 1, entry_price: entry, mark_price: 100, upnl: 1, upnl_pct: 10,
    leverage: 10, liq_price: entry - 10, liq_distance_pct: 20, margin_ratio: 1, tp_price: entry + 20, sl_price: entry - 5 };
}
async function update(patch: Partial<Snapshot>) {
  await act(async () => { mocks.snapshot = { ...mocks.snapshot!, ...patch }; for (const fn of mocks.listeners) fn(); });
}
beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(1_000_000); vi.resetAllMocks();
  mocks.snapshot = { ...EMPTY_SNAPSHOT, position_freshness: fresh(), account_freshness: fresh() };
  mocks.init.mockResolvedValue(undefined); mocks.api.hasCredentials.mockResolvedValue(true);
  mocks.api.alerts.mockResolvedValue([]); mocks.api.autostartEnabled.mockResolvedValue(false);
  host = document.createElement("div"); document.body.append(host); root = createRoot(host);
});
afterEach(async () => { await act(async () => { root.unmount(); }); host.remove(); vi.useRealTimers(); });

describe("App状态一致性", () => {
  it("点击同symbol空仓后重排不改变方向, 开仓/强平/TP/SL线来自实际点击行", async () => {
    const long = position("Long", 100), short = position("Short", 200);
    mocks.snapshot = { ...mocks.snapshot!, positions: [long, short] };
    await act(async () => { root.render(<App />); });
    await act(async () => { host.querySelectorAll<HTMLElement>(".pos")[1].click(); });
    await update({ positions: [short, long] });
    expect(host.querySelectorAll(".focused")).toHaveLength(1);
    expect(host.querySelector(".focused .badge")?.textContent).toContain("空");
    const props = mocks.chart.mock.lastCall![0] as { symbol: string; lines: ChartLine[] };
    expect(props.symbol).toBe("XUSDT");
    expect(props.lines.map((l) => [l.title, l.price])).toEqual([["开", 200], ["强平", 190], ["止盈", 220], ["止损", 195]]);
  });
  it("初始默认方向也不随收益排名改变", async () => {
    const long = position("Long", 100), short = position("Short", 200);
    mocks.snapshot = { ...mocks.snapshot!, positions: [long, short] };
    await act(async () => { root.render(<App />); });
    await update({ positions: [short, long] });
    expect(host.querySelector(".focused .badge")?.textContent).toContain("多");
  });
  it("空仓仍在设置页删除全部告警, 失败保留原行并显示错误", async () => {
    mocks.api.alerts.mockResolvedValue([{ id: "orphan", symbol: "OLDUSDT", price: 90, enabled: true }]);
    mocks.api.removeAlert.mockRejectedValueOnce("保存失败").mockResolvedValueOnce([]);
    await act(async () => { root.render(<App />); });
    await act(async () => { host.querySelector<HTMLButtonElement>('[title="设置"]')!.click(); });
    expect(host.querySelector(".all-alerts")?.textContent).toContain("OLDUSDT");
    await act(async () => { host.querySelector<HTMLButtonElement>(".all-alerts button")!.click(); });
    expect(host.querySelector(".all-alerts")?.textContent).toContain("保存失败");
    expect(host.querySelector(".all-alerts")?.textContent).toContain("OLDUSDT");
    await act(async () => { host.querySelector<HTMLButtonElement>(".all-alerts button")!.click(); });
    expect(mocks.api.removeAlert).toHaveBeenLastCalledWith("orphan");
    expect(host.querySelector(".all-alerts")?.textContent).toContain("暂无价格告警");
  });
  it("无推送时账户独立变旧, 新风险快照不会替账户恢复正常", async () => {
    await act(async () => { root.render(<App />); });
    await act(async () => { await vi.advanceTimersByTimeAsync(6000); });
    expect(host.querySelector(".mr.data-stale")).not.toBeNull();
    await update({ position_freshness: fresh() });
    expect(host.querySelector(".hero.data-stale")).toBeNull();
    expect(host.querySelector(".mr.data-stale")).not.toBeNull();
    expect(host.querySelector(".mr")?.textContent).toContain("过期");
  });
  it("初始化失败显示明确重试, StrictMode清理旧组件续体", async () => {
    mocks.init.mockRejectedValue(new Error("IPC失败"));
    await act(async () => { root.render(<StrictMode><App /></StrictMode>); });
    expect(host.textContent).toContain("初始化失败");
    expect(host.textContent).not.toContain("暂无持仓");
    mocks.init.mockResolvedValue(undefined);
    await act(async () => { host.querySelector<HTMLButtonElement>(".setup button")!.click(); });
    expect(host.textContent).not.toContain("IPC失败");
    expect(host.querySelector('[title="设置"]')).not.toBeNull();
    expect(mocks.listeners.size).toBe(1);
  });
});
