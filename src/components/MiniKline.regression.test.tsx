import { StrictMode, act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Candle, ChartHistory, ChartToken, ChartView, Snapshot } from "../lib/types";
import { EMPTY_CHART, EMPTY_SNAPSHOT } from "../lib/types";

const mock = vi.hoisted(() => ({
  invoke: vi.fn(), events: new Map<string, (e: { payload: Snapshot }) => void>(),
  charts: [] as Array<{ remove: ReturnType<typeof vi.fn>; fit: ReturnType<typeof vi.fn> }>,
  series: [] as Array<{ data: Array<{ time: number; value?: number; close?: number }>; setData: ReturnType<typeof vi.fn>; update: ReturnType<typeof vi.fn> }>,
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mock.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name, fn) => { mock.events.set(name, fn); return () => mock.events.delete(name); }) }));
vi.mock("lightweight-charts", () => ({
  AreaSeries: {}, CandlestickSeries: {}, CrosshairMode: { Normal: 0 }, LineStyle: { Dotted: 1, Dashed: 2, Solid: 0 },
  createChart: () => {
    const chart = { remove: vi.fn(), fit: vi.fn() }; mock.charts.push(chart);
    return {
      addSeries: () => {
        const series = { data: [] as Array<{ time: number; value?: number; close?: number }>, setData: vi.fn(), update: vi.fn(), createPriceLine: vi.fn(), removePriceLine: vi.fn() };
        series.setData.mockImplementation((rows) => { series.data = [...rows]; });
        series.update.mockImplementation((row) => {
          const index = series.data.findIndex((c) => c.time === row.time);
          if (index >= 0) series.data[index] = row; else series.data.push(row);
        });
        mock.series.push(series); return series;
      },
      applyOptions: vi.fn(), removeSeries: vi.fn(), remove: chart.remove, subscribeCrosshairMove: vi.fn(),
      timeScale: () => ({ fitContent: chart.fit, getVisibleLogicalRange: () => null, setVisibleLogicalRange: vi.fn() }),
    };
  },
}));
import MiniKline, { type ChartInterval } from "./MiniKline";
import { initBridge } from "../lib/api";

function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const candle = (t = 60_000, trades = 1, c = 100): Candle => ({ t, o: 100, h: 120, l: 80, c, trades, closed: false });
let host: HTMLDivElement, root: Root;
let generation = 0, revision = 0, chartGeneration = 0;
let token: ChartToken;
let view: ChartView;
let history: (args: { token: ChartToken; reconnectEpoch: number; requestId: number; limit: number }) => Promise<ChartHistory>;
function snapshot(chart = view): Snapshot { return { ...EMPTY_SNAPSHOT, revision: ++revision, account_generation: generation, chart }; }
function deliver(chart = view) { mock.events.get("state://snapshot")!({ payload: snapshot(chart) }); }
function live(c: Candle) {
  view = { ...view, candle: c, live_seq: view.live_seq + 1, last_live_ms: Date.now(), status: "live",
    freshness: { last_success_ms: Date.now(), max_age_ms: 18000, error: null } };
  deliver();
}
function requests() { return mock.invoke.mock.calls.filter(([name]) => name === "get_klines"); }
function activeSeries() { return mock.series[mock.series.length - 1]; }
async function render(symbol = "AUSDT", interval: ChartInterval = "time", strict = false) {
  const ui = <MiniKline symbol={symbol} interval={interval} lines={[]} />;
  await act(async () => { root.render(strict ? <StrictMode>{ui}</StrictMode> : ui); });
  await act(async () => { await vi.advanceTimersByTimeAsync(1); });
}
beforeEach(async () => {
  vi.useFakeTimers(); vi.setSystemTime(1_000_000);
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("requestAnimationFrame", (fn: FrameRequestCallback) => setTimeout(() => fn(0), 0));
  vi.stubGlobal("cancelAnimationFrame", clearTimeout);
  mock.invoke.mockReset(); mock.charts.length = 0; mock.series.length = 0;
  generation++;
  view = { ...EMPTY_CHART };
  history = async (args) => ({ token: args.token, reconnect_epoch: args.reconnectEpoch, request_id: args.requestId, rows: [candle()] });
  mock.invoke.mockImplementation(async (name, args) => {
    if (name === "get_snapshot") return snapshot();
    if (name === "set_focus") {
      token = { account_generation: generation, chart_generation: ++chartGeneration, symbol: args.symbol, interval: args.interval };
      view = { ...EMPTY_CHART, token, reconnect_epoch: 1, connected: true, status: "live", last_live_ms: Date.now(), refresh_interval_ms: 5000,
        freshness: { last_success_ms: Date.now(), max_age_ms: 18000, error: null } };
      deliver(); return token;
    }
    if (name === "clear_focus") return;
    if (name === "get_klines") return history(args);
    throw new Error(`未模拟命令 ${name}`);
  });
  await initBridge(); deliver();
  host = document.createElement("div"); document.body.append(host); root = createRoot(host);
});
afterEach(async () => {
  await act(async () => { root.unmount(); }); host.remove();
  vi.unstubAllGlobals(); vi.useRealTimers();
});

describe("真实MiniKline与IPC入口", () => {
  it("A切B且B历史未完成时立即清图和OHLC, B失败显示B错误与重试", async () => {
    await render(); expect(activeSeries().data[0].value).toBe(100);
    const pending = deferred<ChartHistory>(); history = () => pending.promise;
    await render("BUSDT"); expect(activeSeries().data).toEqual([]); expect(host.querySelector(".ohlc")).toBeNull();
    await act(async () => { pending.reject("B网络失败"); });
    expect(host.textContent).toContain("BUSDT加载失败"); expect(host.textContent).toContain("重试图表");
  });
  it.each(["time", "15m", "1h", "1d"] as const)("%s同根更新与新根都增量可见, 不反复fit", async (interval) => {
    await render("AUSDT", interval);
    const fit = mock.charts[0].fit.mock.calls.length;
    await act(async () => { live(candle(60_000, 2, 101)); live(candle(120_000, 1, 102)); });
    expect(activeSeries().update).toHaveBeenCalledTimes(2);
    expect(activeSeries().data.map((c) => c.value ?? c.close)).toEqual([101, 102]);
    expect(mock.charts[0].fit).toHaveBeenCalledTimes(fit);
    expect(requests()[0][1].limit).toBe(interval === "time" ? 180 : interval === "1d" ? 90 : 120);
  });
  it("历史等待期间React批处理多根live不丢中间根, 较旧历史不覆盖live", async () => {
    const pending = deferred<ChartHistory>(); let args!: Parameters<typeof history>[0];
    history = (input) => { args = input; return pending.promise; };
    await render();
    await act(async () => { live(candle(60_000, 10, 105)); live(candle(120_000, 2, 106)); live(candle(180_000, 1, 107)); });
    await act(async () => { pending.resolve({ token: args.token, reconnect_epoch: args.reconnectEpoch, request_id: args.requestId, rows: [candle(60_000, 1, 99)] }); });
    expect(activeSeries().data.map((c) => c.value)).toEqual([105, 106, 107]);
  });
  it("A->B->A拒绝旧history/live/error, 迟到cleanup仍携带原token", async () => {
    const old = deferred<ChartHistory>(); let oldArgs!: Parameters<typeof history>[0];
    history = (args) => { oldArgs = args; return old.promise; };
    await render(); const oldToken = token; const oldView = view;
    history = async (args) => ({ token: args.token, reconnect_epoch: args.reconnectEpoch, request_id: args.requestId, rows: [candle(60_000, 3, 110)] });
    await render("BUSDT"); await render("AUSDT");
    expect(token.chart_generation).not.toBe(oldToken.chart_generation);
    await act(async () => {
      deliver({ ...oldView, candle: candle(120_000, 50, 90), status: "error", freshness: { last_success_ms: 1, max_age_ms: 5000, error: "旧A失败" } });
      old.resolve({ token: oldArgs.token, reconnect_epoch: oldArgs.reconnectEpoch, request_id: oldArgs.requestId, rows: [candle(60_000, 100, 80)] });
    });
    expect(activeSeries().data.map((c) => c.value)).toEqual([110]); expect(host.textContent).not.toContain("旧A失败");
    expect(mock.invoke).toHaveBeenCalledWith("clear_focus", { token: oldToken });
  });
  it("重连补齐多根缺口, 失败保持陈旧, 显式重试恢复", async () => {
    await render();
    history = async () => { throw new Error("补洞失败"); };
    await act(async () => { view = { ...view, reconnect_epoch: 2, status: "recovering" }; deliver(); });
    expect(activeSeries().data.length).toBe(1); expect(host.querySelector(".data-stale")).not.toBeNull();
    expect(host.textContent).toContain("补洞失败");
    history = async (args) => ({ token: args.token, reconnect_epoch: args.reconnectEpoch, request_id: args.requestId, rows: [candle(), candle(120_000), candle(180_000)] });
    await act(async () => { host.querySelector<HTMLButtonElement>(".chart-status button")!.click(); });
    expect(activeSeries().data.map((c) => c.time)).toEqual([60, 120, 180]); expect(host.textContent).not.toContain("补洞失败");
    expect(requests().map(([, args]) => args.reconnectEpoch)).toEqual([1, 2, 2]);
  });
  it("健康推送超过20秒不拉全量历史, 断流无快照也更新陈旧提示", async () => {
    await render(); const initial = requests().length;
    for (let i = 0; i < 25; i++) await act(async () => { live(candle(60_000, i + 2)); await vi.advanceTimersByTimeAsync(1000); });
    expect(requests()).toHaveLength(initial);
    history = () => new Promise(() => {});
    await act(async () => { await vi.advanceTimersByTimeAsync(19000); });
    expect(host.querySelector(".data-stale")).not.toBeNull();
    expect(requests()).toHaveLength(initial + 1);
  });
  it("REST刚更新且WS断开时显示定时刷新和真实连接错误, 超预算才过期", async () => {
    await render();
    history = () => new Promise(() => {});
    await act(async () => {
      view = { ...view, connected: false, status: "polling", last_live_ms: 0, connection_error: "WS握手被拒绝: HTTP 403" };
      deliver();
      await vi.advanceTimersByTimeAsync(2000);
    });
    expect(host.textContent).toContain("定时刷新");
    expect(host.textContent).toContain("HTTP 403");
    expect(host.textContent).not.toContain("数据已过期");
    expect(host.querySelector(".data-stale")).toBeNull();
    await act(async () => { await vi.advanceTimersByTimeAsync(17000); });
    expect(host.textContent).toContain("数据已过期");
  });
  it("握手但无有效帧不能标实时正常", async () => {
    await render();
    await act(async () => { view = { ...view, status: "recovering", last_live_ms: 0 }; deliver(); });
    expect(host.textContent).toContain("定时刷新");
    expect(host.textContent).toContain("等待有效推送");
    expect(host.querySelector(".data-stale")).toBeNull();
    await act(async () => { live(candle(60000, 2)); });
    expect(host.querySelector(".chart-status")?.textContent).toBe("实时");
  });
  it("StrictMode只保留一个图表实例, 无销毁实例重绘, 卸载清理订阅", async () => {
    await render("AUSDT", "time", true);
    expect(mock.charts).toHaveLength(1);
    expect(mock.charts[0].remove).not.toHaveBeenCalled();
    await act(async () => { root.render(<></>); });
    expect(mock.charts[0].remove).toHaveBeenCalledTimes(1);
    const updates = activeSeries().update.mock.calls.length;
    await act(async () => { live(candle(120_000)); });
    expect(activeSeries().update).toHaveBeenCalledTimes(updates);
  });
});
