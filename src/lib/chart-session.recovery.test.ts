import { afterEach, describe, expect, it, vi } from "vitest";
import { ChartSession, chartFeed } from "./chart-session";
import { EMPTY_CHART, type ChartHistory, type ChartToken } from "./types";

let account = 400;
function setup() {
  const token: ChartToken = { account_generation: ++account, chart_generation: 1, symbol: "X", interval: "1m" };
  const pending: Array<{ epoch: number; request: number; resolve: (v: ChartHistory) => void; reject: (e: unknown) => void }> = [];
  const api = {
    setFocus: vi.fn(async () => token), clearFocus: vi.fn(async () => {}),
    klines: vi.fn((_token: ChartToken, epoch: number, request: number) => new Promise<ChartHistory>((resolve, reject) => pending.push({ epoch, request, resolve, reject }))),
  };
  const view = { ...EMPTY_CHART, token, reconnect_epoch: 1, refresh_interval_ms: 5000,
    freshness: { last_success_ms: 0, max_age_ms: 18000, error: null } };
  chartFeed.ingest(view, account, 1);
  return { token, pending, api, view, session: new ChartSession(api, "X", "1m") };
}
afterEach(() => vi.useRealTimers());
describe("图表恢复请求所有权", () => {
  it("同token/epoch的失败握手通知与重试共用8秒历史请求", async () => {
    vi.useFakeTimers(); vi.setSystemTime(100_000);
    const { token, pending, api, view, session } = setup();
    try {
      await session.start();
      for (const delay of [2000, 4000]) {
        await vi.advanceTimersByTimeAsync(delay);
        chartFeed.ingest({ ...view, status: "recovering" }, account, Date.now());
        session.retry();
        expect(api.klines).toHaveBeenCalledTimes(1);
      }
      await vi.advanceTimersByTimeAsync(2000);
      pending[0].resolve({ token, reconnect_epoch: 1, request_id: pending[0].request,
        rows: [{ t: 60000, o: 100, h: 110, l: 90, c: 105, trades: 1, closed: false }] });
      await Promise.resolve();
      expect(session.get().rows).toHaveLength(1);
      expect(session.get().loading).toBe(false);
      await vi.advanceTimersByTimeAsync(6000);
      chartFeed.ingest({ ...view, status: "recovering" }, account, Date.now());
      expect(api.klines).toHaveBeenCalledTimes(2);
    } finally { session.dispose(); }
  });
  it("补洞失败后持续有效推送不能阻止历史重试", async () => {
    vi.useFakeTimers(); vi.setSystemTime(100_000);
    const { pending, api, view, session } = setup();
    try {
      await session.start();
      pending[0].reject("补洞失败");
      await Promise.resolve();
      for (let second = 0; second < 5; second++) {
        chartFeed.ingest({ ...view, connected: true, last_live_ms: Date.now(), status: "stale",
          freshness: { ...view.freshness, last_success_ms: Date.now(), error: "补洞失败" } }, account, second + 2);
        await vi.advanceTimersByTimeAsync(1000);
      }
      expect(api.klines).toHaveBeenCalledTimes(2);
      expect(session.get().loading).toBe(true);
    } finally { session.dispose(); }
  });
  it("旧epoch finally不能推迟新epoch失败后的重试", async () => {
    vi.useFakeTimers(); vi.setSystemTime(100_000);
    const { pending, api, view, session } = setup();
    try {
      await session.start();
      chartFeed.ingest({ ...view, reconnect_epoch: 2 }, account, 2);
      pending[1].reject("新epoch历史失败");
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(4000);
      pending[0].reject("旧epoch迟到失败");
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(2000);
      expect(api.klines).toHaveBeenCalledTimes(3);
      expect(pending[2].epoch).toBe(2);
    } finally { session.dispose(); }
  });
});
