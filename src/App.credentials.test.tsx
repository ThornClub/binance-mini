import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT } from "./lib/types";

const mocks = vi.hoisted(() => ({
  api: {
    hasCredentials: vi.fn(),
    alerts: vi.fn(),
    autostartEnabled: vi.fn(),
    clearCredentials: vi.fn(),
  },
}));

vi.mock("./lib/api", () => ({
  api: mocks.api,
  initBridge: async () => {},
  getSnapshot: () => EMPTY_SNAPSHOT,
  subscribeSnapshot: () => () => {},
}));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: vi.fn() }));
vi.mock("./components/MiniKline", () => ({ default: () => null }));

import App from "./App";

let host: HTMLDivElement;
let root: Root;

beforeEach(async () => {
  vi.resetAllMocks();
  localStorage.clear();
  mocks.api.hasCredentials.mockResolvedValue(true);
  mocks.api.alerts.mockResolvedValue([]);
  mocks.api.autostartEnabled.mockResolvedValue(false);
  mocks.api.clearCredentials.mockResolvedValue(undefined);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => { root.render(<App />); });
  await toggleSettings();
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
});

async function toggleSettings() {
  await act(async () => { host.querySelector<HTMLButtonElement>('[title="设置"]')!.click(); });
}
function button(label: string) {
  const found = [...host.querySelectorAll("button")].find((item) => item.textContent?.trim() === label);
  expect(found).toBeDefined();
  return found!;
}
async function clickButton(label: string) {
  await act(async () => { button(label).click(); });
}
async function confirmClear() {
  await clickButton("清除密钥");
  await clickButton("确认清除");
}

describe("清除凭据的真实界面", () => {
  it("首次点击仅显示行内确认与影响, 不调用后端", async () => {
    await clickButton("清除密钥");
    expect(mocks.api.clearCredentials).not.toHaveBeenCalled();
    expect(host.querySelector(".credential-confirm")?.textContent).toContain("停止本机连接");
    expect(host.querySelector(".credential-confirm")?.textContent).toContain("重新录入");
    expect(button("确认清除")).toBeDefined(); expect(button("取消")).toBeDefined();
    expect(host.querySelector('[role="dialog"]')).toBeNull();
  });

  it("取消不调用后端且保留连接", async () => {
    await clickButton("清除密钥"); await clickButton("取消");
    expect(mocks.api.clearCredentials).not.toHaveBeenCalled();
    expect(host.querySelector(".credential-confirm")).toBeNull();
    expect(host.querySelector("h1")).toBeNull();
    expect(host.querySelector(".settings")).not.toBeNull();
  });

  it("Escape取消确认, 再次清除需要重新确认", async () => {
    await clickButton("清除密钥");
    await act(async () => { host.querySelector(".settings")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); });
    expect(host.querySelector(".credential-confirm")).toBeNull();
    expect(mocks.api.clearCredentials).not.toHaveBeenCalled();
    await clickButton("清除密钥");
    expect(button("确认清除")).toBeDefined();
    expect(mocks.api.clearCredentials).not.toHaveBeenCalled();
  });

  it("关闭再打开设置不保留已武装确认", async () => {
    await clickButton("清除密钥"); await toggleSettings(); await toggleSettings();
    expect(host.querySelector(".credential-confirm")).toBeNull();
    expect(mocks.api.clearCredentials).not.toHaveBeenCalled();
    expect(button("清除密钥")).toBeDefined();
  });

  it("确认在途时连续点击只清除一次", async () => {
    let finish!: () => void;
    mocks.api.clearCredentials.mockReturnValue(new Promise<void>((resolve) => { finish = resolve; }));
    await clickButton("清除密钥");
    const confirm = button("确认清除");
    await act(async () => { confirm.click(); confirm.click(); });
    expect(mocks.api.clearCredentials).toHaveBeenCalledTimes(1);
    await act(async () => { finish(); });
    expect(host.querySelector("h1")?.textContent).toBe("连接币安合约");
  });

  it("删除失败后切回未连接状态并保留可见错误与重试入口", async () => {
    const error = "内存会话已停用, 系统凭据删除失败: 访问被拒绝";
    mocks.api.clearCredentials.mockRejectedValue(error);
    mocks.api.hasCredentials.mockResolvedValue(false);
    await confirmClear();
    expect(host.querySelector("h1")?.textContent).toBe("连接币安合约");
    expect(host.querySelector('[role="alert"]')?.textContent).toContain(error);
    expect(host.textContent).toContain("重试清除");
    expect(mocks.api.hasCredentials).toHaveBeenCalledTimes(2);
  });

  it("重试删除成功后移除残留风险提示", async () => {
    mocks.api.clearCredentials.mockRejectedValueOnce("系统凭据删除失败").mockResolvedValueOnce(undefined);
    mocks.api.hasCredentials.mockResolvedValue(false);
    await confirmClear();
    await clickButton("重试清除");
    expect(host.querySelector("h1")?.textContent).toBe("连接币安合约");
    expect(host.querySelector('[role="alert"]')).toBeNull();
    expect(mocks.api.clearCredentials).toHaveBeenCalledTimes(2);
  });

  it("清除成功直接回到向导且不显示失败", async () => {
    mocks.api.clearCredentials.mockResolvedValue(undefined);
    await confirmClear();
    expect(host.querySelector("h1")?.textContent).toBe("连接币安合约");
    expect(host.querySelector('[role="alert"]')).toBeNull();
  });
});
