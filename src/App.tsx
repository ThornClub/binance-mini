import { useEffect, useId, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, getSnapshot, initBridge, subscribeSnapshot } from "./lib/api";
import type { AlertLine, PositionView } from "./lib/types";
import { freshnessLabel, isStale, useNow } from "./lib/freshness";
import SetupWizard from "./components/SetupWizard";
import PositionClose from "./components/PositionClose";
import MiniKline, { type ChartInterval, type ChartLine } from "./components/MiniKline";
import "./App.css";
import "./components/chart-state.css";

type ResizeDir = "North" | "South" | "West" | "East" | "NorthWest" | "NorthEast" | "SouthWest" | "SouthEast";
const RESIZE_ZONES: { dir: ResizeDir; cls: string }[] = [
  { dir: "North", cls: "n" }, { dir: "South", cls: "s" }, { dir: "West", cls: "w" }, { dir: "East", cls: "e" },
  { dir: "NorthWest", cls: "nw" }, { dir: "NorthEast", cls: "ne" }, { dir: "SouthWest", cls: "sw" }, { dir: "SouthEast", cls: "se" },
];
function ResizeHandles() {
  return <>{RESIZE_ZONES.map((z) => <div key={z.cls} className={`rz ${z.cls}`} onMouseDown={(e) => {
    if (e.button !== 0) return;
    e.preventDefault(); void getCurrentWindow().startResizeDragging(z.dir);
  }} />)}</>;
}
function fmt(v: number, digits = 2) {
  if (!Number.isFinite(v)) return "--";
  return v.toLocaleString("en-US", { minimumFractionDigits: digits, maximumFractionDigits: digits });
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
function positionKey(p: PositionView) { return `${p.symbol}|${p.side}`; }
function fundingCountdown(ts: number, now: number) {
  if (!ts) return "--:--";
  const ms = ts - now;
  if (ms <= 0) return "结算中";
  return `${Math.floor(ms / 3600000)}h${String(Math.floor((ms % 3600000) / 60000)).padStart(2, "0")}m`;
}

function AlertComposer({ symbol, onSaved }: { symbol: string; onSaved: (alerts: AlertLine[]) => void }) {
  const [open, setOpen] = useState(false);
  const [price, setPrice] = useState("");
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const inFlight = useRef(false);
  const errorId = useId();
  const error = fieldError ?? saveError;
  async function save() {
    if (inFlight.current) return;
    const amount = Number(price);
    if (!price.trim() || !Number.isFinite(amount) || amount <= 0) {
      setFieldError("请输入大于 0 的有效价格"); return;
    }
    // ref先于React批量提交生效, 同一事件周期的Enter和点击也只能保存一次.
    inFlight.current = true;
    setSaving(true); setFieldError(null); setSaveError(null);
    try { onSaved(await api.addAlert(symbol, amount)); setPrice(""); setOpen(false); }
    catch (error) { setSaveError(String(error)); }
    finally { inFlight.current = false; setSaving(false); }
  }
  return <div className="alert-composer">{open ? <>
    <div className="alert-add" aria-busy={saving}>
      <input autoFocus value={price} disabled={saving} inputMode="decimal" placeholder="价格" aria-label={`${symbol} 告警价格`}
        aria-invalid={fieldError != null} aria-describedby={error ? errorId : undefined}
        onChange={(e) => { setPrice(e.target.value); setFieldError(null); setSaveError(null); }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.nativeEvent.isComposing) { e.preventDefault(); void save(); }
          if (e.key === "Escape" && !inFlight.current) { setPrice(""); setFieldError(null); setSaveError(null); setOpen(false); }
        }} />
      <button disabled={saving} onClick={() => { void save(); }} aria-label={saving ? "正在保存告警" : "保存告警"}>{saving ? "保存中..." : "✓"}</button>
    </div>
    {error && <div id={errorId} className="err" role="alert">{error}</div>}
  </> : <button className="alert-btn" onClick={() => setOpen(true)}>＋告警</button>}</div>;
}

export default function App() {
  const snap = useSyncExternalStore(subscribeSnapshot, getSnapshot);
  const now = useNow();
  const [hasCreds, setHasCreds] = useState<boolean | null>(null);
  const [focus, setFocus] = useState<string | null>(null);
  const [alerts, setAlerts] = useState<AlertLine[]>([]);
  const [alertError, setAlertError] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [chartInterval, setChartInterval] = useState<ChartInterval>("time");
  const [autoStart, setAutoStart] = useState(false);
  const [liqDraft, setLiqDraft] = useState<string | null>(null);
  const [liqConfirmed, setLiqConfirmed] = useState(snap.risk.liq_warn_pct);
  const [riskSaving, setRiskSaving] = useState(false);
  // 只允许一个仓位行处于平仓面板展开状态, 避免多行同时确认.
  const [closeKey, setCloseKey] = useState<string | null>(null);
  const [closeMsg, setCloseMsg] = useState<string | null>(null);
  const [riskError, setRiskError] = useState<string | null>(null);
  const [checkMsg, setCheckMsg] = useState<string | null>(null);
  const [credentialError, setCredentialError] = useState<string | null>(null);
  const [confirmingClear, setConfirmingClear] = useState(false);
  const [clearingCredentials, setClearingCredentials] = useState(false);
  const clearInFlight = useRef(false);
  const [initAttempt, setInitAttempt] = useState(0);
  const [initError, setInitError] = useState<string | null>(null);
  const [initialized, setInitialized] = useState(false);
  const [bgAlpha, setBgAlpha] = useState(() => {
    const saved = Number(localStorage.getItem("bgAlpha.v4"));
    return Number.isFinite(saved) && saved >= 0.3 && saved <= 1 ? saved : 1;
  });
  const alphaStyle = { "--bg-alpha": String(bgAlpha) } as CSSProperties;
  function applyAlpha(v: number) { setBgAlpha(v); localStorage.setItem("bgAlpha.v4", String(v)); }

  useEffect(() => {
    let disposed = false;
    setInitError(null);
    setInitialized(false);
    void (async () => {
      try {
        await initBridge();
        const [available, savedAlerts] = await Promise.all([api.hasCredentials(), api.alerts()]);
        if (disposed) return;
        setHasCreds(available); setAlerts(savedAlerts); setInitialized(true);
      } catch (error) { if (!disposed) setInitError(String(error)); }
    })();
    void api.autostartEnabled().then((on) => { if (!disposed) setAutoStart(on); }).catch(() => {});
    return () => { disposed = true; };
  }, [initAttempt]);
  useEffect(() => { setLiqConfirmed(snap.risk.liq_warn_pct); }, [snap.risk.liq_warn_pct]);

  async function saveRisk() {
    if (liqDraft == null || riskSaving) return;
    setCheckMsg(null);
    const pct = Number(liqDraft);
    if (!liqDraft.trim() || !Number.isFinite(pct) || pct < 1 || pct > 50) {
      setRiskError("请输入 1-50 之间的强平告警阈值"); return;
    }
    setRiskError(null);
    if (pct === liqConfirmed) { setLiqDraft(null); return; }
    setRiskSaving(true);
    try {
      const confirmed = await api.setRisk(pct);
      setLiqConfirmed(confirmed.liq_warn_pct); setLiqDraft(null);
      setCheckMsg(`强平距离低于 ${confirmed.liq_warn_pct}% 时通知`);
    } catch (error) { setRiskError(String(error)); }
    finally { setRiskSaving(false); }
  }

  const position = snap.positions.find((p) => positionKey(p) === focus) ?? snap.positions[0] ?? null;
  const focused = position?.symbol ?? null;
  const selectedKey = position ? positionKey(position) : null;
  // 初始自动选中也必须落成稳定的symbol+Side, 排名变化不能改变方向.
  useEffect(() => { if (focus !== selectedKey) setFocus(selectedKey); }, [focus, selectedKey]);
  const market = snap.markets.find((m) => m.symbol === focused) ?? null;
  const focusedAlerts = alerts.filter((a) => a.symbol === focused);
  const chartLines: ChartLine[] = useMemo(() => {
    const out: ChartLine[] = [];
    if (position) {
      out.push({ price: position.entry_price, color: "rgba(233,237,243,0.45)", title: "开", dashed: true });
      if (position.liq_price) out.push({ price: position.liq_price, color: "#f6465d", title: "强平", dashed: true });
      if (position.tp_price) out.push({ price: position.tp_price, color: "#0ecb81", title: "止盈" });
      if (position.sl_price) out.push({ price: position.sl_price, color: "#f6465d", title: "止损" });
    }
    for (const a of alerts.filter((a) => a.symbol === focused && a.enabled))
      out.push({ price: a.price, color: "#f0b90b", title: "告警", dashed: true });
    return out;
  }, [position, alerts, focused]);
  const riskStale = isStale(snap.position_freshness, now);
  const accountStale = isStale(snap.account_freshness, now);
  const marketStale = snap.positions.some((p) => isStale(snap.markets.find((m) => m.symbol === p.symbol)?.freshness, now));
  const totalUpnl = snap.positions.reduce((sum, p) => sum + p.upnl, 0);
  const dotState = !snap.health.user_stream_ok ? "down" : riskStale || accountStale || marketStale ? "stale" : "ok";

  async function refreshAlerts() {
    try { setAlerts(await api.alerts()); } catch (error) { setAlertError(String(error)); }
  }
  async function removeAlert(id: string) {
    setAlertError(null);
    try { setAlerts(await api.removeAlert(id)); } catch (error) { setAlertError(String(error)); }
  }
  async function clearCredentials() {
    if (clearInFlight.current) return;
    clearInFlight.current = true;
    setConfirmingClear(false); setClearingCredentials(true); setCredentialError(null);
    try { await api.clearCredentials(); setHasCreds(false); setShowSettings(false); }
    catch (error) {
      setCredentialError(String(error));
      try {
        const available = await api.hasCredentials();
        setHasCreds(available); if (!available) setShowSettings(false);
      } catch { setCredentialError(`${String(error)}; 无法确认当前连接状态, 请重试清除.`); }
    } finally { clearInFlight.current = false; setClearingCredentials(false); }
  }
  const credentialNotice = credentialError && <div className="err" role="alert">
    {credentialError}<button disabled={clearingCredentials} onClick={clearCredentials}>重试清除</button>
  </div>;
  if (!initialized) return <><div className="widget setup-mode" style={alphaStyle}>
    <div className="setup" role={initError ? "alert" : "status"}>
      {initError ? `初始化失败: ${initError}` : "正在连接本地数据服务..."}
      {initError && <button onClick={() => setInitAttempt((n) => n + 1)}>重试初始化</button>}
    </div>
  </div><ResizeHandles /></>;
  if (hasCreds === false) return <><div className="widget setup-mode" style={alphaStyle}>
    {credentialNotice}<SetupWizard onDone={() => { setHasCreds(true); setCredentialError(null); void refreshAlerts(); }} />
  </div><ResizeHandles /></>;

  return <><div className="widget" style={alphaStyle}>
    <header data-tauri-drag-region="deep">
      <span className={`dot ${dotState}`} title={snap.health.last_error ?? "连接正常"} />
      <span className={`hero${riskStale || marketStale ? " data-stale" : ""}`} title={`持仓风险: ${freshnessLabel(snap.position_freshness, now)}`}>
        <b className={totalUpnl >= 0 ? "up" : "down"}>{totalUpnl >= 0 ? "+" : ""}{fmt(totalUpnl, 2)}</b><em>未实现盈亏</em>
      </span>
      <span className={`mr${accountStale ? " data-stale" : ""}`} title={`账户: ${freshnessLabel(snap.account_freshness, now)}`}>
        <i>维持{accountStale ? "(过期)" : ""}</i><b>{snap.account ? `${fmt(snap.account.margin_ratio, 1)}%` : "--"}</b>
      </span>
      <button className="ghost" onClick={() => { setConfirmingClear(false); setShowSettings((v) => !v); }} title="设置">⚙</button>
      <button className="ghost" onClick={() => api.hideWindow()} title="隐藏 (托盘恢复)">✕</button>
    </header>
    {showSettings && <div className="settings" onKeyDown={(e) => { if (e.key === "Escape") setConfirmingClear(false); }}>
      <div className="row"><button onClick={() => api.setAlwaysOnTop(true)}>置顶</button><button onClick={() => api.setAlwaysOnTop(false)}>取消置顶</button></div>
      <div className="row"><label className="alpha">不透明度<input type="range" min={30} max={100} step={5} value={Math.round(bgAlpha * 100)} onChange={(e) => applyAlpha(Number(e.target.value) / 100)} /><span className="val">{Math.round(bgAlpha * 100)}%</span></label></div>
      <div className="row"><label className="alpha">强平告警<input type="number" min={1} max={50} step="any" aria-label="强平告警阈值"
        aria-invalid={riskError != null} aria-describedby={riskError ? "risk-error" : undefined} disabled={riskSaving}
        value={liqDraft ?? String(liqConfirmed)} onChange={(e) => { setLiqDraft(e.target.value); setRiskError(null); setCheckMsg(null); }} onBlur={saveRisk} /><span className="val">%</span></label></div>
      {riskError && <div id="risk-error" className="err" role="alert">{riskError}</div>}
      <div className="row">
        <button onClick={async () => { const next = !autoStart; try { await api.setAutostart(next); setAutoStart(next); setCheckMsg(next ? "已开启开机自启" : "已关闭开机自启"); } catch (e) { setCheckMsg(String(e)); } }}>开机自启 {autoStart ? "开" : "关"}</button>
        <button onClick={async () => { setCheckMsg("检测中..."); try { setCheckMsg(await api.selfCheck()); } catch (e) { setCheckMsg(String(e)); } }}>权限自检</button>
      </div>
      <div className="row"><button className="danger" disabled={clearingCredentials} onClick={() => setConfirmingClear(true)}>{clearingCredentials ? "清除中..." : "清除密钥"}</button></div>
      {confirmingClear && <div className="credential-confirm">
        <p id="clear-credentials-note">清除后会停止本机连接, 需要重新录入密钥.</p>
        <div className="row"><button className="danger" aria-describedby="clear-credentials-note" disabled={clearingCredentials} onClick={clearCredentials}>确认清除</button>
          <button onClick={() => setConfirmingClear(false)}>取消</button></div>
      </div>}
      <section className="all-alerts" aria-label="全部价格告警">
        <div>全部价格告警</div>
        {alerts.length === 0 ? <span>暂无价格告警</span> : alerts.map((a) => <div className="row" key={a.id}>
          <span>{a.symbol} · {fmtPrice(a.price)}{a.enabled ? "" : " · 已停用"}</span>
          <button onClick={() => removeAlert(a.id)} aria-label={`删除 ${a.symbol} ${a.price} 告警`}>删除</button>
        </div>)}
        {alertError && <div className="err" role="alert">{alertError}</div>}
      </section>
      <div className="row"><button onClick={() => api.quit()}>退出并停止监控</button></div>
      {checkMsg && <div className="ok-msg">{checkMsg}</div>}
      {snap.health.last_error && <div className="err">{snap.health.last_error}</div>}{credentialNotice}
    </div>}
    <main>
      {snap.positions.length === 0 && <div className="empty">{snap.position_freshness.last_success_ms ? "暂无持仓" : "正在加载持仓"}</div>}
      {snap.positions.map((p) => {
        const source = snap.markets.find((m) => m.symbol === p.symbol)?.freshness;
        const stale = isStale(source, now);
        return <div key={positionKey(p)} className={`pos ${positionKey(p) === selectedKey ? "focused" : ""}`}
          role="button" tabIndex={0} aria-pressed={positionKey(p) === selectedKey}
          onClick={() => setFocus(positionKey(p))} onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); setFocus(positionKey(p)); } }}>
          <div className="pos-l"><div className="pos-sym"><b>{p.symbol.replace("USDT", "")}</b><span className={`badge ${sideLabel(p) === "多" ? "long" : "short"}`}>{sideLabel(p)}<em>{p.leverage}x</em></span></div>
            <div className={`pos-px${stale ? " data-stale" : ""}`} title={freshnessLabel(source, now)}><span>{fmtPrice(p.entry_price)}</span><i>→</i><span className="mark">{fmtPrice(p.mark_price)}{stale ? " (过期)" : ""}</span></div>
          </div>
          <div className={`pos-r${stale || riskStale ? " data-stale" : ""}`}><b className={p.upnl >= 0 ? "up" : "down"}>{p.upnl >= 0 ? "+" : ""}{fmt(p.upnl, 2)}</b><span className={p.upnl_pct >= 0 ? "up" : "down"}>{fmt(p.upnl_pct, 1)}%</span>
            {/* 平仓入口只在风险数据可信时开放, 避免用过期数量下真实订单. */}
            <button className="pos-close-open" disabled={riskStale}
              title={riskStale ? "风险数据过期, 先等持仓刷新" : "市价平仓"}
              onClick={(event) => {
                event.stopPropagation();
                setCloseMsg(null);
                setCloseKey(closeKey === positionKey(p) ? null : positionKey(p));
              }}>平仓</button>
          </div>
          <div title={freshnessLabel(snap.position_freshness, now)} className={`pos-liq${riskStale ? " data-stale" : ""}${p.liq_distance_pct != null && p.liq_distance_pct <= snap.risk.liq_warn_pct ? " danger" : ""}`}>
            强平{riskStale ? "(风险数据过期)" : ""} {p.liq_price ? `${fmtPrice(p.liq_price)} · 距离 ${fmt(p.liq_distance_pct ?? 0, 1)}%` : "--"}
          </div>
          {closeKey === positionKey(p) && <PositionClose key={`${snap.account_generation}|${positionKey(p)}`} position={p}
            stale={riskStale} onCancel={() => setCloseKey(null)}
            onDone={(message) => { setCloseKey(null); setCloseMsg(message); }} />}
        </div>;
      })}
      {closeMsg && <div className="pos-close-msg" role="status">平仓: {closeMsg}<button onClick={() => setCloseMsg(null)} aria-label="关闭平仓提示">✕</button></div>}
    </main>
    <footer>{focused ? <>
      <div className="chart-head"><span className="chart-title"><span className="sym">{focused}</span>
        {position && <span className={`badge ${sideLabel(position) === "多" ? "long" : "short"}`} aria-label="已选持仓方向">{sideLabel(position)}</span>}</span>
        <div className="seg">{(["time", "15m", "1h", "1d"] as ChartInterval[]).map((k) => <button key={k} className={chartInterval === k ? "on" : ""} onClick={() => setChartInterval(k)}>{k === "time" ? "分时" : k === "1d" ? "1日" : k}</button>)}</div>
        <div className="alert-zone">
          {focusedAlerts.length > 0 && <div className="alert-chips" role="group" aria-label="当前合约价格告警">{focusedAlerts.map((a) => <span className="alert-chip" key={a.id}>{fmtPrice(a.price)}<button onClick={() => removeAlert(a.id)} title="删除告警" aria-label={`删除 ${a.symbol} ${a.price} 告警`}>✕</button></span>)}</div>}
          {/* 按账户和合约隔离输入实例, 旧提交不能清除切换后的新草稿. */}
          <AlertComposer key={`${snap.account_generation}|${focused}`} symbol={focused} onSaved={(saved) => {
            if (getSnapshot().account_generation === snap.account_generation) setAlerts(saved);
          }} />
          {!showSettings && alertError && <span className="err" role="alert">{alertError}</span>}
        </div>
      </div>
      <div className="chart-sub"><span className="market-summary">24h 高 <b>{market ? fmtPrice(market.high_24h) : "--"}</b></span><span className="market-summary">低 <b>{market ? fmtPrice(market.low_24h) : "--"}</b></span>
        <span className={`market-summary ${(market?.change_pct_24h ?? 0) >= 0 ? "up" : "down"}`}>{market ? `${market.change_pct_24h >= 0 ? "+" : ""}${fmt(market.change_pct_24h, 2)}%` : "--"}</span>
        <span className="grow" /><span>费率 {market ? `${fmt(market.funding_rate * 100, 4)}%` : "--"}</span><span>{fundingCountdown(market?.next_funding_at ?? 0, now)}</span>
      </div>
      <div className="chart-stack"><MiniKline key={snap.account_generation} symbol={focused} interval={chartInterval} lines={chartLines} /></div>
    </> : <div className="empty">开仓后这里显示迷你 K 线</div>}</footer>
  </div><ResizeHandles /></>;
}
