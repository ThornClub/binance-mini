import { beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT } from "./types";
const mocks = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }));
beforeEach(() => { vi.resetModules(); vi.resetAllMocks(); });

describe("真实IPC初始化", () => {
  it.each([0, 1, 2, 3])("阶段%s失败清理所有已注册监听, 并发共享失败且可重试", async (stage) => {
    let calls = 0;
    const cleanup: Array<ReturnType<typeof vi.fn>> = [];
    mocks.listen.mockImplementation(async () => {
      if (calls++ === stage) throw new Error("监听失败");
      const stop = vi.fn(); cleanup.push(stop); return stop;
    });
    mocks.invoke.mockRejectedValue(new Error("快照失败"));
    const { initBridge } = await import("./api");
    const first = initBridge(), second = initBridge();
    expect(second).toBe(first);
    await expect(first).rejects.toThrow(stage === 3 ? "快照失败" : "监听失败");
    expect(cleanup).toHaveLength(stage);
    for (const stop of cleanup) expect(stop).toHaveBeenCalledTimes(1);
    mocks.listen.mockImplementation(async () => vi.fn());
    mocks.invoke.mockResolvedValue(EMPTY_SNAPSHOT);
    const retry = initBridge(); expect(retry).not.toBe(first); await retry;
    expect(initBridge()).toBe(retry);
  });
  it("迟到初始快照不能覆盖新revision, 串行订阅不等待历史", async () => {
    let snapshotListener!: (event: { payload: typeof EMPTY_SNAPSHOT }) => void;
    mocks.listen.mockImplementation(async (name, callback) => { if (name === "state://snapshot") snapshotListener = callback; return vi.fn(); });
    let resolveInitial!: (snapshot: typeof EMPTY_SNAPSHOT) => void;
    mocks.invoke.mockImplementation((name) => name === "get_snapshot" ? new Promise((done) => { resolveInitial = done; }) : Promise.resolve());
    const bridge = await import("./api"); const init = bridge.initBridge();
    for (let i = 0; i < 5; i++) await Promise.resolve();
    snapshotListener({ payload: { ...EMPTY_SNAPSHOT, revision: 20, account_generation: 2 } });
    resolveInitial({ ...EMPTY_SNAPSHOT, revision: 10, account_generation: 1 }); await init;
    expect(bridge.getSnapshot().revision).toBe(20);
    let finishFocus!: () => void;
    mocks.invoke.mockImplementation((name) => {
      if (name === "set_focus") return new Promise<void>((done) => { finishFocus = done; });
      if (name === "get_klines") return new Promise(() => {});
      return Promise.resolve();
    });
    const a = bridge.api.setFocus("A", "1m"); await Promise.resolve();
    const token = { account_generation: 2, chart_generation: 1, symbol: "A", interval: "1m" as const };
    void bridge.api.klines(token, 1, 1, 180);
    const clear = bridge.api.clearFocus(token); await Promise.resolve();
    expect(mocks.invoke.mock.calls.filter(([name]) => name === "clear_focus")).toHaveLength(0);
    finishFocus(); await a; await clear;
    expect(mocks.invoke).toHaveBeenCalledWith("clear_focus", { token });
  });
});
