import { useEffect, useRef, useState } from "react";
import {
  AreaSeries,
  CandlestickSeries,
  CrosshairMode,
  LineStyle,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type MouseEventParams,
  type UTCTimestamp,
} from "lightweight-charts";
import type { Candle } from "../lib/types";
import { api } from "../lib/api";

/** 分时 = 1 分钟收盘价连线; 其余走蜡烛图. */
export type ChartInterval = "time" | "15m" | "1h" | "1d";

const KLINE_INTERVAL: Record<ChartInterval, string> = {
  time: "1m",
  "15m": "15m",
  "1h": "1h",
  "1d": "1d",
};

const BAR_LIMIT: Record<ChartInterval, number> = {
  time: 180,
  "15m": 120,
  "1h": 120,
  "1d": 90,
};

export interface ChartLine {
  price: number;
  color: string;
  title: string;
  dashed?: boolean;
}

interface Props {
  symbol: string;
  live: Candle | null;
  interval: ChartInterval;
  /** 开仓价/强平/止盈止损/告警等需要画在图上的价格线. */
  lines: ChartLine[];
}

function fmtPrice(v: number) {
  if (!Number.isFinite(v) || v === 0) return "--";
  if (v >= 1000) return v.toFixed(1);
  if (v >= 1) return v.toFixed(3);
  return v.toPrecision(4);
}

/** 迷你 K 线: 可滚轮缩放/拖动平移, 悬停显示该根的开高低收. */
export default function MiniKline({ symbol, live, interval, lines }: Props) {
  const boxRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Candlestick"> | ISeriesApi<"Area"> | null>(null);
  const dataRef = useRef<Candle[]>([]);
  const mapRef = useRef<Map<number, Candle>>(new Map());
  const fitKeyRef = useRef<string>("");
  const [hover, setHover] = useState<Candle | null>(null);
  const linesRef = useRef<{ series: unknown; line: unknown }[]>([]);
  // 非分时周期没有逐笔推送可用, 靠定时重取保持最新.
  const [reload, setReload] = useState(0);

  useEffect(() => {
    const id = window.setInterval(() => setReload((v) => v + 1), 20_000);
    return () => window.clearInterval(id);
  }, []);

  // 图表实例整个组件只建一次, 切周期走换系列; 重建图表会让旧实例残留未执行的重绘帧.
  const [ready, setReady] = useState(0);

  useEffect(() => {
    const el = boxRef.current;
    if (!el) return;
    let disposed = false;
    let chart: IChartApi | null = null;
    let ro: ResizeObserver | null = null;
    let sizeKey = "";
    let resizeRaf = 0;

    // StrictMode 会同步卸载再挂载: 延迟一帧建图表, 卸载时直接取消 rAF,
    // 旧实例根本不存在, 也就不会在下一帧对着已销毁的画布重绘 (控制台刷 "Object is disposed").
    const raf = requestAnimationFrame(() => {
      if (disposed) return;
      const c = createChart(el, {
        width: Math.max(el.clientWidth, 240),
        height: Math.max(el.clientHeight, 80),
        layout: {
          background: { color: "transparent" },
          textColor: "#79818f",
          fontSize: 9,
          // 图表右下角的 TradingView 水印标志, 小窗里挡 K 线.
          attributionLogo: false,
        },
        grid: { vertLines: { visible: false }, horzLines: { color: "rgba(255,255,255,0.05)" } },
        rightPriceScale: {
          visible: true,
          borderVisible: false,
          entireTextOnly: true,
          scaleMargins: { top: 0.12, bottom: 0.12 },
        },
        timeScale: {
          visible: true,
          borderVisible: false,
          secondsVisible: false,
          rightOffset: 3,
        },
        crosshair: {
          mode: CrosshairMode.Normal,
          vertLine: {
            color: "rgba(255,255,255,0.2)",
            width: 1,
            style: LineStyle.Dotted,
            labelBackgroundColor: "#2b3240",
          },
          horzLine: {
            color: "rgba(255,255,255,0.2)",
            width: 1,
            style: LineStyle.Dotted,
            labelBackgroundColor: "#2b3240",
          },
        },
        localization: {
          priceFormatter: (p: number) => (p >= 1000 ? p.toFixed(1) : p >= 1 ? p.toFixed(3) : p.toFixed(4)),
        },
        // 允许滚轮缩放 + 按住拖动平移; 竖向拖动不接管, 避免误触窗口拖动.
        handleScale: { axisPressedMouseMove: true, mouseWheel: true, pinch: true },
        handleScroll: {
          mouseWheel: false,
          pressedMouseMove: true,
          horzTouchDrag: true,
          vertTouchDrag: false,
        },
      });
      chart = c;
      chartRef.current = c;

      // 悬停/长按显示该根的 OHLC. 触摸设备按下即触发, 相当于长按查看.
      c.subscribeCrosshairMove((param: MouseEventParams) => {
        if (disposed) return;
        if (param.time === undefined) {
          setHover(null);
          return;
        }
        setHover(mapRef.current.get(param.time as number) ?? null);
      });

      // 高度跟着容器走: 窗口拉大时 K 线一起长高, 不留空白.
      // 尺寸没变就不下发, 避免无意义的重绘帧堆积.
      // 回调里直接改画布尺寸会让 ResizeObserver 立刻再触发一次, 控制台刷
      // "ResizeObserver loop completed with undelivered notifications". 推到下一帧再改.
      ro = new ResizeObserver(() => {
        if (disposed || !chart) return;
        cancelAnimationFrame(resizeRaf);
        resizeRaf = requestAnimationFrame(() => {
          if (disposed || !chart) return;
          const w = el.clientWidth;
          const h = el.clientHeight;
          if (w <= 0 || h <= 0) return;
          const key = `${w}x${h}`;
          if (key === sizeKey) return;
          sizeKey = key;
          chart.applyOptions({ width: w, height: h });
        });
      });
      ro.observe(el);

      setReady((v) => v + 1);
    });

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      cancelAnimationFrame(resizeRaf);
      ro?.disconnect();
      if (chart) {
        chart.remove();
        chart = null;
      }
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, []);

  // 切周期只换系列 (面积线 / 蜡烛), 图表实例与画布都不动.
  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    const series =
      interval === "time"
        ? chart.addSeries(AreaSeries, {
            lineColor: "#0ecb81",
            lineWidth: 1,
            topColor: "rgba(14,203,129,0.22)",
            bottomColor: "rgba(14,203,129,0.01)",
            priceLineVisible: true,
            priceLineStyle: LineStyle.Dotted,
            lastValueVisible: true,
          })
        : chart.addSeries(CandlestickSeries, {
            upColor: "#0ecb81",
            downColor: "#f6465d",
            borderVisible: false,
            wickUpColor: "#0ecb81",
            wickDownColor: "#f6465d",
            // 最后一根的价格线 + 右侧价格标签, 一眼能读到当前价.
            priceLineVisible: true,
            priceLineStyle: LineStyle.Dotted,
            priceLineWidth: 1,
            lastValueVisible: true,
          });
    seriesRef.current = series;
    dataRef.current = [];
    mapRef.current = new Map();
    fitKeyRef.current = "";
    linesRef.current = [];
    chart.applyOptions({ timeScale: { timeVisible: interval !== "1d" } });

    return () => {
      // 整体卸载时图表已销毁, 再调 removeSeries 会抛 "Object is disposed".
      if (chartRef.current === chart) chart.removeSeries(series);
      if (seriesRef.current === series) seriesRef.current = null;
    };
  }, [interval, ready]);

  useEffect(() => {
    let cancelled = false;
    if (!symbol || !chartRef.current) return;

    api
      .klines(symbol, KLINE_INTERVAL[interval], BAR_LIMIT[interval])
      .then((rows) => {
        if (cancelled) return;
        const series = seriesRef.current;
        if (!series) return;

        // 记住用户当前的缩放位置, 定时刷新时不要把他拉回全览.
        const prevRange = chartRef.current?.timeScale().getVisibleLogicalRange() ?? null;
        const prevLen = dataRef.current.length;

        dataRef.current = rows;
        mapRef.current = new Map(rows.map((c) => [Math.floor(c.t / 1000), c]));

        if (interval === "time") {
          (series as ISeriesApi<"Area">).setData(
            rows.map((c) => ({ time: (c.t / 1000) as UTCTimestamp, value: c.c })),
          );
        } else {
          (series as ISeriesApi<"Candlestick">).setData(
            rows.map((c) => ({
              time: (c.t / 1000) as UTCTimestamp,
              open: c.o,
              high: c.h,
              low: c.l,
              close: c.c,
            })),
          );
        }

        const key = `${symbol}|${interval}`;
        const chart = chartRef.current;
        if (!chart) return;
        if (fitKeyRef.current !== key) {
          // 换合约/换周期才全览
          fitKeyRef.current = key;
          chart.timeScale().fitContent();
        } else if (prevRange && prevLen === rows.length) {
          chart.timeScale().setVisibleLogicalRange(prevRange);
        }
      })
      .catch(() => {
        /* K 线失败不影响持仓展示 */
      });

    return () => {
      cancelled = true;
    };
  }, [symbol, interval, reload, ready]);

  // 分时周期可以吃 1 分钟实时推送; 其他周期等定时重取.
  useEffect(() => {
    if (!live || interval !== "time") return;
    const series = seriesRef.current as ISeriesApi<"Area"> | null;
    if (!series) return;
    const last = dataRef.current[dataRef.current.length - 1];
    if (last && last.t === live.t) {
      dataRef.current[dataRef.current.length - 1] = live;
    } else if (!last || live.t > last.t) {
      dataRef.current.push(live);
    } else {
      return;
    }
    mapRef.current.set(Math.floor(live.t / 1000), live);
    series.update({ time: (live.t / 1000) as UTCTimestamp, value: live.c });
  }, [live, interval, ready]);

  // 价格线随持仓/告警变化重建; 用 JSON 做依赖, 避免每帧都拆装.
  const linesKey = JSON.stringify(lines);
  useEffect(() => {
    const series = seriesRef.current;
    if (!series) return;
    for (const it of linesRef.current) {
      try {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (it.series as any).removePriceLine(it.line);
      } catch {
        /* 已经随系列一起销毁 */
      }
    }
    linesRef.current = [];
    for (const l of lines) {
      if (!Number.isFinite(l.price) || l.price <= 0) continue;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const line = (series as any).createPriceLine({
        price: l.price,
        color: l.color,
        lineWidth: 1,
        lineStyle: l.dashed ? LineStyle.Dashed : LineStyle.Solid,
        axisLabelVisible: true,
        title: l.title,
      });
      linesRef.current.push({ series, line });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [linesKey, interval, ready]);

  const shown = hover ?? dataRef.current[dataRef.current.length - 1] ?? null;
  const chg = shown && shown.o > 0 ? ((shown.c - shown.o) / shown.o) * 100 : 0;

  return (
    <div className="chart-box">
      <div className="kline" ref={boxRef} />
      {shown && (
        <div className="ohlc">
          <span>开 {fmtPrice(shown.o)}</span>
          <span>高 {fmtPrice(shown.h)}</span>
          <span>低 {fmtPrice(shown.l)}</span>
          <span>收 {fmtPrice(shown.c)}</span>
          <span className={chg >= 0 ? "up" : "down"}>
            {chg >= 0 ? "+" : ""}
            {chg.toFixed(2)}%
          </span>
        </div>
      )}
    </div>
  );
}
