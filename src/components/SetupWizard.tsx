import { useState } from "react";
import { api } from "../lib/api";

/** 首次启动向导: 用户确认权限边界, 接口自检不能替代交易所侧权限核查. */
export default function SetupWizard({ onDone }: { onDone: () => void }) {
  const [key, setKey] = useState("");
  const [secret, setSecret] = useState("");
  const [ackRead, setAckRead] = useState(false);
  const [ackNoTrade, setAckNoTrade] = useState(false);
  const [ackWithdraw, setAckWithdraw] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit = key.trim().length > 20 && secret.trim().length > 20 && ackRead && ackNoTrade && ackWithdraw && !busy;

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
        建议配置可信出口 IP 白名单, 无需关闭. 接口自检仅验证持仓读取, 不能完整审计 Key 权限.
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
        已启用读取权限, 使用只读 Key
      </label>
      <label className="ack">
        <input type="checkbox" checked={ackNoTrade} onChange={(e) => setAckNoTrade(e.target.checked)} />
        未启用交易权限
      </label>
      <label className="ack">
        <input type="checkbox" checked={ackWithdraw} onChange={(e) => setAckWithdraw(e.target.checked)} />
        未启用提现权限
      </label>

      {error && <div className="err" role="alert">{error}</div>}

      <button disabled={!canSubmit} onClick={submit}>
        {busy ? "校验中..." : "保存并连接"}
      </button>
    </div>
  );
}
