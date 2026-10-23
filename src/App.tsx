import { useEffect, useMemo, useState, useSyncExternalStore, type CSSProperties } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, getSnapshot, initBridge, subscribeSnapshot } from "./lib/api";
import type { PositionView } from "./lib/types";
import SetupWizard from "./components/SetupWizard";
import MiniKline, { type ChartInterval, type ChartLine } from "./components/MiniKline";
import "./App.css";

/** 无边框窗口没有系统缩放热区, 用四边四角热区手工补上. */
/** 该版本 @tauri-apps/api 未导出 ResizeDirection 类型, 这里本地声明同一套取值. */
type ResizeDir = "North" | "South" | "West" | "East" | "NorthWest" | "NorthEast" | "SouthWest" | "SouthEast";
const RESIZE_ZONES: { dir: ResizeDir; cls: string }[] = [
  { dir: "North", cls: "n" },
  { dir: "South", cls: "s" },
  { dir: "West", cls: "w" },
  { dir: "East", cls: "e" },
  { dir: "NorthWest", cls: "nw" },
  { dir: "NorthEast", cls: "ne" },
  { dir: "SouthWest", cls: "sw" },
  { dir: "SouthEast", cls: "se" },
];

function ResizeHandles() {
  return (
    <>
      {RESIZE_ZONES.map((z) => (
        <div
          key={z.cls}
          className={`rz ${z.cls}`}
          onMouseDown={(e) => {
            if (e.button !== 0) return;
            e.preventDefault();
            void getCurrentWindow().startResizeDragging(z.dir);
          }}
        />
      ))}
    </>
  );
}

function fmt(v: number, digits = 2) {
  if (!Number.isFinite(v)) return "--";
  return v.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

function fmtPrice(v: number) {
  if (!Number.isFinite(v) || v === 0) return "--";
  if (v >= 1000) return fmt(v, 1);
  if (v >= 1) return fmt(v, 3);
  return v.toPrecision(4);
}

function sideLabel(p: PositionView) {
  if (p.side === "Long") return "多";
  if (p.side === "Short") return "空";
  return p.qty > 0 ? "多" : "空";
}

function fundingCountdown(ts: number) {
  if (!ts) return "--:--";
  const ms = ts - Date.now();
  if (ms <= 0) return "结算中";
  const h = Math.floor(ms / 3600000);
  const m = Math.floor((ms % 3600000) / 60000);
  return `${h}h${String(m).padStart(2, "0")}m`;
}

export default function App() {
  const snap = useSyncExternalStore(subscribeSnapshot, getSnapshot);
  const [hasCreds, setHasCreds] = useState<boolean | null>(null);
  const [focus, setFocus] = useState<string | null>(null);
  const [alertPrice, setAlertPrice] = useState("");
  const [alerts, setAlerts] = useState<{ id: string; symbol: string; price: number }[]>([]);
  const [showSettings, setShowSettings] = useState(false);
  const [chartInterval, setChartInterval] = useState<ChartInterval>("time");
  const [autoStart, setAutoStart] = useState(false);
  // 输入框正在编辑的值; null 表示跟随后端快照里的阈值.
  const [liqDraft, setLiqDraft] = useState<number | null>(null);
  const [alertInputOpen, setAlertInputOpen] = useState(false);
  const [checkMsg, setCheckMsg] = useState<string | null>(null);
  // 窗口不透明度, 存本地: 重启后保持上次的手感.
  const [bgAlpha, setBgAlpha] = useState(() => {
    const saved = Number(localStorage.getItem("bgAlpha.v4"));
    return Number.isFinite(saved) && saved >= 0.3 && saved <= 1 ? saved : 1;
  });

  function applyAlpha(v: number) {
    setBgAlpha(v);
    localStorage.setItem("bgAlpha.v4", String(v));
  }

  // CSS 变量透传, 让 .widget 的背景色跟着滑杆走.
  const alphaStyle = { "--bg-alpha": String(bgAlpha) } as CSSProperties;

  // 整页底色必须保持透明: 一旦给 html/body 铺色, 窗口四角就变成方角,
  // .widget 的 12px 圆角只能盖住内容区, 盖不住窗口矩形本身.

  useEffect(() => {
    initBridge().then(async () => {
      setHasCreds(await api.hasCredentials());
      setAlerts(await api.alerts());
    });
    api.autostartEnabled().then(setAutoStart).catch(() => {});
    
  }, []);

  const focused = useMemo(() => {
    if (focus && snap.positions.some((p) => p.symbol === focus)) return focus;
    return snap.positions[0]?.symbol ?? null;
  }, [focus, snap.positions]);

  useEffect(() => {
    if (focused) void api.setFocus(focused);
  }, [focused]);

  const market = snap.markets.find((m) => m.symbol === focused) ?? null;
  const position = snap.positions.find((p) => p.symbol === focused) ?? null;
  const chartLines: ChartLine[] = useMemo(() => {
    const out: ChartLine[] = [];
    if (position) {
      out.push({ price: position.entry_price, color: "rgba(233,237,243,0.45)", title: "开", dashed: true });
      if (position.liq_price) {
        out.push({ price: position.liq_price, color: "#f6465d", title: "强平", dashed: true });
      }
      if (position.tp_price) {
        out.push({ price: position.tp_price, color: "#0ecb81", title: "止盈" });
      }
      if (position.sl_price) {
        out.push({ price: position.sl_price, color: "#f6465d", title: "止损" });
      }
    }
    for (const a of alerts.filter((x) => x.symbol === focused)) {
      out.push({ price: a.price, color: "#f0b90b", title: "告警", dashed: true });
    }
    return out;
  }, [position, alerts, focused]);
  const totalUpnl = snap.positions.reduce((s, p) => s + p.upnl, 0);
  const dotState = !snap.health.user_stream_ok
    ? "down"
    : snap.stale
      ? "stale"
      : "ok";

  async function refreshAlerts() {
    setAlerts(await api.alerts());
  }

  async function addAlert() {
    if (!focused) return;
    const price = Number(alertPrice);
    if (!Number.isFinite(price) || price <= 0) return;
    setAlerts(await api.addAlert(focused, price));
    setAlertPrice("");
  }

  if (hasCreds === false) {
    return (
      <>
      <div className="widget setup-mode" style={alphaStyle}>
        <SetupWizard
          onDone={() => {
            setHasCreds(true);
            refreshAlerts();
          }}
        />
      </div>
      <ResizeHandles />
      </>
    );
  }

  return (
    <>
    <div className={`widget ${snap.stale ? "is-stale" : ""}`} style={alphaStyle}>
      <header data-tauri-drag-region="deep">
        <span className={`dot ${dotState}`} title={snap.health.last_error ?? "连接正常"} />
        <span className="hero">
          <b className={totalUpnl >= 0 ? "up" : "down"}>
            {totalUpnl >= 0 ? "+" : ""}
            {fmt(totalUpnl, 2)}
          </b>
          <em>未实现盈亏</em>
        </span>
        <span className="mr">
          <i>维持</i>
          <b>{snap.account ? `${fmt(snap.account.margin_ratio, 1)}%` : "--"}</b>
        </span>
        <button className="ghost" onClick={() => setShowSettings((v) => !v)} title="设置">
          ⚙
        </button>
        <button className="ghost" onClick={() => api.hideWindow()} title="隐藏 (托盘恢复)">
          ✕
        </button>
      </header>

      {showSettings && (
        <div className="settings">
          <div className="row">
            <button onClick={() => api.setAlwaysOnTop(true)}>置顶</button>
            <button onClick={() => api.setAlwaysOnTop(false)}>取消置顶</button>
          </div>
          <div className="row">
            <label className="alpha">
              不透明度
              <input
                type="range"
                min={30}
                max={100}
                step={5}
                value={Math.round(bgAlpha * 100)}
                onChange={(e) => applyAlpha(Number(e.target.value) / 100)}
              />
              <span className="val">{Math.round(bgAlpha * 100)}%</span>
            </label>
          </div>
          <div className="row">
            <label className="alpha">
              强平告警
              <input
                type="number"
                min={1}
                max={50}
                step={1}
                value={liqDraft ?? Math.round(snap.risk.liq_warn_pct)}
                onChange={(e) => setLiqDraft(Number(e.target.value))}
                onBlur={async () => {
                  if (liqDraft == null) return;
                  try {
                    await api.setRisk(liqDraft);
                    setCheckMsg(`强平距离低于 ${Math.round(liqDraft)}% 时通知`);
                  } catch (e) {
                    setCheckMsg(String(e));
                  }
                  setLiqDraft(null);
                }}
              />
              <span className="val">%</span>
            </label>
          </div>
          <div className="row">
            <button
              onClick={async () => {
                const next = !autoStart;
                try {
                  await api.setAutostart(next);
                  setAutoStart(next);
                  setCheckMsg(next ? "已开启开机自启" : "已关闭开机自启");
                } catch (e) {
                  setCheckMsg(String(e));
                }
              }}
            >
              开机自启 {autoStart ? "开" : "关"}
            </button>
            <button
              onClick={async () => {
                setCheckMsg("检测中...");
                try {
                  setCheckMsg(await api.selfCheck());
                } catch (e) {
                  setCheckMsg(String(e));
                }
              }}
            >
              权限自检
            </button>
            <button
              className="danger"
              onClick={async () => {
                await api.clearCredentials();
                setHasCreds(false);
                setShowSettings(false);
              }}
            >
              清除密钥
            </button>
          </div>
          <div className="row">
            <button onClick={() => api.quit()}>退出程序</button>
          </div>
          {checkMsg && <div className="ok-msg">{checkMsg}</div>}
          {snap.health.last_error && <div className="err">{snap.health.last_error}</div>}
        </div>
      )}

      <main>
        {snap.positions.length === 0 && <div className="empty">暂无持仓</div>}
        {snap.positions.map((p) => (
          <div
            key={`${p.symbol}-${p.side}`}
            className={`pos ${p.symbol === focused ? "focused" : ""}`}
            onClick={() => setFocus(p.symbol)}
          >
            <div className="pos-l">
              <div className="pos-sym">
                <b>{p.symbol.replace("USDT", "")}</b>
                <span className={`badge ${sideLabel(p) === "多" ? "long" : "short"}`}>
                  {sideLabel(p)}
                  <em>{p.leverage}x</em>
                </span>
              </div>
              <div className="pos-px">
                <span>{fmtPrice(p.entry_price)}</span>
                <i>→</i>
                <span className="mark">{fmtPrice(p.mark_price)}</span>
              </div>
            </div>
            <div className="pos-r">
              <b className={p.upnl >= 0 ? "up" : "down"}>
                {p.upnl >= 0 ? "+" : ""}
                {fmt(p.upnl, 2)}
              </b>
              <span className={p.upnl_pct >= 0 ? "up" : "down"}>{fmt(p.upnl_pct, 1)}%</span>
            </div>
            <div
              className={`pos-liq${
                p.liq_distance_pct != null && p.liq_distance_pct <= snap.risk.liq_warn_pct
                  ? " danger"
                  : ""
              }`}
            >
              强平{" "}
              {p.liq_price
                ? `${fmtPrice(p.liq_price)} · 距离 ${fmt(p.liq_distance_pct ?? 0, 1)}%`
                : "--"}
            </div>
          </div>
        ))}
      </main>

      <footer>
        {focused ? (
          <>
            <div className="chart-head">
              <span className="sym">{focused}</span>
              <div className="seg">
                {(["time", "15m", "1h", "1d"] as ChartInterval[]).map((k) => (
                  <button
                    key={k}
                    className={chartInterval === k ? "on" : ""}
                    onClick={() => setChartInterval(k)}
                  >
                    {k === "time" ? "分时" : k === "1d" ? "1日" : k}
                  </button>
                ))}
              </div>
              <div className="alert-zone">
                {alerts
                  .filter((a) => a.symbol === focused)
                  .map((a) => (
                    <span className="alert-chip" key={a.id}>
                      {fmtPrice(a.price)}
                      <button
                        onClick={async () => setAlerts(await api.removeAlert(a.id))}
                        title="删除告警"
                      >
                        ✕
                      </button>
                    </span>
                  ))}
              {alertInputOpen ? (
                <div className="alert-add">
                  <input
                    autoFocus
                    value={alertPrice}
                    onChange={(e) => setAlertPrice(e.target.value)}
                    placeholder="价格"
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        addAlert();
                        setAlertInputOpen(false);
                      }
                      if (e.key === "Escape") {
                        setAlertPrice("");
                        setAlertInputOpen(false);
                      }
                    }}
                    onBlur={() => !alertPrice && setAlertInputOpen(false)}
                  />
                  <button
                    onClick={() => {
                      addAlert();
                      setAlertInputOpen(false);
                    }}
                  >
                    ✓
                  </button>
                </div>
              ) : (
                <button className="alert-btn" onClick={() => setAlertInputOpen(true)}>
                  ＋告警
                </button>
              )}
              </div>
            </div>
            <div className="chart-sub">
              <span>
                24h 高 <b>{market ? fmtPrice(market.high_24h) : "--"}</b>
              </span>
              <span>
                低 <b>{market ? fmtPrice(market.low_24h) : "--"}</b>
              </span>
              <span className={(market?.change_pct_24h ?? 0) >= 0 ? "up" : "down"}>
                {market ? `${market.change_pct_24h >= 0 ? "+" : ""}${fmt(market.change_pct_24h, 2)}%` : "--"}
              </span>
              <span className="grow" />
              <span>费率 {market ? `${fmt(market.funding_rate * 100, 4)}%` : "--"}</span>
              <span>{fundingCountdown(market?.next_funding_at ?? 0)}</span>
            </div>
            <div className="chart-stack">
              <MiniKline
                symbol={focused}
                live={market?.last_candle ?? null}
                interval={chartInterval}
                lines={chartLines}
              />

            </div>
          </>
        ) : (
          <div className="empty">开仓后这里显示迷你 K 线</div>
        )}
      </footer>
    </div>
    <ResizeHandles />
    </>
  );
}
