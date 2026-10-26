import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ saveCredentials: vi.fn() }));
vi.mock("../lib/api", () => ({ api: { saveCredentials: mocks.saveCredentials } }));

import SetupWizard from "./SetupWizard";

let host: HTMLDivElement;
let root: Root;
const onDone = vi.fn();
const key = "test-only-api-key-123456789";
const secret = "test-only-api-secret-123456789";

beforeEach(async () => {
  vi.resetAllMocks();
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => { root.render(<SetupWizard onDone={onDone} />); });
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
});

function keyInput() {
  return host.querySelector<HTMLInputElement>('[placeholder="API Key"]')!;
}

function secretInput() {
  return host.querySelector<HTMLInputElement>('[placeholder="Secret Key"]')!;
}

function submitButton() {
  return host.querySelector<HTMLButtonElement>("button")!;
}

function confirmations() {
  return [...host.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')];
}

async function fillCredentials() {
  await act(async () => {
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    for (const [input, value] of [[keyInput(), key], [secretInput(), secret]] as const) {
      setValue.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
  });
}

async function confirmAll() {
  for (const checkbox of confirmations()) {
    await act(async () => { checkbox.click(); });
  }
}

describe("首次连接权限边界", () => {
  it("只确认读取及未开交易/提现, 不要求关闭 IP 白名单", () => {
    const labels = [...host.querySelectorAll("label.ack")].map((label) => label.textContent ?? "");
    expect(labels).toHaveLength(3);
    // 先应用本测试到旧组件, 这里应因旧IP强制确认而失败, 不是编译失败.
    expect(labels.join(" ")).not.toMatch(/IP|白名单/);
    expect(labels[0]).toContain("已启用读取权限");
    expect(labels[0]).toContain("只读");
    expect(labels[1]).toContain("未启用交易权限");
    expect(labels[2]).toContain("未启用提现权限");
    expect(host.querySelector(".hint")?.textContent).toContain("建议配置可信出口 IP 白名单, 无需关闭");
    expect(host.querySelector(".hint")?.textContent).toContain("仅验证持仓读取");
    expect(host.querySelector(".hint")?.textContent).toContain("不能完整审计 Key 权限");
    expect(secretInput().type).toBe("password");
  });

  it.each([0, 1, 2])("缺少第 %i 项确认不允许保存, 三项齐全才放行", async (missing) => {
    expect(submitButton().disabled).toBe(true);
    await fillCredentials();
    const checkboxes = confirmations();
    for (const [index, checkbox] of checkboxes.entries()) {
      if (index !== missing) await act(async () => { checkbox.click(); });
    }
    expect(submitButton().disabled).toBe(true);
    await act(async () => { submitButton().click(); });
    expect(mocks.saveCredentials).not.toHaveBeenCalled();
    await act(async () => { checkboxes[missing].click(); });
    expect(submitButton().disabled).toBe(false);
  });

  it("保存中禁止重复提交, 成功清空输入并只通知一次完成", async () => {
    let finish!: () => void;
    mocks.saveCredentials.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
    await fillCredentials();
    await confirmAll();
    await act(async () => { submitButton().click(); });
    expect(mocks.saveCredentials).toHaveBeenCalledExactlyOnceWith(key, secret);
    expect(onDone).not.toHaveBeenCalled();
    expect(submitButton().disabled).toBe(true);
    expect(submitButton().textContent).toBe("校验中...");
    await act(async () => { submitButton().click(); });
    expect(mocks.saveCredentials).toHaveBeenCalledTimes(1);
    await act(async () => { finish(); });
    expect(onDone).toHaveBeenCalledTimes(1);
    expect(keyInput().value).toBe("");
    expect(secretInput().value).toBe("");
    expect(submitButton().disabled).toBe(true);
    expect(host.querySelector(".err")).toBeNull();
  });

  it("保存失败显示错误并保留输入, 重试成功后移除错误", async () => {
    mocks.saveCredentials.mockRejectedValueOnce("持仓读取失败, 请核查 Key 与 IP 白名单")
      .mockResolvedValueOnce(undefined);
    await fillCredentials();
    await confirmAll();
    await act(async () => { submitButton().click(); });
    expect(host.querySelector('[role="alert"]')?.textContent).toContain("持仓读取失败");
    expect(onDone).not.toHaveBeenCalled();
    expect(keyInput().value).toBe(key);
    expect(secretInput().value).toBe(secret);
    expect(submitButton().disabled).toBe(false);
    expect(confirmations().every((checkbox) => checkbox.checked)).toBe(true);
    await act(async () => { submitButton().click(); });
    expect(mocks.saveCredentials).toHaveBeenCalledTimes(2);
    expect(onDone).toHaveBeenCalledTimes(1);
    expect(host.querySelector('[role="alert"]')).toBeNull();
    expect(secretInput().value).toBe("");
  });
});
