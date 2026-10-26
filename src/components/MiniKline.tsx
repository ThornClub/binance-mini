import { useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  AreaSeries, CandlestickSeries, CrosshairMode, LineStyle, createChart,
  type IChartApi, type ISeriesApi, type IPriceLine, type UTCTimestamp,
} from "lightweight-charts";
import type { Candle, KlineInterval } from "../lib/types";
import { api } from "../lib/api";
import { ChartSession, type SessionValue } from "../lib/chart-session";
import { freshnessLabel, isStale, useNow } from "../lib/freshness";
import "./chart-state.css";

export type ChartInterval = "time" | "15m" | "1h" | "1d";
export interface ChartLine { price: number; color: string; title: string; dashed?: boolean }
interface Props { symbol: string; interval: ChartInterval; lines: ChartLine[] }
const EMPTY: SessionValue = { rows: [], loading: true, error: null, view: null, historyRevision: 0 };
function fmtPrice(v: number) {
  if (!Number.isFinite(v) || v === 0) return "--";
  if (v >= 1000) return v.toFixed(1);
  if (v >= 1) return v.toFixed(3);
  return v.toPrecision(4);
}

export default function MiniKline({ symbol, interval, lines }: Props) {
  const boxRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Candlestick"> | ISeriesApi<"Area"> | null>(null);
  const mapRef = useRef(new Map<number, Candle>());
  const painted = useRef<Candle[]>([]);
  const historyRevision = useRef(-1);
  const fitted = useRef(false);
  const sessionRef = useRef<ChartSession | null>(null);
  const [ready, setReady] = useState(0);
  const [hover, setHover] = useState<Candle | null>(null);
  const [result, setResult] = useState<{ key: string; value: SessionValue }>({ key: "", value: EMPTY });
  const key = `${symbol}|${interval}`;
  const value = result.key === key ? result.value : EMPTY;
  const now = useNow();
  const exchangeInterval: KlineInterval = interval === "time" ? "1m" : interval;

  // layout阶段清掉旧画布和悬停, 不让B的标题与A的价格同屏一帧.
  useLayoutEffect(() => {
    seriesRef.current?.setData([]);
    painted.current = [];
    mapRef.current.clear();
    historyRevision.current = -1;
    fitted.current = false;
    setHover(null);
    setResult({ key, value: EMPTY });
    const session = new ChartSession(api, symbol, exchangeInterval);
    sessionRef.current = session;
    const stop = session.subscribe(() => setResult({ key, value: session.get() }));
    void session.start();
    return () => { stop(); session.dispose(); sessionRef.current = null; };
  }, [key, symbol, exchangeInterval]);

  useEffect(() => {
    const el = boxRef.current;
    if (!el) return;
    let chart: IChartApi | null = null;
    let observer: ResizeObserver | null = null;
    let resizeRaf = 0;
    let sizeKey = "";
    const raf = requestAnimationFrame(() => {
      chart = createChart(el, {
        width: Math.max(el.clientWidth, 240), height: Math.max(el.clientHeight, 80),
        layout: { background: { color: "transparent" }, textColor: "#79818f", fontSize: 11, attributionLogo: false },
        grid: { vertLines: { visible: false }, horzLines: { color: "rgba(255,255,255,0.05)" } },
        rightPriceScale: { visible: true, borderVisible: false, entireTextOnly: true, scaleMargins: { top: 0.12, bottom: 0.12 } },
        timeScale: { visible: true, borderVisible: false, secondsVisible: false, rightOffset: 3 },
        crosshair: {
          mode: CrosshairMode.Normal,
          vertLine: { color: "rgba(255,255,255,0.2)", width: 1, style: LineStyle.Dotted, labelBackgroundColor: "#2b3240" },
          horzLine: { color: "rgba(255,255,255,0.2)", width: 1, style: LineStyle.Dotted, labelBackgroundColor: "#2b3240" },
        },
        localization: { priceFormatter: (p: number) => p >= 1000 ? p.toFixed(1) : p >= 1 ? p.toFixed(3) : p.toFixed(4) },
        handleScale: { axisPressedMouseMove: true, mouseWheel: true, pinch: true },
        handleScroll: { mouseWheel: false, pressedMouseMove: true, horzTouchDrag: true, vertTouchDrag: false },
      });
      chartRef.current = chart;
      chart.subscribeCrosshairMove((param) => setHover(param.time === undefined ? null : mapRef.current.get(param.time as number) ?? null));
      observer = new ResizeObserver(() => {
        cancelAnimationFrame(resizeRaf);
        resizeRaf = requestAnimationFrame(() => {
          if (!chart) return;
          const w = el.clientWidth, h = el.clientHeight;
          if (w <= 0 || h <= 0 || `${w}x${h}` === sizeKey) return;
          sizeKey = `${w}x${h}`;
          chart.applyOptions({ width: w, height: h });
        });
      });
      observer.observe(el);
      setReady((n) => n + 1);
    });
    return () => {
      cancelAnimationFrame(raf); cancelAnimationFrame(resizeRaf); observer?.disconnect();
      chart?.remove(); chart = null; chartRef.current = null; seriesRef.current = null;
    };
  }, []);

  useLayoutEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    const series = interval === "time" ? chart.addSeries(AreaSeries, {
      lineColor: "#0ecb81", lineWidth: 1, topColor: "rgba(14,203,129,0.22)", bottomColor: "rgba(14,203,129,0.01)",
      priceLineVisible: true, priceLineStyle: LineStyle.Dotted, lastValueVisible: true,
    }) : chart.addSeries(CandlestickSeries, {
      upColor: "#0ecb81", downColor: "#f6465d", borderVisible: false, wickUpColor: "#0ecb81", wickDownColor: "#f6465d",
      priceLineVisible: true, priceLineStyle: LineStyle.Dotted, priceLineWidth: 1, lastValueVisible: true,
    });
    seriesRef.current = series;
    painted.current = []; historyRevision.current = -1; fitted.current = false;
    chart.applyOptions({ timeScale: { timeVisible: interval !== "1d" } });
    return () => {
      if (chartRef.current === chart) chart.removeSeries(series);
      if (seriesRef.current === series) seriesRef.current = null;
    };
  }, [interval, ready]);

  useLayoutEffect(() => {
    const series = seriesRef.current, chart = chartRef.current;
    if (!series || !chart) return;
    const rows = value.rows;
    const old = painted.current;
    const range = chart.timeScale().getVisibleLogicalRange();
    const area = (c: Candle) => ({ time: (c.t / 1000) as UTCTimestamp, value: c.c });
    const bar = (c: Candle) => ({ time: (c.t / 1000) as UTCTimestamp, open: c.o, high: c.h, low: c.l, close: c.c });
    const setData = () => {
      if (interval === "time") (series as ISeriesApi<"Area">).setData(rows.map(area));
      else (series as ISeriesApi<"Candlestick">).setData(rows.map(bar));
    };
    const previous = new Map(old.map((row) => [row.t, row]));
    const changed = rows.filter((row) => previous.get(row.t) !== row);
    const last = old[old.length - 1]?.t ?? 0;
    if (!old.length || historyRevision.current !== value.historyRevision || changed.some((c) => c.t < last)) {
      setData();
      if (range && fitted.current) chart.timeScale().setVisibleLogicalRange(range);
    } else {
      for (const c of changed) {
        if (interval === "time") (series as ISeriesApi<"Area">).update(area(c));
        else (series as ISeriesApi<"Candlestick">).update(bar(c));
      }
      // 缓存窗口移出最旧根时同步裁剪图表, 普通同根更新只走update.
      if (old[0]?.t !== rows[0]?.t) { setData(); if (range) chart.timeScale().setVisibleLogicalRange(range); }
    }
    painted.current = rows;
    historyRevision.current = value.historyRevision;
    mapRef.current = new Map(rows.map((c) => [c.t / 1000, c]));
    if (rows.length && !fitted.current) { chart.timeScale().fitContent(); fitted.current = true; }
  }, [value.rows, value.historyRevision, interval, ready]);

  const linesKey = JSON.stringify(lines);
  useEffect(() => {
    const series = seriesRef.current;
    if (!series) return;
    const created: IPriceLine[] = [];
    for (const line of lines) {
      if (!Number.isFinite(line.price) || line.price <= 0) continue;
      created.push(series.createPriceLine({ price: line.price, color: line.color, title: line.title,
        lineWidth: 1, lineStyle: line.dashed ? LineStyle.Dashed : LineStyle.Solid, axisLabelVisible: true }));
    }
    return () => { if (seriesRef.current === series) for (const line of created) series.removePriceLine(line); };
    // 价格线按内容变化重建, 不随行情每帧拆装.
  }, [linesKey, interval, ready]);

  const stale = value.error != null || isStale(value.view?.freshness, now);
  const view = value.view;
  const live = !stale && !value.loading && view?.status === "live" && view.connected &&
    view.last_live_ms > 0 && now - view.last_live_ms <= view.refresh_interval_ms;
  const status = value.error ? `${symbol}加载失败`
    : stale && value.rows.length ? `图表数据已过期${value.loading ? " · 补齐中" : ""}`
    : value.loading ? `${symbol}${value.rows.length ? "补齐中" : "加载中"}`
    : live ? "实时"
    : `定时刷新 · ${view?.connected ? "等待有效推送" : "正在重连"}`;
  const detail = !live && !value.loading ? [value.error, freshnessLabel(view?.freshness, now),
    view?.connection_error ? `实时连接: ${view.connection_error}` : null].filter(Boolean).join("; ") : null;
  const shown = (result.key === key ? hover : null) ?? value.rows[value.rows.length - 1] ?? null;
  const change = shown ? (shown.c - shown.o) / shown.o * 100 : 0;
  return <div className={`chart-box${stale && value.rows.length ? " data-stale" : ""}`}>
    <div className="chart-status" role="status">
      <span className="chart-status-label">{status}</span>
      {!value.loading && !live && <button onClick={() => sessionRef.current?.retry()}>重试图表</button>}
      {detail && <details className="chart-details" key={key}><summary>详情</summary><div>{detail}</div></details>}
    </div>
    {/* 状态行不占绘图区, ResizeObserver仍只测量实际Canvas宿主. */}
    <div className="chart-plot">
      <div className="kline" ref={boxRef} />
      {shown && <div className="ohlc">
        <span>开 {fmtPrice(shown.o)}</span><span>高 {fmtPrice(shown.h)}</span>
        <span>低 {fmtPrice(shown.l)}</span><span>收 {fmtPrice(shown.c)}</span>
        <span className={change >= 0 ? "up" : "down"}>{change >= 0 ? "+" : ""}{change.toFixed(2)}%</span>
      </div>}
    </div>
  </div>;
}
