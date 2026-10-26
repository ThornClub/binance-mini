import { describe, expect, it, vi } from "vitest";
import { ChartSession, chartFeed, mergeCandles, validCandle } from "./chart-session";
import { EMPTY_CHART, type Candle, type ChartHistory, type ChartToken } from "./types";
const row = (t = 60000, trades = 10, closed = false, c = 100): Candle => ({ t, trades, closed, c, o: 100, h: 120, l: 80 });
describe("图表跨REST/WS版本契约", () => {
  it("旧历史不覆盖live, 终结态只能前进", () => {
    const live = row(60000, 10, true, 110);
    for (const history of [row(60000, 9, true, 90), row(60000, 10, false, 90), row(60000, 11, false, 90)]) {
      expect(mergeCandles([live], [history], 180)).toEqual([live]);
    }
    expect(mergeCandles([row()], [row(60000, 10, true)], 180)[0].closed).toBe(true);
    expect(mergeCandles([row()], [row(60000, 11, false, 111)], 180)[0].c).toBe(111);
  });
  it("乱序批量输入合并为有界窗口, 无效行不污染数据", () => {
    const rows = Array.from({ length: 300 }, (_, n) => row((n + 1) * 60000));
    const result = mergeCandles([], rows.reverse(), 90);
    expect(result).toHaveLength(90); expect(result[0].t).toBe(211 * 60000);
    expect(mergeCandles(result, [{ ...row(), c: NaN }], 90)).toEqual(result);
    expect(validCandle({ ...row(), h: 50 })).toBe(false);
    expect(validCandle({ ...row(), trades: -1 })).toBe(false);
  });
  it("新epoch恢复完成后旧epoch历史失败不能覆盖成功状态", async () => {
    const token: ChartToken = { account_generation: 100, chart_generation: 1, symbol: "X", interval: "1h" };
    const pending: Array<{ resolve: (value: ChartHistory) => void; reject: (e: unknown) => void; epoch: number; request: number }> = [];
    const api = {
      setFocus: vi.fn(async () => token), clearFocus: vi.fn(async () => {}),
      klines: vi.fn((_token: ChartToken, epoch: number, request: number) => new Promise<ChartHistory>((resolve, reject) => pending.push({ resolve, reject, epoch, request }))),
    };
    const session = new ChartSession(api, "X", "1h");
    const view = { ...EMPTY_CHART, token, reconnect_epoch: 1, connected: true, last_live_ms: Date.now(), refresh_interval_ms: 5000,
      freshness: { last_success_ms: Date.now(), max_age_ms: 18000, error: null } };
    chartFeed.ingest(view, 100, 1);
    try {
      await session.start();
      chartFeed.ingest({ ...view, reconnect_epoch: 2 }, 100, 2);
      expect(pending).toHaveLength(2);
      pending[1].resolve({ token, reconnect_epoch: 2, request_id: pending[1].request, rows: [row()] });
      await Promise.resolve();
      pending[0].reject(new Error("旧epoch失败"));
      await Promise.resolve();
      expect(session.get().rows).toEqual([row()]);
      expect(session.get().loading).toBe(false); expect(session.get().error).toBeNull();
    } finally { session.dispose(); }
    expect(api.clearFocus).toHaveBeenCalledWith(token);
  });
  it("set_focus迟到时已销毁会话只条件退订, 不再读取历史", async () => {
    let resolve!: (token: ChartToken) => void;
    const token: ChartToken = { account_generation: 101, chart_generation: 2, symbol: "A", interval: "1m" };
    const api = { setFocus: vi.fn(() => new Promise<ChartToken>((done) => { resolve = done; })),
      clearFocus: vi.fn(async () => {}), klines: vi.fn() };
    const session = new ChartSession(api, "A", "1m");
    const start = session.start(); session.dispose(); resolve(token); await start;
    expect(api.clearFocus).toHaveBeenCalledWith(token); expect(api.klines).not.toHaveBeenCalled();
  });
  it.each([
    ["1m", 60_000, 200], ["15m", 900_000, 201], ["1h", 3_600_000, 202], ["1d", 86_400_000, 203],
  ] as const)("%s握手后新epoch补齐上一根最终版本", async (interval, width, account) => {
    const token: ChartToken = { account_generation: account, chart_generation: 1, symbol: "X", interval };
    const early = row(width, 1, false, 100);
    const closed = row(width, 8, true, 109);
    const next = row(width * 2, 2, false, 108);
    const api = {
      setFocus: vi.fn(async () => token), clearFocus: vi.fn(async () => {}),
      klines: vi.fn(async (_token: ChartToken, epoch: number, request: number) => ({
        token, reconnect_epoch: epoch, request_id: request,
        rows: epoch === 1 ? [early] : [closed, row(width * 2, 1)],
      })),
    };
    const view = { ...EMPTY_CHART, token, reconnect_epoch: 1, connected: false, status: "recovering" as const, refresh_interval_ms: 5000,
      freshness: { last_success_ms: Date.now(), max_age_ms: 18000, error: null } };
    chartFeed.ingest(view, account, 1);
    const session = new ChartSession(api, "X", interval);
    try {
      await session.start();
      await Promise.resolve();
      expect(session.get().rows).toEqual([early]);
      const connected = { ...view, reconnect_epoch: 2, connected: true };
      chartFeed.ingest(connected, account, 2);
      chartFeed.ingest({ ...connected, candle: next, live_seq: 1 }, account, 3);
      expect(session.get().loading).toBe(true);
      await Promise.resolve();
      expect(api.klines).toHaveBeenCalledTimes(2);
      expect(session.get().rows).toEqual([closed, next]);
      chartFeed.ingest({ ...connected, status: "live", candle: next, live_seq: 1 }, account, 4);
      expect(session.get().view?.status).toBe("live");
      expect(session.get().error).toBeNull();
    } finally { session.dispose(); }
  });
});
