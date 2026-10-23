# binance-mini

Windows 桌面常驻的币安 USDT-M 永续合约盯盘小窗. 置顶、无边框、可一键隐藏, 抬眼两秒就能看到持仓风险状态.

只用只读 API Key, 不下单, 不碰任何写接口.

## 功能

- 持仓面板: 交易对、方向、杠杆、开仓价、标记价、未实现盈亏 (金额 + 百分比)、强平距离
- 双向持仓按 `positionSide` 拆行, 同一合约的 LONG / SHORT 分别显示
- 风险信息: 保证金率、资金费率、下次结算倒计时
- 迷你 K 线: 分时 / 15m / 1h / 1d, 可滚轮缩放、拖动平移, 图上画开仓价、强平价、止盈止损、告警线
- 价格线告警: 穿越判定 + 5 分钟冷却, 触发时发 Windows 通知 + 提示音
- 连接健康度: 私有流与行情流分开监测, 断线自动重连 (指数退避), 数据超过 15 秒未更新整块标灰
- 托盘图标: 显示 / 隐藏、切换置顶、退出; 全局快捷键 `Ctrl+Alt+B` 显隐
- 单实例: 重复启动只会聚焦已有窗口, 不会开出第二个进程
- 窗口位置尺寸、不透明度 (30%–100%)、开机自启都会持久化
- 密钥存 Windows 凭据管理器, 不落明文配置文件

## 安装

从 [Releases](https://github.com/ThornClub/binance-mini/releases) 下载:

- `BinanceMini_0.1.0_x64-setup.exe` — 安装包, 用户级安装, 不需要管理员权限
- `binance-mini.exe` — 免安装, 直接双击运行

首次启动会要求填 API Key / Secret. Key 只需开启**读取**权限, 不需要交易和提现权限.

## 开发

前置:

| 组件 | 版本 |
| --- | --- |
| Node.js | 20.19+ 或 22.12+ |
| Rust | stable (edition 2024, 需要 1.85+) |
| Visual Studio Build Tools | 2022, 含 MSVC 与 Windows SDK |
| WebView2 Runtime | Win10/11 通常已自带 |

```bash
npm install
npm run tauri dev      # 开发模式, 前端热更新
npm run tauri build    # 出安装包, 产物在 src-tauri/target/release/
```

产物:

- `src-tauri/target/release/binance-mini.exe`
- `src-tauri/target/release/bundle/nsis/BinanceMini_0.1.0_x64-setup.exe`

## 发布

推 `v*` 标签自动触发 GitHub Actions, 编译 Windows 安装包并创建 draft release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

也可以在 Actions 页面手动 `Run workflow`, 只编译并上传产物, 不发 release.

仓库需要开启写权限: Settings → Actions → General → Workflow permissions 选 **Read and write permissions**, 否则创建 release 会报 `Resource not accessible by integration`.

## 技术栈

Tauri 2 + React 19 + TypeScript + lightweight-charts. 网络、签名、WebSocket 全在 Rust 侧, 前端只订阅状态快照.

## 免责

个人自用工具. 数据来自币安公开 API, 显示延迟、断线、限流都可能导致数字不准, 任何决策请以币安官方客户端为准.
