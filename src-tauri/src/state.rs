use crate::binance::Creds;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 持仓方向. 双向持仓模式下同一合约会出现 Long 与 Short 两条, 单向模式只有 Both.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Side {
    Long,
    Short,
    Both,
}

impl Side {
    pub fn from_position_side(s: &str) -> Side {
        match s {
            "LONG" => Side::Long,
            "SHORT" => Side::Short,
            _ => Side::Both,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candle {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PositionView {
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub entry_price: f64,
    pub mark_price: f64,
    pub upnl: f64,
    pub upnl_pct: f64,
    pub leverage: f64,
    pub liq_price: Option<f64>,
    pub liq_distance_pct: Option<f64>,
    pub margin_ratio: Option<f64>,
    /// 交易所挂单里的止盈/止损触发价, 由 refresh_orders 写入.
    pub tp_price: Option<f64>,
    pub sl_price: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct MarketView {
    pub symbol: String,
    pub mark_price: f64,
    pub funding_rate: f64,
    pub next_funding_at: i64,
    /// 24 小时统计, 来自 REST ticker/24hr.
    pub high_24h: f64,
    pub low_24h: f64,
    pub change_pct_24h: f64,
    pub quote_volume_24h: f64,
    pub last_candle: Option<Candle>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct AccountView {
    pub total_wallet_balance: f64,
    pub total_unrealized_pnl: f64,
    pub total_maint_margin: f64,
    pub available_balance: f64,
    pub total_margin_balance: f64,
    /// 维持保证金 / 保证金余额, 百分比. 越高越接近爆仓.
    pub margin_ratio: f64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct HealthView {
    pub user_stream_ok: bool,
    pub market_stream_ok: bool,
    pub last_msg_ms: i64,
    /// 行情流最近一次收到推送的时间. 与私有流分开记, 否则行情流停了但私有流在推时判断不出掉线.
    pub last_market_msg_ms: i64,
    /// 最近一次 REST 兜底成功的时间. 推送被掐断时靠它判断数据是否还新鲜.
    pub last_rest_ms: i64,
    pub started_at_ms: i64,
    pub last_error: Option<String>,
}

impl HealthView {
    /// 超过 15 秒既没有推送也没有 REST 成功, 才认为数据陈旧, 前端据此标灰.
    /// 只看推送会导致"节点不推数据但 REST 正常"时永久标灰.
    pub fn is_stale(&self) -> bool {
        let last = self.last_msg_ms.max(self.last_rest_ms);
        last > 0 && now_ms() - last > 15_000
    }
}

/// 用户设置的价格线. 只存业务字段, 运行时判定状态放在 AlertRuntime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertLine {
    pub id: String,
    pub symbol: String,
    pub price: f64,
    pub enabled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AlertRuntime {
    /// 上一次价格在线的哪一侧: 1 在上方, -1 在下方, 0 未初始化.
    pub last_side: i8,
    pub last_fired_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub positions: Vec<PositionView>,
    pub markets: Vec<MarketView>,
    pub account: Option<AccountView>,
    pub health: HealthView,
    pub stale: bool,
}

pub struct Inner {
    pub positions: HashMap<(String, Side), PositionView>,
    pub markets: HashMap<String, MarketView>,
    pub account: Option<AccountView>,
    pub health: HealthView,
    pub alerts: Vec<AlertLine>,
    pub alert_rt: HashMap<String, AlertRuntime>,
    /// 前端当前聚焦的合约. K 线只刷它, 避免每轮给每个合约都拉一次.
    pub focus_symbol: Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            markets: HashMap::new(),
            account: None,
            health: HealthView {
                started_at_ms: now_ms(),
                ..Default::default()
            },
            alerts: Vec::new(),
            alert_rt: HashMap::new(),
            focus_symbol: None,
        }
    }
}

pub struct AppState {
    inner: RwLock<Inner>,
    /// 当前应该订阅行情的合约集合; 持仓变化时通过它通知行情任务重连.
    symbols_tx: watch::Sender<Vec<String>>,
    /// 凭证变化时通知各任务重建连接.
    creds_tx: watch::Sender<Option<Creds>>,
}

impl AppState {
    pub fn new() -> Self {
        let (symbols_tx, _) = watch::channel(Vec::new());
        let (creds_tx, _) = watch::channel(None);
        Self {
            inner: RwLock::new(Inner::default()),
            symbols_tx,
            creds_tx,
        }
    }

    pub fn read<R>(&self, f: impl FnOnce(&Inner) -> R) -> R {
        f(&self.inner.read())
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.write())
    }

    pub fn subscribe_symbols(&self) -> watch::Receiver<Vec<String>> {
        self.symbols_tx.subscribe()
    }

    pub fn subscribe_creds(&self) -> watch::Receiver<Option<Creds>> {
        self.creds_tx.subscribe()
    }

    pub fn set_creds(&self, creds: Option<Creds>) {
        self.write(|s| {
            s.positions.clear();
            s.markets.clear();
            s.account = None;
        });
        // 必须用 send_replace: 启动阶段还没有任何订阅者, send 会直接丢弃初值,
        // 导致各网络任务永远看不到凭证而静默空转.
        self.symbols_tx.send_replace(Vec::new());
        self.creds_tx.send_replace(creds);
    }

    /// 根据持仓与告警线重算订阅集合并推给行情任务. 集合没变时不触发重连.
    pub fn recompute_symbols(&self) -> Vec<String> {
        let symbols = self.read(|s| {
            let mut set: Vec<String> = s
                .positions
                .values()
                .filter(|p| p.qty > 0.0)
                .map(|p| p.symbol.clone())
                .collect();
            for a in &s.alerts {
                if a.enabled {
                    set.push(a.symbol.clone());
                }
            }
            set.sort();
            set.dedup();
            set
        });
        self.symbols_tx.send_if_modified(|cur| {
            if *cur == symbols {
                false
            } else {
                *cur = symbols.clone();
                true
            }
        });
        symbols
    }

    pub fn snapshot(&self) -> Snapshot {
        self.read(|s| {
            let mut positions: Vec<PositionView> = s.positions.values().cloned().collect();
            positions.sort_by(|a, b| {
                b.upnl
                    .abs()
                    .partial_cmp(&a.upnl.abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut markets: Vec<MarketView> = s.markets.values().cloned().collect();
            markets.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            Snapshot {
                positions,
                markets,
                account: s.account.clone(),
                health: s.health.clone(),
                stale: s.health.is_stale(),
            }
        })
    }
}
