export type Side = "Long" | "Short" | "Both";

export interface Candle {
  t: number;
  o: number;
  h: number;
  l: number;
  c: number;
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
  last_candle: Candle | null;
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
  last_msg_ms: number;
  started_at_ms: number;
  last_error: string | null;
}
/** 强平距离告警设置, 距离单位百分比. */
export interface RiskSettings {
  liq_warn_pct: number;
  hysteresis_pct: number;
}

export interface Snapshot {
  positions: PositionView[];
  markets: MarketView[];
  account: AccountView | null;
  health: HealthView;
  stale: boolean;
  risk: RiskSettings;
}

export interface AlertLine {
  id: string;
  symbol: string;
  price: number;
  enabled: boolean;
}

export const EMPTY_SNAPSHOT: Snapshot = {
  positions: [],
  markets: [],
  account: null,
  health: {
    user_stream_ok: false,
    market_stream_ok: false,
    last_msg_ms: 0,
    started_at_ms: 0,
    last_error: null,
  },
  stale: false,
  risk: { liq_warn_pct: 10, hysteresis_pct: 2 },
};
