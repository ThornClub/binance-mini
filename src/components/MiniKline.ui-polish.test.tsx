import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_CHART, EMPTY_SNAPSHOT, type ChartView, type Snapshot } from "../lib/types";

const mock = vi.hoisted(() => ({
  invoke: vi.fn(), events: new Map<string, (e: { payload: Snapshot }) => void>(),
  elements: [] as HTMLElement[], applyOptions: vi.fn(), remove: vi.fn(), fit: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mock.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name, fn) => { mock.events.set(name, fn); return () => mock.events.delete(name); }) }));
vi.mock("lightweight-charts", () => ({
  AreaSeries: {}, CandlestickSeries: {}, CrosshairMode: { Normal: 0 }, LineStyle: { Dotted: 1, Dashed: 2, Solid: 0 },
  createChart: (element: HTMLElement) => {
    mock.elements.push(element);
    return {
      addSeries: () => ({ setData: vi.fn(), update: vi.fn(), createPriceLine: vi.fn(), removePriceLine: vi.fn() }),
      applyOptions: mock.applyOptions, remove: mock.remove, removeSeries: vi.fn(), subscribeCrosshairMove: vi.fn(),
      timeScale: () => ({ fitContent: mock.fit, getVisibleLogicalRange: () => null, setVisibleLogicalRange: vi.fn() }),
    };
  },
}));
import MiniKline from "./MiniKline";
import { initBridge } from "../lib/api";

let host: HTMLDivElement, root: Root, view: ChartView;
let generation = 0, revision = 0, failHistory = false;
let resize!: () => void, observed: Element | null;
function snapshot(): Snapshot { return { ...EMPTY_SNAPSHOT, revision: ++revision, account_generation: generation, chart: view }; }
function deliver() { mock.events.get("state://snapshot")!({ payload: snapshot() }); }
beforeEach(async () => {
  vi.useFakeTimers(); vi.setSystemTime(1_000_000); vi.resetAllMocks();
  mock.elements.length = 0; generation++; failHistory = false; observed = null; view = { ...EMPTY_CHART };
  vi.stubGlobal("ResizeObserver", class {
    constructor(callback: () => void) { resize = callback; }
    observe(element: Element) { observed = element; }
    disconnect() {}
  });
  vi.stubGlobal("requestAnimationFrame", (fn: FrameRequestCallback) => setTimeout(() => fn(0), 0));
  vi.stubGlobal("cancelAnimationFrame", clearTimeout);
  mock.invoke.mockImplementation(async (name, args) => {
    if (name === "get_snapshot") return snapshot();
    if (name === "clear_focus") return;
    if (name === "set_focus") {
      view = { ...EMPTY_CHART,
        token: { account_generation: generation, chart_generation: generation, symbol: args.symbol, interval: args.interval },
        reconnect_epoch: 1, status: "live", connected: true, last_live_ms: Date.now(),
        freshness: { last_success_ms: Date.now(), max_age_ms: 18000, error: null } };
      deliver(); return view.token;
    }
    if (name === "get_klines") {
      if (failHistory) throw new Error("历史恢复失败: 详细诊断");
      return { token: args.token, reconnect_epoch: args.reconnectEpoch, request_id: args.requestId,
        rows: [{ t: 60000, o: 100, h: 120, l: 80, c: 100, trades: 1, closed: false }] };
    }
    throw new Error(`未模拟命令 ${name}`);
  });
  await initBridge(); deliver();
  host = document.createElement("div"); document.body.append(host); root = createRoot(host);
  await act(async () => { root.render(<MiniKline symbol="XUSDT" interval="1h" lines={[]} />); });
  await act(async () => { await vi.advanceTimersByTimeAsync(1); });
});
afterEach(async () => {
  await act(async () => { root.unmount(); }); host.remove(); vi.unstubAllGlobals(); vi.useRealTimers();
});

describe("图表紧凑状态呈现", () => {
  it("正常状态是绘图区之前的紧凑头部, 不在Canvas覆盖层", () => {
    const status = host.querySelector(".chart-status"), plot = host.querySelector(".chart-plot");
    expect(plot).not.toBeNull();
    expect(status?.textContent).toBe("实时");
    expect(status?.parentElement).toBe(plot?.parentElement);
    expect(status?.nextElementSibling).toBe(plot);
    expect(plot?.contains(status)).toBe(false);
    expect(plot?.contains(mock.elements[0])).toBe(true);
    expect(host.querySelector(".chart-status details")).toBeNull();
  });

  it("ResizeObserver只观测真实绘图区, 状态区域变化后按其实际尺寸调整", async () => {
    const plot = host.querySelector(".chart-plot");
    expect(plot).not.toBeNull();
    expect(observed).toBe(mock.elements[0]); expect(plot?.contains(observed)).toBe(true);
    Object.defineProperties(mock.elements[0], { clientWidth: { value: 265, configurable: true }, clientHeight: { value: 73, configurable: true } });
    await act(async () => { resize(); await vi.advanceTimersByTimeAsync(1); });
    expect(mock.applyOptions).toHaveBeenLastCalledWith({ width: 265, height: 73 });
    expect(mock.fit).toHaveBeenCalledTimes(1);
  });

  it("异常摘要保留明确合约, 详情默认收起且可展开, 重试走原会话恢复", async () => {
    failHistory = true;
    await act(async () => { view = { ...view, reconnect_epoch: 2, status: "recovering" }; deliver(); });
    expect(host.querySelector(".chart-status-label")?.textContent).toBe("XUSDT加载失败");
    const details = host.querySelector<HTMLDetailsElement>(".chart-status details");
    expect(details).not.toBeNull(); expect(details?.open).toBe(false);
    expect(details?.textContent).toContain("历史恢复失败: 详细诊断");
    await act(async () => { details!.querySelector("summary")!.click(); });
    expect(details?.open).toBe(true);
    failHistory = false;
    await act(async () => { host.querySelector<HTMLButtonElement>(".chart-status button")!.click(); });
    expect(host.querySelector(".chart-status-label")?.textContent).toContain("定时刷新");
    expect(host.textContent).not.toContain("历史恢复失败");
    // REST回包不伪造WS状态, 仍需有效推送才能恢复实时标签.
    await act(async () => {
      view = { ...view, status: "live", live_seq: 1, last_live_ms: Date.now(),
        candle: { t: 60000, o: 100, h: 120, l: 80, c: 101, trades: 2, closed: false } };
      deliver();
    });
    expect(host.querySelector(".chart-status")?.textContent).toBe("实时");
    expect(mock.invoke.mock.calls.filter(([name]) => name === "get_klines")).toHaveLength(3);
    expect(mock.fit).toHaveBeenCalledTimes(1);
  });

  it("REST备用与重连保持短摘要和连接详情, 不冒称实时", async () => {
    await act(async () => {
      view = { ...view, connected: false, status: "polling", last_live_ms: 0, connection_error: "握手失败: HTTP 403" }; deliver();
    });
    expect(host.querySelector(".chart-status-label")?.textContent).toContain("定时刷新");
    expect(host.querySelector(".chart-status-label")?.textContent).toContain("正在重连");
    expect(host.querySelector(".chart-status details")?.textContent).toContain("HTTP 403");
    expect(host.querySelector(".chart-status-label")?.textContent).not.toContain("过期");
  });
});
