import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT, type AlertLine, type PositionView, type Snapshot } from "./lib/types";

const mocks = vi.hoisted(() => ({
  snapshot: null as Snapshot | null,
  api: {
    hasCredentials: vi.fn(), alerts: vi.fn(), autostartEnabled: vi.fn(),
    addAlert: vi.fn(), removeAlert: vi.fn(), quit: vi.fn(), hideWindow: vi.fn(),
  },
}));
vi.mock("./lib/api", () => ({
  api: mocks.api, initBridge: async () => {}, getSnapshot: () => mocks.snapshot ?? EMPTY_SNAPSHOT,
  subscribeSnapshot: () => () => {},
}));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: vi.fn() }));
vi.mock("./components/MiniKline", () => ({ default: () => null }));
import App from "./App";

// Vitest会将CSS导入置空; 只在测试运行时载入产品样式, 不扩展前端Node类型或复制规则.
const { readFileSync } = await vi.importActual<{ readFileSync: (path: string, encoding: "utf8") => string }>("node:fs");
const { fileURLToPath } = await vi.importActual<{ fileURLToPath: (url: string) => string }>("node:url");
const styles = ["./App.css", "./components/chart-state.css"].map((path) => readFileSync(fileURLToPath(new URL(path, import.meta.url).href), "utf8")).join("\n");
let host: HTMLDivElement, root: Root, style: HTMLStyleElement;
function position(symbol: string, side: PositionView["side"]): PositionView {
  return { symbol, side, qty: 1, entry_price: 100, mark_price: 100, upnl: 0, upnl_pct: 0,
    leverage: 10, liq_price: 80, liq_distance_pct: 20, margin_ratio: null, tp_price: null, sl_price: null };
}
function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
beforeEach(() => {
  vi.resetAllMocks(); localStorage.clear();
  mocks.snapshot = { ...EMPTY_SNAPSHOT, positions: [position("XUSDT", "Long"), position("XUSDT", "Short"), position("YUSDT", "Long")] };
  mocks.api.hasCredentials.mockResolvedValue(true); mocks.api.alerts.mockResolvedValue([]);
  mocks.api.autostartEnabled.mockResolvedValue(false);
  style = document.createElement("style"); style.textContent = styles; document.head.append(style);
  host = document.createElement("div"); document.body.append(host); root = createRoot(host);
});
afterEach(async () => { await act(async () => { root.unmount(); }); host.remove(); style.remove(); });

async function render() { await act(async () => { root.render(<App />); }); }
async function openAlert() {
  const button = host.querySelector<HTMLButtonElement>(".alert-btn");
  if (button) await act(async () => { button.click(); });
}
function alertInput() { return host.querySelector<HTMLInputElement>('[placeholder="价格"]')!; }
function confirmAlert() { return host.querySelector<HTMLButtonElement>(".alert-add button")!; }
async function changePrice(value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(alertInput(), value);
    alertInput().dispatchEvent(new Event("input", { bubbles: true }));
  });
}
async function submit() { await act(async () => { confirmAlert().click(); }); }
async function select(index: number) { await act(async () => { host.querySelectorAll<HTMLElement>(".pos")[index].click(); }); }

describe("告警输入的真实组件保护", () => {
  it.each(["", " ", "abc", "0", "-1", "Infinity"])("无效价格 %s 显示可访问字段提示且不提交", async (price) => {
    await render(); await openAlert(); await changePrice(price); await submit();
    const input = alertInput();
    expect(mocks.api.addAlert).not.toHaveBeenCalled();
    expect(input.getAttribute("aria-label")).toContain("XUSDT");
    expect(input.getAttribute("aria-invalid")).toBe("true");
    const error = document.getElementById(input.getAttribute("aria-describedby")!);
    expect(error?.getAttribute("role")).toBe("alert");
    expect(error?.textContent).toContain("大于 0");
    expect(input.value).toBe(price);
  });

  it("编辑清理字段错误, 有效价格保存成功才清空", async () => {
    const pending = deferred<AlertLine[]>(); mocks.api.addAlert.mockReturnValue(pending.promise);
    await render(); await openAlert(); await changePrice("-2"); await submit();
    expect(alertInput().getAttribute("aria-invalid")).toBe("true");
    await changePrice("90");
    expect(alertInput().getAttribute("aria-invalid")).not.toBe("true");
    expect(host.querySelector('[role="alert"]')).toBeNull();
    await submit(); expect(alertInput().value).toBe("90");
    await act(async () => { pending.resolve([{ id: "new", symbol: "XUSDT", price: 90, enabled: true }]); });
    expect(host.querySelector(".alert-add input")).toBeNull();
    expect(host.querySelector(".alert-chip")?.textContent).toContain("90.000");
  });

  it("同一事件周期Enter加连续点击只提交一次, 显示忙态并禁用确认", async () => {
    const pending = deferred<AlertLine[]>(); mocks.api.addAlert.mockReturnValue(pending.promise);
    await render(); await openAlert(); await changePrice("90");
    const input = alertInput(), button = confirmAlert();
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
      button.click();
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    });
    expect(mocks.api.addAlert).toHaveBeenCalledExactlyOnceWith("XUSDT", 90);
    expect(confirmAlert().disabled).toBe(true);
    expect(host.querySelector('[aria-busy="true"]')).not.toBeNull();
    expect(host.textContent).toContain("保存中");
    await act(async () => { pending.resolve([]); });
  });

  it("保存失败保留草稿并可重试, 服务错误不冒充价格校验", async () => {
    mocks.api.addAlert.mockRejectedValueOnce("磁盘不可写").mockResolvedValueOnce([]);
    await render(); await openAlert(); await changePrice("90"); await submit();
    expect(alertInput().value).toBe("90"); expect(confirmAlert().disabled).toBe(false);
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("磁盘不可写");
    expect(alertInput().getAttribute("aria-invalid")).not.toBe("true");
    await submit();
    expect(mocks.api.addAlert).toHaveBeenCalledTimes(2);
    expect(host.querySelector(".alert-add input")).toBeNull();
  });

  it.each(["成功", "失败"])("X请求迟到%s不清除Y新草稿或显示X输入错误", async (outcome) => {
    const pending = deferred<AlertLine[]>(); mocks.api.addAlert.mockReturnValue(pending.promise);
    await render(); await openAlert(); await changePrice("90"); await submit();
    await select(2); await openAlert(); await changePrice("200");
    await act(async () => {
      if (outcome === "成功") pending.resolve([{ id: "x", symbol: "XUSDT", price: 90, enabled: true }]);
      else pending.reject("X保存失败");
    });
    expect(alertInput()?.value).toBe("200");
    expect(alertInput()?.getAttribute("aria-label")).toContain("YUSDT");
    expect(host.textContent).not.toContain("X保存失败");
    mocks.api.addAlert.mockResolvedValue([]); await submit();
    expect(mocks.api.addAlert).toHaveBeenLastCalledWith("YUSDT", 200);
  });

  it("X切Y再回X时旧X请求不能清除新X草稿", async () => {
    const pending = deferred<AlertLine[]>(); mocks.api.addAlert.mockReturnValue(pending.promise);
    await render(); await openAlert(); await changePrice("90"); await submit();
    await select(2); await select(0); await openAlert(); await changePrice("95");
    await act(async () => { pending.resolve([]); });
    expect(alertInput()?.value).toBe("95");
  });

  it("账户代次改变后迟到告警响应不覆盖新账户列表或草稿", async () => {
    const pending = deferred<AlertLine[]>(); mocks.api.addAlert.mockReturnValue(pending.promise);
    await render(); await openAlert(); await changePrice("90"); await submit();
    mocks.snapshot = { ...mocks.snapshot!, account_generation: mocks.snapshot!.account_generation + 1 };
    await render(); await openAlert(); await changePrice("95");
    await act(async () => { pending.resolve([{ id: "old-account", symbol: "XUSDT", price: 90, enabled: true }]); });
    expect(alertInput()?.value).toBe("95");
    expect(host.querySelector(".alert-chip")).toBeNull();
    await act(async () => { host.querySelector<HTMLButtonElement>('[title="设置"]')!.click(); });
    expect(host.querySelector(".all-alerts")?.textContent).toContain("暂无价格告警");
  });

  it("成功后仍允许再次提交同价告警", async () => {
    mocks.api.addAlert.mockResolvedValue([]);
    await render(); await openAlert(); await changePrice("90"); await submit();
    await openAlert(); await changePrice("90"); await submit();
    expect(mocks.api.addAlert.mock.calls).toEqual([["XUSDT", 90], ["XUSDT", 90]]);
  });

  it("删除错误保留独立提示, 不标记新价格字段无效", async () => {
    mocks.api.alerts.mockResolvedValue([{ id: "line", symbol: "XUSDT", price: 90, enabled: true }]);
    mocks.api.removeAlert.mockRejectedValue("删除失败");
    await render(); await act(async () => { host.querySelector<HTMLButtonElement>('[title="删除告警"]')!.click(); });
    await openAlert();
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("删除失败");
    expect(alertInput().getAttribute("aria-invalid")).not.toBe("true");
    expect(alertInput().getAttribute("aria-describedby")).toBeNull();
  });

  it("多告警列表与新增入口各自占位, 列表不包含新增按钮", async () => {
    mocks.api.alerts.mockResolvedValue(Array.from({ length: 12 }, (_, i) => ({ id: String(i), symbol: "XUSDT", price: 90 + i, enabled: true })));
    await render();
    const chips = host.querySelector(".alert-chips");
    expect(chips?.querySelectorAll(".alert-chip")).toHaveLength(12);
    expect(chips?.contains(host.querySelector(".alert-btn"))).toBe(false);
    await openAlert(); expect(alertInput()).not.toBeNull();
  });

  it("真实CSS约束保留风险和输入字号、24px点击区与可滚动告警列表", async () => {
    mocks.api.alerts.mockResolvedValue([{ id: "line", symbol: "XUSDT", price: 90, enabled: true }]);
    await render(); await openAlert();
    expect(getComputedStyle(host.querySelector(".pos-liq")!).fontSize).toBe("11px");
    expect(getComputedStyle(alertInput()).fontSize).toBe("11px");
    expect(getComputedStyle(alertInput()).minHeight).toBe("24px");
    for (const button of host.querySelectorAll(".seg button, .alert-add button, .alert-chip button, .ghost")) {
      expect(getComputedStyle(button).minHeight).toBe("24px");
    }
    expect(getComputedStyle(host.querySelector(".chart-head")!).flexWrap).toBe("wrap");
    expect(getComputedStyle(host.querySelector(".alert-zone")!).overflow).not.toBe("hidden");
    expect(getComputedStyle(host.querySelector(".alert-chips")!).overflowX).toBe("auto");
  });
});

describe("图表标题与设置文案", () => {
  it("方向标题来自实际选择行, 同symbol切方向及时更新", async () => {
    await render();
    expect(host.querySelector(".chart-head .badge")?.textContent).toBe("多");
    await select(1);
    expect(host.querySelector(".chart-head .sym")?.textContent).toBe("XUSDT");
    expect(host.querySelector(".chart-head .badge.short")?.textContent).toBe("空");
  });
  it("退出明确停止监控, 隐藏仍使用托盘恢复入口", async () => {
    await render(); await act(async () => { host.querySelector<HTMLButtonElement>('[title="设置"]')!.click(); });
    const quit = [...host.querySelectorAll("button")].find((button) => button.textContent === "退出并停止监控");
    expect(quit).toBeDefined();
    expect(host.querySelector('[title="隐藏 (托盘恢复)"]')).not.toBeNull();
    expect(mocks.api.quit).not.toHaveBeenCalled(); expect(mocks.api.hideWindow).not.toHaveBeenCalled();
  });
});
