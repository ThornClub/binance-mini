import { useEffect, useState } from "react";
import type { Freshness } from "./types";

// 期限只由后端调度提供, 前端不再维护另一套15秒规则.
export function isStale(source: Freshness | undefined, now: number): boolean {
  return !source || source.last_success_ms <= 0 || source.error != null ||
    now - source.last_success_ms > source.max_age_ms;
}
export function freshnessLabel(source: Freshness | undefined, now: number): string {
  if (!source || source.last_success_ms <= 0) return source?.error ?? "等待首次有效数据";
  const age = Math.max(0, Math.floor((now - source.last_success_ms) / 1000));
  return `${source.error ? `${source.error}; ` : ""}${age}秒前更新${isStale(source, now) ? ", 数据已过期" : ""}`;
}
export function useNow(): number {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const update = () => setNow(Date.now());
    const timer = window.setInterval(update, 1000);
    document.addEventListener("visibilitychange", update);
    return () => { window.clearInterval(timer); document.removeEventListener("visibilitychange", update); };
  }, []);
  return now;
}
