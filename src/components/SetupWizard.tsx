import { useState } from "react";
import { api } from "../lib/api";

/** 首次启动向导: 强制确认密钥权限边界, 避免用户拿一个能下单的 Key 上来就用. */
export default function SetupWizard({ onDone }: { onDone: () => void }) {
  const [key, setKey] = useState("");
  const [secret, setSecret] = useState("");
  const [ackRead, setAckRead] = useState(false);
  const [ackNoIp, setAckNoIp] = useState(false);
  const [ackWithdraw, setAckWithdraw] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit = key.trim().length > 20 && secret.trim().length > 20 && ackRead && ackNoIp && ackWithdraw && !busy;

  async function submit() {
    setBusy(true);
    setError(null);
    try {
      await api.saveCredentials(key, secret);
      setKey("");
      setSecret("");
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="setup">
      <h1>连接币安合约</h1>
      <p className="hint">
        密钥只存进 Windows 凭据管理器, 不写文件, 不出现在日志里.
      </p>

      <input
        placeholder="API Key"
        value={key}
        onChange={(e) => setKey(e.target.value)}
        spellCheck={false}
        autoComplete="off"
      />
      <input
        placeholder="Secret Key"
        type="password"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        spellCheck={false}
        autoComplete="off"
      />

      <label className="ack">
        <input type="checkbox" checked={ackRead} onChange={(e) => setAckRead(e.target.checked)} />
        已勾选「启用读取」
      </label>
      <label className="ack">
        <input type="checkbox" checked={ackNoIp} onChange={(e) => setAckNoIp(e.target.checked)} />
        不配 IP 白名单, 我知道 Secret 泄露后别人能下单
      </label>
      <label className="ack">
        <input type="checkbox" checked={ackWithdraw} onChange={(e) => setAckWithdraw(e.target.checked)} />
        未勾选「允许提现」
      </label>

      {error && <div className="err">{error}</div>}

      <button disabled={!canSubmit} onClick={submit}>
        {busy ? "校验中..." : "保存并连接"}
      </button>
    </div>
  );
}
