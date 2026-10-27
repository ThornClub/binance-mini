import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import type { ClosePlan, PositionView } from "../lib/types";

interface Props {
  position: PositionView;
  /** 持仓风险数据过期时不允许平仓, 避免用旧数量下真实订单. */
  stale: boolean;
  onDone: (message: string) => void;
  onCancel: () => void;
}

const PREVIEW_DEBOUNCE_MS = 250;

function fmtNotional(v: number) {
  return Number.isFinite(v) ? v.toFixed(2) : "--";
}

/**
 * 仓位行内的市价部分平仓面板.
 * 首次打开只显示预览, 点"确认平仓"才发单; 发单期间锁死, 同一面板不会重复提交.
 */
export default function PositionClose({ position, stale, onDone, onCancel }: Props) {
  const [draft, setDraft] = useState(String(position.qty));
  const [plan, setPlan] = useState<ClosePlan | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const sendingRef = useRef(false);
  const panelRef = useRef<HTMLDivElement>(null);
  const key = `${position.symbol}|${position.side}`;
  const keyRef = useRef(key);
  keyRef.current = key;

  // 面板在可滚动的持仓区里, 展开后必须自己滚进可视范围, 否则底部队会被裁掉.
  useEffect(() => {
    const panel = panelRef.current;
    // jsdom 与部分旧 WebView 没有 scrollIntoView, 缺失时只影响滚动, 不影响面板.
    if (panel && typeof panel.scrollIntoView === "function") panel.scrollIntoView({ block: "nearest" });
  }, []);

  const qty = Number(draft);
  useEffect(() => {
    if (stale) return;
    if (!draft.trim() || !Number.isFinite(qty) || qty <= 0) { setPlan(null); setPreviewError("请输入大于 0 的数量"); return; }
    let cancelled = false;
    const timer = setTimeout(() => {
      void api.closePreview(position.symbol, position.side, qty).then((next) => {
        if (cancelled || keyRef.current !== key) return;
        setPlan(next); setPreviewError(null);
      }).catch((reason) => {
        if (cancelled) return;
        setPlan(null); setPreviewError(String(reason));
      });
    }, PREVIEW_DEBOUNCE_MS);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [draft, qty, stale, position.symbol, position.side, key]);

  async function confirm() {
    // 同一面板只允许一次提交: 双击、回车连按都不能变成两笔真实订单.
    if (sendingRef.current || !plan || stale) return;
    sendingRef.current = true;
    setSending(true); setError(null);
    try {
      const result = await api.closePosition(position.symbol, position.side, plan.qty);
      const price = result.avg_price > 0 ? ` @ ${result.avg_price}` : "";
      onDone(`${result.qty_text} ${position.symbol} 已提交, 状态 ${result.status}${price}`);
    } catch (reason) {
      setError(String(reason));
    } finally {
      sendingRef.current = false;
      setSending(false);
    }
  }

  return <div className="pos-close-panel" role="group" aria-label={`平仓 ${position.symbol}`} ref={panelRef}
    onClick={(event) => event.stopPropagation()}>
    <div className="pos-close-head">
      <span>市价平仓 {position.symbol}</span>
      <span className="pos-close-max">持仓 {position.qty}</span>
    </div>
    <div className="pos-close-row">
      <input aria-label="平仓数量" inputMode="decimal" value={draft} disabled={sending}
        onChange={(event) => setDraft(event.target.value)} />
      <button className="pos-close-confirm" disabled={sending || stale || !plan} onClick={() => { void confirm(); }}
        title={stale ? "风险数据过期, 先等持仓刷新" : undefined}>
        {sending ? "发单中..." : "确认平仓"}
      </button>
      <button className="pos-close-cancel" disabled={sending} onClick={onCancel}>取消</button>
    </div>
    {stale && <div className="err">风险数据过期, 先等持仓刷新再平仓</div>}
    {plan && !stale && <div className="pos-close-hint">
      {plan.reduce_only ? "单向 · reduceOnly" : `双向 · ${plan.position_side}`} · 数量 {plan.qty_text} ·
      约 {fmtNotional(plan.notional)} USDT @ {plan.mark_price}
    </div>}
    {!plan && previewError && !stale && <div className="err" role="alert">{previewError}</div>}
    {error && <div className="err" role="alert">{error}</div>}
  </div>;
}
