import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT, type ClosePlan, type CloseResult, type PositionView, type Snapshot } from "./lib/types";

const mocks = vi.hoisted(() => ({
  snapshot: null as Snapshot | null,
  api: {
    hasCredentials: vi.fn(),
    alerts: vi.fn(),
    autostartEnabled: vi.fn(),
    setFocus: vi.fn(),
    setRisk: vi.fn(),
    addAlert: vi.fn(),
    removeAlert: vi.fn(),
    closePreview: vi.fn(),
    closePosition: vi.fn(),
  },
}));

vi.mock("./lib/api", () => ({
  api: mocks.api,
  initBridge: async () => {},
  getSnapshot: () => mocks.snapshot ?? EMPTY_SNAPSHOT,
  subscribeSnapshot: () => () => {},
}));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: vi.fn() }));
vi.mock("./components/MiniKline", () => ({ default: () => null }));

import App from "./App";

let host: HTMLDivElement;
let root: Root;

const plan: ClosePlan = {
  symbol: "XUSDT", qty: 2, qty_text: "2.000", notional: 180,
  order_side: "SELL", position_side: "BOTH", reduce_only: true, max_qty: 2, mark_price: 90,
};
const result: CloseResult = { order_id: 11, status: "FILLED", filled_qty: 2, avg_price: 90.5, qty_text: "2.000" };

function position(side: PositionView["side"], qty: number): PositionView {
  return {
    symbol: "XUSDT", side, qty, entry_price: 100, mark_price: 90,
    upnl: 20, upnl_pct: 100, leverage: 10, liq_price: 120, liq_distance_pct: 100 / 3,
    margin_ratio: null, tp_price: 80, sl_price: 110,
  };
}

function freshSnapshot(positions: PositionView[]): Snapshot {
  const now = Date.now();
  return { ...EMPTY_SNAPSHOT, revision: 1, account_generation: 1, positions,
    position_freshness: { last_success_ms: now, max_age_ms: 60_000, error: null } };
}

async function render() {
  await act(async () => { root.render(<App />); });
  await act(async () => { await Promise.resolve(); });
}

async function openPanel() {
  await act(async () => { host.querySelector<HTMLButtonElement>(".pos-close-open")!.click(); });
  await act(async () => { await new Promise((done) => setTimeout(done, 300)); });
}

beforeEach(() => {
  vi.resetAllMocks();
  localStorage.clear();
  mocks.snapshot = freshSnapshot([position("Both", 2)]);
  mocks.api.hasCredentials.mockResolvedValue(true);
  mocks.api.alerts.mockResolvedValue([]);
  mocks.api.autostartEnabled.mockResolvedValue(false);
  mocks.api.setFocus.mockResolvedValue(undefined);
  mocks.api.closePreview.mockResolvedValue(plan);
  mocks.api.closePosition.mockResolvedValue(result);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
});

describe("实盘平仓按钮", () => {
  it("打开面板只做预览, 不发真实订单", async () => {
    await render();
    await openPanel();
    expect(mocks.api.closePreview).toHaveBeenCalledWith("XUSDT", "Both", 2);
    expect(mocks.api.closePosition).not.toHaveBeenCalled();
    expect(host.querySelector(".pos-close-hint")?.textContent).toContain("2.000");
    expect(host.querySelector(".pos-close-hint")?.textContent).toContain("reduceOnly");
  });

  it("确认点击只发一次, 双击不产生第二笔订单", async () => {
    let resolve!: (value: CloseResult) => void;
    mocks.api.closePosition.mockReturnValue(new Promise<CloseResult>((done) => { resolve = done; }));
    await render();
    await openPanel();
    const confirm = host.querySelector<HTMLButtonElement>(".pos-close-confirm")!;
    await act(async () => {
      confirm.click();
      confirm.click();
      confirm.click();
    });
    expect(mocks.api.closePosition).toHaveBeenCalledTimes(1);
    expect(mocks.api.closePosition).toHaveBeenCalledWith("XUSDT", "Both", 2);
    expect(confirm.disabled).toBe(true);
    await act(async () => { resolve(result); });
    expect(host.querySelector(".pos-close-panel")).toBeNull();
    expect(host.querySelector(".pos-close-msg")?.textContent).toContain("FILLED");
  });

  it("预览被后端拒绝时确认按钮不可用", async () => {
    mocks.api.closePreview.mockRejectedValue("名义值 0.10 USDT 低于最小要求 5.00");
    await render();
    await openPanel();
    expect(host.querySelector<HTMLButtonElement>(".pos-close-confirm")!.disabled).toBe(true);
    expect(host.querySelector('[role="alert"]')?.textContent).toContain("名义值");
  });

  it("发单失败保留面板并显示后端分类错误", async () => {
    mocks.api.closePosition.mockRejectedValue("HTTP 401 (code -2015) 认证或读取被拒绝");
    await render();
    await openPanel();
    await act(async () => { host.querySelector<HTMLButtonElement>(".pos-close-confirm")!.click(); });
    await act(async () => { await Promise.resolve(); });
    expect(host.querySelector(".pos-close-panel")).not.toBeNull();
    expect(host.querySelector('[role="alert"]')?.textContent).toContain("-2015");
    expect(host.querySelector<HTMLButtonElement>(".pos-close-confirm")!.disabled).toBe(false);
  });

  it("风险数据过期时入口禁用且不发预览", async () => {
    mocks.snapshot = { ...EMPTY_SNAPSHOT, revision: 1, account_generation: 1, positions: [position("Both", 2)],
      position_freshness: { last_success_ms: Date.now() - 10 * 60_000, max_age_ms: 60_000, error: null } };
    await render();
    const button = host.querySelector<HTMLButtonElement>(".pos-close-open")!;
    expect(button.disabled).toBe(true);
    expect(button.getAttribute("title")).toContain("过期");
    await act(async () => { button.click(); });
    expect(host.querySelector(".pos-close-panel")).toBeNull();
    expect(mocks.api.closePreview).not.toHaveBeenCalled();
  });

  it("平仓按钮点击不改变当前选中合约", async () => {
    mocks.snapshot = freshSnapshot([position("Both", 2), { ...position("Short", 1), symbol: "YUSDT" }]);
    await render();
    const rows = host.querySelectorAll<HTMLElement>(".pos");
    await act(async () => { rows[1].click(); });
    expect(rows[1].getAttribute("aria-pressed")).toBe("true");
    await act(async () => { rows[0].querySelector<HTMLButtonElement>(".pos-close-open")!.click(); });
    expect(rows[1].getAttribute("aria-pressed")).toBe("true");
    expect(rows[0].getAttribute("aria-pressed")).toBe("false");
  });
});
