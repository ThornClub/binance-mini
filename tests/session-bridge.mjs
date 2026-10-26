// 单次断言自检, 转译真实bridge和图表缓存, 不访问浏览器或音频设备.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import ts from "typescript";

const events = new Map();
let resolveInitial;
const initial = new Promise((resolve) => { resolveInitial = resolve; });
let markRequested;
const requested = new Promise((resolve) => { markRequested = resolve; });
let soundAttempts = 0;
function load(relative, imports = {}) {
  const source = readFileSync(new URL(relative, import.meta.url), "utf8");
  const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } });
  const exports = {};
  vm.runInNewContext(outputText, {
    exports,
    require(name) { assert.ok(Object.hasOwn(imports, name), `未配置的模块: ${name}`); return imports[name]; },
    window: { addEventListener() {} },
    setInterval, clearInterval,
    AudioContext: class { constructor() { soundAttempts++; throw new Error("测试不访问音频设备"); } },
  }, { filename: relative });
  return exports;
}
const types = load("../src/lib/types.ts");
const chart = load("../src/lib/chart-session.ts");
const bridge = load("../src/lib/api.ts", {
  "./types": types,
  "./chart-session": chart,
  "@tauri-apps/api/core": { invoke() { markRequested(); return initial; } },
  "@tauri-apps/api/event": { async listen(name, handler) { events.set(name, handler); return () => events.delete(name); } },
});
const snapshot = (revision, generation) => ({ ...types.EMPTY_SNAPSHOT, revision, account_generation: generation });
const deliver = (name, payload) => events.get(name)({ payload });
const initialized = bridge.initBridge();
assert.equal(bridge.initBridge(), initialized);
await requested;
deliver("state://snapshot", snapshot(10, 1));
deliver("state://alert", { symbol: "A", price: 1, account_generation: 1 });
assert.equal(bridge.lastAlert.symbol, "A");
assert.equal(soundAttempts, 1);
deliver("state://snapshot", snapshot(11, 2));
assert.equal(bridge.lastAlert, null);
deliver("state://snapshot", snapshot(12, 3));
deliver("state://alert", { symbol: "A", price: 1, account_generation: 1 });
deliver("state://liq", { symbol: "A", side: "Long", distance: 1, account_generation: 1 });
assert.equal(bridge.lastAlert, null);
assert.equal(bridge.lastLiqAlert, null);
assert.equal(soundAttempts, 1);
resolveInitial(snapshot(9, 1));
await initialized;
assert.equal(bridge.getSnapshot().revision, 12);
assert.equal(bridge.getSnapshot().account_generation, 3);
deliver("state://snapshot", snapshot(10, 1));
assert.equal(bridge.getSnapshot().revision, 12);
console.log("会话快照、共享初始化与迟到通知断言通过");
