import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT, type PositionView, type RiskSettings, type Snapshot } from "./lib/types";

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

beforeEach(() => {
  vi.resetAllMocks();
  localStorage.clear();
  mocks.snapshot = { ...EMPTY_SNAPSHOT, risk: { liq_warn_pct: 10, hysteresis_pct: 2 } };
  mocks.api.hasCredentials.mockResolvedValue(true);
  mocks.api.alerts.mockResolvedValue([]);
  mocks.api.autostartEnabled.mockResolvedValue(false);
  mocks.api.setFocus.mockResolvedValue(undefined);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
});

async function renderSettings() {
  await act(async () => { root.render(<App />); });
  await act(async () => { host.querySelector<HTMLButtonElement>('[title="设置"]')!.click(); });
}

function riskInput() {
  return host.querySelector<HTMLInputElement>('[aria-label="强平告警阈值"]')!;
}

async function changeInput(input: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function blurRisk() {
  await act(async () => { riskInput().dispatchEvent(new FocusEvent("focusout", { bubbles: true })); });
}

async function setRiskDraft(value: string) {
  await changeInput(riskInput(), value);
  await blurRisk();
}

function position(side: PositionView["side"], qty: number): PositionView {
  return {
    symbol: "XUSDT", side, qty, entry_price: 100, mark_price: 90,
    upnl: 20, upnl_pct: 100, leverage: 10, liq_price: 120, liq_distance_pct: 100 / 3,
    margin_ratio: null, tp_price: 80, sl_price: 110,
  };
}

describe("强平阈值真实组件", () => {
  it.each(["", "0", "100", "-1", "0.5", "50.1"])("无效草稿 %s 不提交且不显示成功", async (value) => {
    await renderSettings();
    await setRiskDraft(value);
    expect(mocks.api.setRisk).not.toHaveBeenCalled();
    expect(riskInput().value).toBe(value);
    expect(riskInput().getAttribute("aria-invalid")).toBe("true");
    expect(host.querySelector('[role="alert"]')?.textContent).toContain("1-50");
    expect(host.querySelector(".ok-msg")).toBeNull();
  });

  it.each([1, 50, 12.5])("合法阈值 %s 提交并确认", async (value) => {
    mocks.api.setRisk.mockResolvedValue({ liq_warn_pct: value, hysteresis_pct: 2 });
    await renderSettings();
    await setRiskDraft(String(value));
    expect(mocks.api.setRisk).toHaveBeenCalledExactlyOnceWith(value);
    expect(riskInput().value).toBe(String(value));
    expect(host.querySelector(".ok-msg")?.textContent).toBe(`强平距离低于 ${value}% 时通知`);
    expect(host.querySelector('[role="alert"]')).toBeNull();
  });

  it("显示后端确认值而非请求草稿, 不舍入", async () => {
    mocks.api.setRisk.mockResolvedValue({ liq_warn_pct: 12.25, hysteresis_pct: 2 });
    await renderSettings();
    await setRiskDraft("12");
    expect(mocks.api.setRisk).toHaveBeenCalledExactlyOnceWith(12);
    expect(riskInput().value).toBe("12.25");
    expect(host.querySelector(".ok-msg")?.textContent).toBe("强平距离低于 12.25% 时通知");
  });

  it("持久化失败保留可修正草稿, 不显示成功, 再次失焦可重试", async () => {
    mocks.api.setRisk.mockRejectedValueOnce("保存设置失败: 磁盘不可写")
      .mockResolvedValueOnce({ liq_warn_pct: 20, hysteresis_pct: 2 });
    await renderSettings();
    await setRiskDraft("20");
    expect(riskInput().value).toBe("20");
    expect(riskInput().disabled).toBe(false);
    expect(host.querySelector('[role="alert"]')?.textContent).toContain("磁盘不可写");
    expect(host.querySelector(".ok-msg")).toBeNull();
    await blurRisk();
    expect(mocks.api.setRisk).toHaveBeenCalledTimes(2);
    expect(host.querySelector('[role="alert"]')).toBeNull();
    expect(host.querySelector(".ok-msg")?.textContent).toContain("20%");
  });

  it("相同有效值不再次提交或重新武装", async () => {
    await renderSettings();
    await setRiskDraft("10.0");
    await blurRisk();
    expect(mocks.api.setRisk).not.toHaveBeenCalled();
    expect(riskInput().value).toBe("10");
    expect(host.querySelector('[role="alert"]')).toBeNull();
  });

  it("保存等待期间禁止重入, 只有确认后显示成功", async () => {
    let resolve!: (risk: RiskSettings) => void;
    mocks.api.setRisk.mockReturnValue(new Promise<RiskSettings>((done) => { resolve = done; }));
    await renderSettings();
    await setRiskDraft("20");
    expect(riskInput().disabled).toBe(true);
    expect(host.querySelector(".ok-msg")).toBeNull();
    await blurRisk();
    expect(mocks.api.setRisk).toHaveBeenCalledTimes(1);
    await act(async () => { resolve({ liq_warn_pct: 20, hysteresis_pct: 2 }); });
    expect(riskInput().disabled).toBe(false);
    expect(riskInput().value).toBe("20");
    await setRiskDraft("20.0");
    expect(mocks.api.setRisk).toHaveBeenCalledTimes(1);
  });

  it("重新挂载读取已确认快照阈值", async () => {
    mocks.snapshot = { ...EMPTY_SNAPSHOT, risk: { liq_warn_pct: 27.5, hysteresis_pct: 2 } };
    await renderSettings();
    expect(riskInput().value).toBe("27.5");
    expect(mocks.api.setRisk).not.toHaveBeenCalled();
  });
});

describe("持仓方向与告警错误", () => {
  it.each([
    ["Both", -2, "空", "short"], ["Both", 2, "多", "long"],
    ["Long", 2, "多", "long"], ["Short", 2, "空", "short"],
  ] as const)("%s 数量 %s 显示 %s", async (side, qty, label, cls) => {
    mocks.snapshot = { ...EMPTY_SNAPSHOT, positions: [position(side, qty)] };
    await act(async () => { root.render(<App />); });
    expect(host.querySelector(`.badge.${cls}`)?.textContent).toBe(`${label}10x`);
    expect(host.querySelector(".pos-r b")?.textContent).toBe("+20.00");
    expect(host.querySelector(".pos-r span")?.textContent).toBe("100.0%");
  });

  it("新增告警存储失败可见并保留输入", async () => {
    mocks.snapshot = { ...EMPTY_SNAPSHOT, positions: [position("Both", -2)] };
    mocks.api.addAlert.mockRejectedValue("保存设置失败");
    await act(async () => { root.render(<App />); });
    await act(async () => { host.querySelector<HTMLButtonElement>(".alert-btn")!.click(); });
    const input = host.querySelector<HTMLInputElement>('[placeholder="价格"]')!;
    await changeInput(input, "90");
    await act(async () => { host.querySelector<HTMLButtonElement>(".alert-add button")!.click(); });
    expect(mocks.api.addAlert).toHaveBeenCalledExactlyOnceWith("XUSDT", 90);
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("保存设置失败");
    expect(host.querySelector<HTMLInputElement>('[placeholder="价格"]')?.value).toBe("90");
    expect(host.querySelector(".alert-chip")).toBeNull();
  });

  it("删除告警存储失败可见且保留原告警", async () => {
    mocks.snapshot = { ...EMPTY_SNAPSHOT, positions: [position("Both", -2)] };
    mocks.api.alerts.mockResolvedValue([{ id:"line", symbol:"XUSDT", price:90, enabled:true }]);
    mocks.api.removeAlert.mockRejectedValue("保存设置失败");
    await act(async () => { root.render(<App />); });
    await act(async () => { host.querySelector<HTMLButtonElement>('[title="删除告警"]')!.click(); });
    expect(mocks.api.removeAlert).toHaveBeenCalledExactlyOnceWith("line");
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("保存设置失败");
    expect(host.querySelector(".alert-chip")?.textContent).toContain("90.000");
  });
});
