import type { Candle, ChartHistory, ChartToken, ChartView, KlineInterval } from "./types";

export const BAR_LIMIT: Record<KlineInterval, number> = { "1m": 180, "15m": 120, "1h": 120, "1d": 90 };
/** 未收到快照时没有后端刷新周期可用, 用固定回退周期继续重试. */
export const FALLBACK_REFRESH_MS = 5_000;
/** 超过该时长仍在途的请求视为卡死, 由新请求作废, 不让自动重试和按钮永久失效. */
export const STUCK_REQUEST_MS = 15_000;
export function sameToken(a: ChartToken | null, b: ChartToken | null): boolean {
  return a != null && b != null && a.account_generation === b.account_generation &&
    a.chart_generation === b.chart_generation && a.symbol === b.symbol && a.interval === b.interval;
}
export function validCandle(c: Candle): boolean {
  return Number.isSafeInteger(c.t) && c.t > 0 && Number.isSafeInteger(c.trades) && c.trades >= 0 &&
    [c.o, c.h, c.l, c.c].every((n) => Number.isFinite(n) && n > 0) &&
    c.h >= Math.max(c.o, c.c, c.l) && c.l <= Math.min(c.o, c.c) && typeof c.closed === "boolean";
}
export function mergeCandles(current: readonly Candle[], incoming: readonly Candle[], limit: number): Candle[] {
  const map = new Map(current.map((row) => [row.t, row]));
  for (const c of incoming) {
    if (!validCandle(c)) continue;
    const old = map.get(c.t);
    // 成交笔数是跨REST/WS版本; 终结态只能前进, 相同版本保留已知live.
    if (!old || (c.trades > old.trades && (!old.closed || c.closed)) ||
      (c.trades === old.trades && c.closed && !old.closed)) map.set(c.t, c);
  }
  return [...map.values()].sort((a, b) => a.t - b.t).slice(-limit);
}
interface FeedValue { view: ChartView; rows: Candle[] }
// IPC入口同步收集每条蜡烛, React即使批处理多次通知也不会丢中间根.
class ChartFeed {
  private value: FeedValue | null = null;
  private account = 0;
  private generation = 0;
  private revision = -1;
  private listeners = new Set<() => void>();
  subscribe = (fn: () => void) => { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; };
  get = () => this.value;
  ingest(view: ChartView, account: number, revision: number) {
    if (account < this.account) return;
    if (account > this.account) { this.value = null; this.generation = 0; this.revision = -1; this.account = account; }
    const token = view.token;
    if (!token) {
      if (revision < this.revision) return;
      this.revision = revision;
      this.value = null;
    } else {
      if (token.account_generation !== account || token.chart_generation < this.generation) return;
      if (token.chart_generation > this.generation) {
        if (revision < this.revision) return;
        this.generation = token.chart_generation;
        this.value = { view, rows: [] };
      }
      // clear之后迟到的同代快照不能恢复已退订图表.
      if (!this.value || !sameToken(this.value.view.token, token)) return;
      const old = this.value;
      if (view.reconnect_epoch < old.view.reconnect_epoch) return;
      const rows = view.candle ? mergeCandles(old.rows, [view.candle], BAR_LIMIT[token.interval]) : old.rows;
      this.value = { view: revision >= this.revision ? view : old.view, rows };
      this.revision = Math.max(this.revision, revision);
    }
    for (const fn of this.listeners) fn();
  }
}
export const chartFeed = new ChartFeed();
export interface ChartApi {
  setFocus(symbol: string, interval: KlineInterval): Promise<ChartToken>;
  clearFocus(token: ChartToken): Promise<void>;
  klines(token: ChartToken, epoch: number, requestId: number, limit: number): Promise<ChartHistory>;
}
export interface SessionValue {
  rows: Candle[];
  loading: boolean;
  error: string | null;
  view: ChartView | null;
  historyRevision: number;
}
export class ChartSession {
  private token: ChartToken | null = null;
  private disposed = false;
  private starting = false;
  private request = 0;
  private epoch = -1;
  private timer: ReturnType<typeof setInterval> | null = null;
  private nextRetry = 0;
  private loadingSince = 0;
  private inFlightEpoch: number | null = null;
  private lastHistoryMs = 0;
  private unsubscribe: (() => void) | null = null;
  private value: SessionValue = { rows: [], loading: true, error: null, view: null, historyRevision: 0 };
  private listeners = new Set<() => void>();
  constructor(private api: ChartApi, readonly symbol: string, readonly interval: KlineInterval) {}
  get = () => this.value;
  subscribe = (fn: () => void) => { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; };
  private publish(patch: Partial<SessionValue>) {
    this.value = { ...this.value, ...patch };
    for (const fn of this.listeners) fn();
  }
  async start() {
    if (this.starting || this.disposed) return;
    this.starting = true;
    this.publish({ loading: true, error: null });
    this.loadingSince = Date.now();
    try {
      const token = await this.api.setFocus(this.symbol, this.interval);
      if (this.disposed) { await this.api.clearFocus(token); return; }
      this.token = token;
      this.unsubscribe = chartFeed.subscribe(this.receive);
      this.receive();
      if (this.epoch < 0) { this.epoch = 0; void this.reload(); }
      this.timer = setInterval(() => {
        const view = this.value.view;
        const now = Date.now();
        if (now < this.nextRetry) return;
        // 卡死请求(含REST 12秒预算)必须能被新请求作废, 否则按钮与重试时钟一起永久失效.
        const stuck = this.value.loading && now - this.loadingSince >= STUCK_REQUEST_MS;
        if (this.value.loading && !stuck) return;
        const interval = view?.refresh_interval_ms ?? FALLBACK_REFRESH_MS;
        // 活跃WS不全量刷新; 备用周期独立于包含请求预算的数据超龄期限.
        const live = view != null && view.connected && view.last_live_ms > 0 && now - view.last_live_ms <= interval;
        if ((!live || view?.freshness.error != null || this.value.error != null) && now - this.lastHistoryMs >= interval)
          void this.reload(stuck);
      }, 1000);
    } catch (error) {
      if (!this.disposed) this.publish({ loading: false, error: String(error) });
    } finally { this.starting = false; }
  }
  private receive = () => {
    const feed = chartFeed.get();
    if (this.disposed || !feed || !sameToken(feed.view.token, this.token)) return;
    const nextEpoch = feed.view.reconnect_epoch;
    this.publish({ view: feed.view, rows: mergeCandles(this.value.rows, feed.rows, BAR_LIMIT[this.interval]) });
    if (nextEpoch !== this.epoch) { this.epoch = nextEpoch; void this.reload(); }
  };
  /** 手动重试: 在途请求仍在预算内时共用同一请求, 超过预算(卡死)才强制新请求. */
  retry = () => {
    if (!this.token) { void this.start(); return; }
    void this.reload(this.value.loading && Date.now() - this.loadingSince >= STUCK_REQUEST_MS);
  };
  /** force 用于手动重试与卡死回收: 同一 epoch 也允许发起新请求. */
  private async reload(force = false) {
    const token = this.token;
    if (!token || this.disposed) return;
    const epoch = this.epoch;
    if (!force && this.inFlightEpoch === epoch) return;
    const request = ++this.request;
    this.inFlightEpoch = epoch;
    this.publish({ loading: true, error: null });
    this.loadingSince = Date.now();
    try {
      const result = await this.api.klines(token, epoch, request, BAR_LIMIT[this.interval]);
      if (this.disposed || request !== this.request || epoch !== this.epoch) return;
      if (!sameToken(result.token, token) || result.reconnect_epoch !== epoch || result.request_id !== request)
        throw new Error("图表历史身份不匹配");
      this.lastHistoryMs = Date.now();
      this.publish({ rows: mergeCandles(this.value.rows, result.rows, BAR_LIMIT[this.interval]),
        loading: false, error: null, historyRevision: this.value.historyRevision + 1 });
    } catch (error) {
      if (!this.disposed && request === this.request && epoch === this.epoch)
        this.publish({ loading: false, error: String(error) });
    } finally {
      // 只有当前请求拥有loading与重试时钟, 旧epoch完成不能延迟新epoch恢复.
      if (!this.disposed && request === this.request && epoch === this.epoch) {
        this.inFlightEpoch = null;
        this.nextRetry = Date.now() + (this.value.view?.refresh_interval_ms ?? 0);
      }
    }
  }
  dispose() {
    this.disposed = true;
    ++this.request;
    this.unsubscribe?.();
    if (this.timer !== null) clearInterval(this.timer);
    this.listeners.clear();
    if (this.token) void this.api.clearFocus(this.token).catch(() => {});
  }
}
