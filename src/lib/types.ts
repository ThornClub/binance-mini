export type Side = "Long" | "Short" | "Both";
export type KlineInterval = "1m" | "15m" | "1h" | "1d";

export interface Candle {
  t: number;
  o: number;
  h: number;
  l: number;
  c: number;
  trades: number;
  closed: boolean;
}

export interface Freshness {
  last_success_ms: number;
  max_age_ms: number;
  error: string | null;
}
export interface ChartToken {
  account_generation: number;
  chart_generation: number;
  symbol: string;
  interval: KlineInterval;
}
export interface ChartView {
  token: ChartToken | null;
  reconnect_epoch: number;
  live_seq: number;
  candle: Candle | null;
  connected: boolean;
  connection_error: string | null;
  last_live_ms: number;
  refresh_interval_ms: number;
  status: "loading" | "recovering" | "live" | "polling" | "stale" | "error";
  freshness: Freshness;
}
export interface ChartHistory {
  token: ChartToken;
  reconnect_epoch: number;
  request_id: number;
  rows: Candle[];
}

export interface PositionView {
  symbol: string;
  side: Side;
  qty: number;
  entry_price: number;
  mark_price: number;
  upnl: number;
  upnl_pct: number;
  leverage: number;
  liq_price: number | null;
  liq_distance_pct: number | null;
  margin_ratio: number | null;
  tp_price: number | null;
  sl_price: number | null;
}
export interface MarketView {
  symbol: string;
  mark_price: number;
  funding_rate: number;
  next_funding_at: number;
  high_24h: number;
  low_24h: number;
  change_pct_24h: number;
  quote_volume_24h: number;
  freshness: Freshness;
}
export interface AccountView {
  total_wallet_balance: number;
  total_unrealized_pnl: number;
  total_maint_margin: number;
  available_balance: number;
  total_margin_balance: number;
  margin_ratio: number;
}
export interface HealthView {
  user_stream_ok: boolean;
  market_stream_ok: boolean;
  started_at_ms: number;
  last_error: string | null;
}
/** 强平距离告警设置, 距离单位百分比. */
export interface RiskSettings {
  liq_warn_pct: number;
  hysteresis_pct: number;
}
export interface Snapshot {
  revision: number;
  account_generation: number;
  positions: PositionView[];
  markets: MarketView[];
  account: AccountView | null;
  health: HealthView;
  position_freshness: Freshness;
  account_freshness: Freshness;
  chart: ChartView;
  risk: RiskSettings;
}
/** 平仓预览: 数量已按交易所步长量化, 与真正发单使用同一份计算. */
export interface ClosePlan {
  symbol: string;
  qty: number;
  qty_text: string;
  notional: number;
  order_side: string;
  position_side: string;
  reduce_only: boolean;
  max_qty: number;
  mark_price: number;
}
export interface CloseResult {
  order_id: number;
  status: string;
  filled_qty: number;
  avg_price: number;
  qty_text: string;
}
export interface AlertLine {
  id: string;
  symbol: string;
  price: number;
  enabled: boolean;
}
export const EMPTY_FRESHNESS: Freshness = { last_success_ms: 0, max_age_ms: 0, error: null };
export const EMPTY_CHART: ChartView = {
  token: null, reconnect_epoch: 0, live_seq: 0, candle: null,
  connected: false, connection_error: null, last_live_ms: 0, refresh_interval_ms: 0,
  status: "loading", freshness: EMPTY_FRESHNESS,
};
export const EMPTY_SNAPSHOT: Snapshot = {
  revision: 0, account_generation: 0, positions: [], markets: [], account: null,
  health: { user_stream_ok: false, market_stream_ok: false, started_at_ms: 0, last_error: null },
  position_freshness: EMPTY_FRESHNESS, account_freshness: EMPTY_FRESHNESS,
  chart: EMPTY_CHART, risk: { liq_warn_pct: 10, hysteresis_pct: 2 },
};
