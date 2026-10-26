import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
const mocks = vi.hoisted(() => ({ setFocus: vi.fn(), clearFocus: vi.fn(), klines: vi.fn(), priceLine: vi.fn() }));
vi.mock("../lib/api", () => ({ api: mocks }));
vi.mock("lightweight-charts", () => ({
  AreaSeries: {}, CandlestickSeries: {}, CrosshairMode: { Normal: 0 }, LineStyle: { Dotted: 1, Dashed: 2, Solid: 0 },
  createChart: () => ({
    addSeries: () => ({ setData: vi.fn(), update: vi.fn(), createPriceLine: mocks.priceLine, removePriceLine: vi.fn() }),
    remove: vi.fn(), removeSeries: vi.fn(), applyOptions: vi.fn(), subscribeCrosshairMove: vi.fn(),
    timeScale: () => ({ getVisibleLogicalRange: () => null, fitContent: vi.fn() }),
  }),
}));
import MiniKline from "./MiniKline";
it("同symbol只换多空价格线不重订阅或重取历史", async () => {
  vi.useFakeTimers();
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("requestAnimationFrame", (fn: FrameRequestCallback) => setTimeout(() => fn(0), 0));
  vi.stubGlobal("cancelAnimationFrame", clearTimeout);
  const token = { account_generation: 1, chart_generation: 1, symbol: "X", interval: "1h" };
  mocks.setFocus.mockResolvedValue(token); mocks.clearFocus.mockResolvedValue(undefined);
  mocks.klines.mockImplementation(async (_token, epoch, request) => ({ token, reconnect_epoch: epoch, request_id: request,
    rows: [{ t: 60000, o: 100, h: 110, l: 90, c: 100, trades: 1, closed: false }] }));
  const host = document.createElement("div"); const root = createRoot(host);
  try {
    await act(async () => { root.render(<MiniKline symbol="X" interval="1h" lines={[{ price: 100, color: "red", title: "开" }]} />); });
    await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    await act(async () => { root.render(<MiniKline symbol="X" interval="1h" lines={[{ price: 200, color: "red", title: "开" }]} />); });
    expect(mocks.setFocus).toHaveBeenCalledExactlyOnceWith("X", "1h");
    expect(mocks.klines).toHaveBeenCalledTimes(1);
    expect(mocks.priceLine).toHaveBeenLastCalledWith(expect.objectContaining({ price: 200, title: "开" }));
  } finally {
    await act(async () => { root.unmount(); });
    vi.unstubAllGlobals(); vi.useRealTimers();
  }
});
