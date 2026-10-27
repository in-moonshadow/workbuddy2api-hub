//! 关卡 4.8：后台定时调度器（`wb_scheduler.py` 全文 231 行）。
//!
//! ## Python 语义（逐条对齐）
//! - **小时表**：`checkin_hours == travel_hours == [9, 21]`、`keepalive_hours == [22]`、
//!   `cat_hours == [1]`，`all_hours = sorted(set(...)) == [1, 9, 21, 22]`。
//! - **启动**：先睡 **10 秒** 再跑一次「启动初次初始化巡检」，此后进入循环。
//! - **循环**：每轮 `_calc_next_fire()` → 若 `enabled` 且 `tm_min == 0` 且
//!   `tm_hour ∈ all_hours` 则执行一次，然后 `sleep(65)` 避开本分钟重复触发 →
//!   `wait(30)`。即**轮询间隔 30 秒**、**命中后抑制 65 秒**。
//! - **`_calc_next_fire`**：取第一个 `h > cur_h` 的整点；若 `h == cur_h` 且
//!   当前是 `mm == 0 且 ss < 10` 也算；都没有则取**明天**的 `all_hours[0]`。
//! - **非阻塞互斥**：`_execute_cycle` 用 `acquire(blocking=False)`，抢不到就记
//!   「跳过本次巡检」并返回；`trigger_now()` 在锁被占时直接回 `ok:false`。
//! - **单账号异常隔离**：`_run_cycle` 里每一步各有成功/失败日志，任一账号的任一
//!   步失败都不中断整轮。
//! - **`enabled=False` 的账号跳过**（`wb_scheduler.py:147-149`）。
//! - **保活**：`exp = expires_at or 0`；`exp` 为真且 `exp - now < 7200` 才刷新。
//! - **`status()` 七键**：`enabled / mode / mode_cn / mode_intl / last_run_time /
//!   next_run_time / logs`，`logs` 取末 20 条；内部 `logs` 环形保留末 60 条。
//! - **日志 tag 分流**：显式 tag 优先；否则按消息关键词判 `tasks`，其余 `scheduler`。
//!
//! ## 与 Python 的有意差异（4.11 账本登记）
//! - **异步而非线程**：Python 用 `threading.Thread` + 阻塞 `time.sleep`；这里用
//!   `tokio::spawn` + 可注入的 [`Sleeper`]，从而测试不发真实请求、也不真等待。
//! - **无 `wb_proxy` 反向依赖**：Python 的 `log()` 直接 `import wb_proxy` 调
//!   `add_log_entry`；这里改成可注入的 [`PanelSink`]（生产侧由 wb-api 接线，
//!   符合依赖链 `gateway/tasks → api`，**禁止** `wb-tasks → wb-api`）。
//! - **明天用日期运算**：Python 走 `time.time() + 86400` 再取日期，遇夏令时会漂；
//!   这里直接做日历进位，语义更稳（账本登记）。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use wb_gateway::pool::AccountState;

use crate::checkin::{can_checkin, can_report_activity};
use crate::growth_runner::Sleeper;

// ---------------------------------------------------------------------------
// 常量表（`wb_scheduler.py:20-24`）
// ---------------------------------------------------------------------------

/// 每日 09:00 / 21:00 签到。
pub const CHECKIN_HOURS: [i32; 2] = [9, 21];
/// 每日 09:00 派出、21:00 领奖闭环。
pub const TRAVEL_HOURS: [i32; 2] = [9, 21];
/// 每日 22:00 集中 Token 保活。
pub const KEEPALIVE_HOURS: [i32; 1] = [22];
/// 每日 01:00 夜猫子专属任务。
pub const CAT_HOURS: [i32; 1] = [1];
/// `sorted(set(...))`：所有整点排程。
pub const ALL_HOURS: [i32; 4] = [1, 9, 21, 22];

/// 启动先休眠 10 秒（`wb_scheduler.py:73`）。
pub const STARTUP_DELAY_SECS: f64 = 10.0;
/// 轮询间隔 30 秒（`wb_scheduler.py:94`）。
pub const POLL_SECS: f64 = 30.0;
/// 命中后抑制 65 秒（`wb_scheduler.py:93`）。
pub const HIT_SUPPRESS_SECS: f64 = 65.0;
/// 国内版每步后的节流 1.0 秒（`wb_scheduler.py:171,181,188,195,203`）。
pub const CN_STEP_PAUSE_SECS: f64 = 1.0;
/// 国际版活跃对话后的节流 1.5 秒（`wb_scheduler.py:218`）。
pub const INTL_STEP_PAUSE_SECS: f64 = 1.5;
/// Token 剩余寿命低于 2 小时即保活（`wb_scheduler.py:153`）。
pub const KEEPALIVE_THRESHOLD_SECS: f64 = 7200.0;
/// 内部日志环形上限（`wb_scheduler.py:43`）。
pub const LOG_RING_CAPACITY: usize = 60;
/// `status()["logs"]` 只回末 20 条（`wb_scheduler.py:230`）。
pub const STATUS_LOG_TAIL: usize = 20;

/// 网络类失败后的补跑节奏：等 5 分钟再试，最多 3 次（D48）。
///
/// 与 Python `wb_scheduler.Scheduler.RETRY_DELAY_SECS` / `RETRY_MAX_TRIES` 同值。
pub const RETRY_DELAY_SECS: i64 = 300;
pub const RETRY_MAX_TRIES: u32 = 3;

/// 网络类瞬时故障的特征串（与 Python `wb_scheduler._NETWORK_FAILURE_MARKERS` 同源）。
///
/// TLS 握手被切断、DNS 临时失败、超时、连接被重置——这类错误重试有意义：
/// 根因通常是链路被掐断一小段时间（2026-10-21 实测：宿主 Clash Verge 每天
/// 08:2x 自动更新订阅并重载代理 core，容器出站被宿主 TUN 承载，在途 TCP
/// 被打断，中断持续 1-3 分钟），而不是账号或上游业务出了问题。
///
/// 业务类错误（HTTP 400 code 11128、429 code 6004、「今日已做」）**不在**此列：
/// 重试它们只会白白发请求、吃上游限流。
pub const NETWORK_FAILURE_MARKERS: &[&str] = &[
    "urlopen error",
    "ssl:",
    "sslerror",
    "eof occurred",
    "errno -3",
    "errno -2",
    "name or service not known",
    "temporary failure in name resolution",
    "timed out",
    "timeout",
    "请求超时",
    "connection reset",
    "connection aborted",
    "connection refused",
    "remotedisconnected",
    "connectionerror",
    "error sending request",
    "broken pipe",
    "connection closed",
];

/// 错误串是否属于「网络类」瞬时故障（值得延后补跑）。
pub fn is_network_failure(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    if t.is_empty() {
        return false;
    }
    NETWORK_FAILURE_MARKERS.iter().any(|m| t.contains(m))
}

/// `last_run_time` 空值占位（`wb_scheduler.py:228` 的 `or "尚未运行"`）。
pub const LAST_RUN_NEVER: &str = "尚未运行";
/// `next_run_time` 空值占位（`wb_scheduler.py:229` 的 `or "待调度"`）。
pub const NEXT_RUN_PENDING: &str = "待调度";

/// `status()["mode"]` / `status()["mode_cn"]`，逐字对齐 Python。
pub const MODE_TEXT: &str =
    "整点排程 (09:00/21:00 签到旅行 · 每日活跃上报 · 22:00 保活 · 01:00 夜猫)";
/// `status()["mode_intl"]`，逐字对齐 Python。
pub const MODE_INTL_TEXT: &str = "账号 Token 自动保活与凭证常驻 (每日 22:00 集中巡检)";

/// 日志自动分流到 `tasks` 的关键词（`wb_scheduler.py:51`）。
pub const TASKS_LOG_KEYWORDS: [&str; 8] = [
    "签到", "打卡", "活跃", "猫猫", "连登", "夜猫", "抽奖", "兑换",
];

// ---------------------------------------------------------------------------
// 时钟（秒级 + 日历字段）
// ---------------------------------------------------------------------------

/// 解析后的本地时刻（`time.localtime()` 的秒级子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
}

impl LocalNow {
    /// 构造（测试用）。
    pub fn new(year: i32, month: u32, day: u32, hour: i32, min: i32, sec: i32) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            min,
            sec,
        }
    }

    /// `time.strftime("%Y-%m-%d %H:%M:%S")`。
    pub fn timestamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        )
    }

    /// `time.strftime("%Y-%m-%d")`。
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// 日期 +1 天（零点）；只用于取「明天的年月日」。
    pub fn next_day(&self) -> LocalNow {
        let leap = (self.year % 4 == 0 && self.year % 100 != 0) || self.year % 400 == 0;
        let days_in_month = match self.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => 31,
        };
        let (mut y, mut m, mut d) = (self.year, self.month, self.day + 1);
        if d > days_in_month {
            d = 1;
            m += 1;
            if m > 12 {
                m = 1;
                y += 1;
            }
        }
        LocalNow::new(y, m, d, 0, 0, 0)
    }
}

/// 秒级本地时钟。默认实现走真实时间；测试注入固定/推进的时刻。
///
/// 与 [`crate::growth_runner::Clock`]（只有 `local_hour`）互补：排程需要分/秒
/// （`mm == 0`、`ss < 10`），`status()` 需要完整时间戳。
pub trait ScheduleClock: Send + Sync {
    /// 当前本地时刻。
    fn local_now(&self) -> LocalNow;

    /// 当前 epoch 秒（`time.time()`），保活判定用。
    fn epoch_secs(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}

/// 生产时钟：`chrono::Local::now()`。
pub struct LocalScheduleClock;

impl ScheduleClock for LocalScheduleClock {
    fn local_now(&self) -> LocalNow {
        use chrono::{Datelike, Timelike};
        let t = chrono::Local::now();
        LocalNow::new(
            t.year(),
            t.month(),
            t.day(),
            t.hour() as i32,
            t.minute() as i32,
            t.second() as i32,
        )
    }
}

// ---------------------------------------------------------------------------
// 纯函数
// ---------------------------------------------------------------------------

/// `_calc_next_fire()`（`wb_scheduler.py:96-113`）：下一次触发的
/// `%Y-%m-%d %H:%M:%S`。
///
/// 取 `all_hours` 里第一个 `h > cur_h` 或（`h == cur_h` 且 `mm == 0` 且
/// `ss < 10`）的整点；都不满足则取**明天**的 `all_hours[0]`（= 01:00）。
pub fn calc_next_fire(now: &LocalNow) -> String {
    let same_hour_ok = now.min == 0 && now.sec < 10;
    let found = ALL_HOURS
        .iter()
        .copied()
        .find(|&h| h > now.hour || (h == now.hour && same_hour_ok));
    let base = match found {
        Some(_) => *now,
        None => now.next_day(),
    };
    let hour = found.unwrap_or(ALL_HOURS[0]);
    LocalNow::new(base.year, base.month, base.day, hour, 0, 0).timestamp()
}

/// 循环里的一次「整点命中」判定（`wb_scheduler.py:87`）。
pub fn is_hour_hit(enabled: bool, now: &LocalNow) -> bool {
    enabled && now.min == 0 && ALL_HOURS.contains(&now.hour)
}

/// 保活判定（`wb_scheduler.py:152-153`）：`exp` 为 `None` 或 0 视为「无过期
/// 信息」，**不刷新**；否则 `exp - now < 7200` 时刷新。
pub fn needs_keepalive(expires_at: Option<f64>, now: f64) -> bool {
    match expires_at {
        Some(exp) if exp != 0.0 => (exp - now) < KEEPALIVE_THRESHOLD_SECS,
        _ => false,
    }
}

/// 日志 tag 分流（`wb_scheduler.py:47-54`）：关键词命中 ⇒ `tasks`，否则 `scheduler`。
///
/// Python 还会把显式 tag 置顶；本移植的调用点从不传显式 tag，故只实现关键词
/// 分支（`log(msg, tag=None)` 的 `tag=None` 情形）。
pub fn derive_log_tag(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if TASKS_LOG_KEYWORDS.iter().any(|k| lower.contains(k)) {
        "tasks"
    } else {
        "scheduler"
    }
}

/// `acc.uid[:8] if acc.uid else "?"` —— 按**字符**截断（对齐 Python 切片）。
pub fn uid8(uid: &str) -> String {
    if uid.is_empty() {
        "?".to_string()
    } else {
        uid.chars().take(8).collect()
    }
}

// ---------------------------------------------------------------------------
// 可注入的执行钩子
// ---------------------------------------------------------------------------

/// 单步结果：`{ok, ...}` 形状由各任务实现给出（对齐 Python 的 dict 返回）。
pub type StepResult = Value;

/// 一个可被巡检调度的账号：`AccountState` 的排程相关字段快照。
///
/// 不加 `Eq`：`expires_at` 是 `f64`。
#[derive(Debug, Clone, PartialEq)]
pub struct AccountView {
    pub uid: String,
    pub realm: String,
    pub enabled: bool,
    pub expires_at: Option<f64>,
    pub is_enterprise: bool,
    /// 上次签到时间（`lastCheckin`，`%Y-%m-%d %H:%M:%S`）。
    pub last_checkin: Option<String>,
    /// 上次活跃上报时间（`lastActivityReport`）。
    pub last_activity_report: Option<String>,
    /// 上次国际活跃对话时间（`lastDailyChat`）。
    pub last_daily_chat: Option<String>,
    /// 今天是否已完成一次**计入官方每日活跃**的对话（`daily_chat_granted_today()`）。
    ///
    /// ⚠️ 上游奖励包跨日发放：对话发生在 D 天，奖励包的 `create_time` 落在 D+1
    /// 天（2026-10-20 逐包核对 20 个 30/50 包：19 个的前一日有成功对话，且
    /// 20/20 的发放时刻都早于当日首次成功对话）。所以**不能**拿「积分快照里有
    /// 今天的奖励包」判「今天已完成」—— 那是**昨天**的结算，拿它拦截会让账号
    /// 隔天才拿一包（今天被昨天的包挡住 → 今天没有对话 → 明天没有包）。
    /// 本字段应由调用方按本地证据（`last_daily_chat` / 活动历史）填；若将来要
    /// 从 credits 快照填，必须走 D+1 口径（查**明天**日期的包）。
    pub daily_chat_granted_today: bool,
    /// 账号的活动触发来源（scheduler/manual/account_add/account_import）。
    pub trigger: String,
}

impl AccountView {
    /// 从网关账号状态构造；三个 `last*` 与 `daily_chat_granted_today` 由调用方
    /// 通过 [`Self::with_last_records`] 补齐。
    pub fn from_state(acc: &AccountState) -> Self {
        Self {
            uid: acc.uid.clone(),
            realm: acc.realm.clone(),
            enabled: acc.enabled,
            expires_at: acc.expires_at,
            is_enterprise: !acc.enterprise_id.is_empty(),
            last_checkin: None,
            last_activity_report: None,
            last_daily_chat: None,
            daily_chat_granted_today: false,
            trigger: acc.trigger.clone(),
        }
    }

    /// 补齐本地记录（持久化层读出的幂等字段）。
    pub fn with_last_records(
        mut self,
        last_checkin: Option<String>,
        last_activity_report: Option<String>,
        last_daily_chat: Option<String>,
        daily_chat_granted_today: bool,
    ) -> Self {
        self.last_checkin = last_checkin;
        self.last_activity_report = last_activity_report;
        self.last_daily_chat = last_daily_chat;
        self.daily_chat_granted_today = daily_chat_granted_today;
        self
    }

    /// `acc.can_checkin()`。
    pub fn can_checkin(&self, today: &str) -> bool {
        can_checkin(
            &self.realm,
            self.is_enterprise,
            self.last_checkin.as_deref(),
            today,
        )
    }

    /// `acc.can_report_activity()`。
    pub fn can_report_activity(&self, today: &str) -> bool {
        can_report_activity(
            &self.realm,
            self.is_enterprise,
            self.last_activity_report.as_deref(),
            today,
        )
    }

    /// `acc.can_daily_chat()`（`wb_accounts.py:1428`）：
    /// 非 intl ⇒ 否；今天已完成（`daily_chat_granted_today`）⇒ 否；否则看
    /// `lastDailyChat` 是否今天。
    pub fn can_daily_chat(&self, today: &str) -> bool {
        if self.realm != "intl" {
            return false;
        }
        if self.daily_chat_granted_today {
            return false;
        }
        match &self.last_daily_chat {
            None => true,
            Some(v) => !v.starts_with(today),
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 单个账号在一轮巡检里要跑的各步。**全部可注入**：生产实现打真实上游
/// （后续接线关卡），测试注入计数 fake，从而**不发任何真实请求**
/// （M4 计划硬约束）。
pub trait CycleExecutor: Send + Sync {
    /// `acc.refresh()`：Token 保活刷新。
    fn refresh<'a>(&'a self, acc: &'a AccountView, trigger: &str) -> BoxFut<'a, bool>;

    /// `acc.checkin(trigger="scheduler")`：`{ok, code?, msg?, data?, error?}`。
    fn checkin<'a>(&'a self, acc: &'a AccountView) -> BoxFut<'a, StepResult>;

    /// `acc.report_activity()`：`{ok, msg?/error?}`。
    fn report_activity<'a>(&'a self, acc: &'a AccountView) -> BoxFut<'a, StepResult>;

    /// `wb_tasks.do_cat_travel(acc)`：`{action, msg, ...}`。
    fn cat_travel<'a>(&'a self, acc: &'a AccountView) -> BoxFut<'a, StepResult>;

    /// `wb_tasks.run_streak_bonus(acc)`：`{logs: [...], ...}`。
    fn streak_bonus<'a>(&'a self, acc: &'a AccountView) -> BoxFut<'a, StepResult>;

    /// `wb_tasks.run_night_growth(acc)`：`{logs: [...]}`。
    fn night_growth<'a>(&'a self, acc: &'a AccountView) -> BoxFut<'a, StepResult>;

    /// `acc.daily_chat(trigger="scheduler")`：`{ok, msg?/error?}`。
    fn daily_chat<'a>(&'a self, acc: &'a AccountView) -> BoxFut<'a, StepResult>;

    /// 账号池快照（`pool.accounts` 的只读副本）。
    fn accounts(&self) -> Vec<AccountView>;
}

/// 全部步骤返回 `ok:false` 的空执行器：未接线时的安全默认。
pub struct NullExecutor;

impl CycleExecutor for NullExecutor {
    fn refresh<'a>(&'a self, _acc: &'a AccountView, _trigger: &str) -> BoxFut<'a, bool> {
        Box::pin(async { false })
    }

    fn checkin<'a>(&'a self, _acc: &'a AccountView) -> BoxFut<'a, StepResult> {
        Box::pin(async { json!({"ok": false, "error": "checkin executor not configured"}) })
    }

    fn report_activity<'a>(&'a self, _acc: &'a AccountView) -> BoxFut<'a, StepResult> {
        Box::pin(async { json!({"ok": false, "error": "report executor not configured"}) })
    }

    fn cat_travel<'a>(&'a self, _acc: &'a AccountView) -> BoxFut<'a, StepResult> {
        Box::pin(async { json!({"action": "skip", "msg": "travel executor not configured"}) })
    }

    fn streak_bonus<'a>(&'a self, _acc: &'a AccountView) -> BoxFut<'a, StepResult> {
        Box::pin(async { json!({"logs": []}) })
    }

    fn night_growth<'a>(&'a self, _acc: &'a AccountView) -> BoxFut<'a, StepResult> {
        Box::pin(async { json!({"logs": []}) })
    }

    fn daily_chat<'a>(&'a self, _acc: &'a AccountView) -> BoxFut<'a, StepResult> {
        Box::pin(async { json!({"ok": false, "error": "daily_chat executor not configured"}) })
    }

    fn accounts(&self) -> Vec<AccountView> {
        Vec::new()
    }
}

/// 面板日志出口（Python 的 `wb_proxy.add_log_entry`）。
///
/// `wb-tasks` 不得依赖 `wb-api`，故以 trait 注入；生产侧由 wb-api 实现。
pub trait PanelSink: Send + Sync {
    fn add_log_entry(&self, msg: &str, tag: &str);
}

// ---------------------------------------------------------------------------
// 一轮巡检的计数
// ---------------------------------------------------------------------------

/// `_run_cycle` 的五个计数器（`wb_scheduler.py:138-142`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CycleCounts {
    pub refreshed: usize,
    pub checkin: usize,
    pub report: usize,
    pub travel: usize,
    pub daily_chat: usize,
}

impl CycleCounts {
    /// 收尾汇总行（`wb_scheduler.py:220`）。
    pub fn summary_line(&self) -> String {
        format!(
            "巡检完成：Token保活 {} 个，国内签到 {} 个，活跃上报 {} 个，猫猫日常 {} 个，国际活跃 {} 个",
            self.refreshed, self.checkin, self.report, self.travel, self.daily_chat
        )
    }
}

// ---------------------------------------------------------------------------
// 调度器
// ---------------------------------------------------------------------------

/// 需要延后补跑的任务种类（D48）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryTask {
    /// 国内版每日签到。
    Checkin,
    /// 国际版每日活跃打卡。
    DailyChat,
}

impl RetryTask {
    /// 日志里的任务名（与 Python 侧 `task` 字段同词）。
    pub fn label(self) -> &'static str {
        match self {
            RetryTask::Checkin => "checkin",
            RetryTask::DailyChat => "daily_chat",
        }
    }
}

/// 一条待补跑的任务（纯内存，不落盘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRetry {
    /// 账号 uid。
    pub uid: String,
    /// 任务种类。
    pub task: RetryTask,
    /// 到期时刻（epoch 秒，取自 `ScheduleClock::epoch_secs`）。
    pub due: i64,
    /// 已经排过几轮（含首次登记）。
    pub tries: u32,
}

struct State {
    enabled: bool,
    last_run_time: Option<String>,
    next_run_time: Option<String>,
    logs: Vec<String>,
    /// 网络类失败的延后补跑队列。
    pending_retries: Vec<PendingRetry>,
}

/// 后台定时调度器。
///
/// **非阻塞互斥**：`run_lock: AtomicBool` 对应 Python 的
/// `threading.Lock` + `acquire(blocking=False)`。
pub struct Scheduler<'a> {
    executor: &'a dyn CycleExecutor,
    sleeper: &'a dyn Sleeper,
    clock: &'a dyn ScheduleClock,
    panel: Option<&'a dyn PanelSink>,
    state: Mutex<State>,
    run_lock: AtomicBool,
    stop_event: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Scheduler<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl<'a> Scheduler<'a> {
    /// 构造：先算一次 `_calc_next_fire()`（`wb_scheduler.py:34`）。
    pub fn new(
        executor: &'a dyn CycleExecutor,
        sleeper: &'a dyn Sleeper,
        clock: &'a dyn ScheduleClock,
    ) -> Self {
        let next = calc_next_fire(&clock.local_now());
        Self {
            executor,
            sleeper,
            clock,
            panel: None,
            state: Mutex::new(State {
                enabled: true,
                last_run_time: None,
                next_run_time: Some(next),
                logs: Vec::new(),
                pending_retries: Vec::new(),
            }),
            run_lock: AtomicBool::new(false),
            stop_event: AtomicBool::new(false),
            task: Mutex::new(None),
        }
    }

    /// 注入面板日志出口。
    pub fn with_panel(mut self, panel: &'a dyn PanelSink) -> Self {
        self.panel = Some(panel);
        self
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_enabled(&self) -> bool {
        self.lock_state().enabled
    }

    /// `log()`（`wb_scheduler.py:39-57`）：本地环形 60 条 + 面板转发（tag 分流）。
    ///
    /// 返回本条消息的 tag（便于测试断言分流结果）。
    pub fn log(&self, msg: &str) -> &'static str {
        let ts = self.clock.local_now().timestamp();
        let entry = format!("[{ts}] {msg}");
        {
            let mut st = self.lock_state();
            st.logs.push(entry);
            let len = st.logs.len();
            if len > LOG_RING_CAPACITY {
                st.logs.drain(0..len - LOG_RING_CAPACITY);
            }
        }
        let tag = derive_log_tag(msg);
        if let Some(panel) = self.panel {
            panel.add_log_entry(&format!("[调度器] {msg}"), tag);
        }
        tag
    }

    /// 当前环形日志（最多 60 条，旧→新）。
    pub fn logs(&self) -> Vec<String> {
        self.lock_state().logs.clone()
    }

    pub fn last_run_time(&self) -> Option<String> {
        self.lock_state().last_run_time.clone()
    }

    pub fn next_run_time(&self) -> Option<String> {
        self.lock_state().next_run_time.clone()
    }

    /// `_calc_next_fire()`：按当前时刻重算并写回 `next_run_time`。
    pub fn recalc_next_fire(&self) -> String {
        let next = calc_next_fire(&self.clock.local_now());
        self.lock_state().next_run_time = Some(next.clone());
        next
    }

    /// 起后台循环（`start()`，`wb_scheduler.py:59-65`）；已启动则不重复起。
    ///
    /// 需要 `Arc<Self>` 且当前处于 tokio runtime 内。
    pub fn start(self: &Arc<Self>)
    where
        Self: 'static,
    {
        {
            let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = guard.as_ref() {
                if !h.is_finished() {
                    return;
                }
            }
            let me = Arc::clone(self);
            *guard = Some(tokio::spawn(async move { me.run_loop().await }));
        }
        self.log("后台定时调度器已启动");
    }

    /// 停止（`stop()`，`wb_scheduler.py:67-69`）：置停止位、记日志、abort 任务。
    pub fn stop(&self) {
        self.stop_event.store(true, Ordering::SeqCst);
        self.log("后台定时调度器已暂停");
        let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = guard.as_ref() {
            h.abort();
        }
        *guard = None;
    }

    /// `stop()` 并等待后台任务真正退出（测试收尾用）。
    pub async fn shutdown(self: &Arc<Self>)
    where
        Self: 'static,
    {
        self.stop();
        let handle = {
            let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(h) = handle {
            // abort 之后 join 只会得到 `Err(Cancelled)`，这里仅等它退出
            let _ = h.await;
        }
    }

    /// 后台循环（`_run_loop`，`wb_scheduler.py:71-94`）。
    ///
    /// 1. 睡 10 秒；
    /// 2. 跑一次「启动初次初始化巡检」，异常只记日志；
    /// 3. 循环：先 `_calc_next_fire`，命中整点则执行并睡 65 秒避开重复，
    ///    再等 30 秒；停止位到即退出。
    pub async fn run_loop(&self) {
        self.sleeper.sleep(STARTUP_DELAY_SECS).await;
        self.execute_cycle("启动初次初始化巡检").await;

        while !self.stop_event.load(Ordering::SeqCst) {
            self.recalc_next_fire();
            let now = self.clock.local_now();
            if is_hour_hit(self.is_enabled(), &now) {
                let reason = format!("整点排程命中 ({}:00)", now.hour);
                self.execute_cycle(&reason).await;
                self.sleeper.sleep(HIT_SUPPRESS_SECS).await;
                if self.stop_event.load(Ordering::SeqCst) {
                    return;
                }
            }
            // 网络类失败的延后补跑：主循环每 30 秒醒一次，顺手看看有没有到期的
            if self.is_enabled() {
                self.run_due_retries().await;
            }
            self.sleeper.sleep(POLL_SECS).await;
        }
    }

    // ------------------------------------------------------------------
    // 网络类失败的延后补跑（D48）
    //
    // 整点排程里离失败时刻最近的一次可能隔 12 小时（21:00 才再跑），
    // 所以链路类故障失败后只记一条日志，奖励就白丢一天。这里给「网络类」
    // 失败单独安排一次延后补跑：等 RETRY_DELAY_SECS 秒（跨过 1-3 分钟的
    // 链路中断）再打一次，最多 RETRY_MAX_TRIES 次。
    // ------------------------------------------------------------------

    /// 把一次网络类失败登记进延后补跑队列。
    fn queue_retry(&self, uid: &str, task: RetryTask) {
        let now = self.clock.epoch_secs() as i64;
        let due = now + RETRY_DELAY_SECS;
        let mut st = self.lock_state();
        // 同一账号同一任务只留一条：重复失败只推迟到期时间
        for item in st.pending_retries.iter_mut() {
            if item.uid == uid && item.task == task {
                item.due = due;
                item.tries += 1;
                return;
            }
        }
        st.pending_retries.push(PendingRetry {
            uid: uid.to_string(),
            task,
            due,
            tries: 1,
        });
        drop(st);
        self.log(&format!(
            "账号 [{}] {} 遇网络故障，{} 分钟后自动补跑",
            uid8(uid),
            task.label(),
            RETRY_DELAY_SECS / 60
        ));
    }

    /// 当前待补跑队列（只读快照）。
    pub fn pending_retries(&self) -> Vec<PendingRetry> {
        self.lock_state().pending_retries.clone()
    }

    /// 执行到期的补跑项（由主循环每 30 秒调一次）。
    pub async fn run_due_retries(&self) {
        let now = self.clock.epoch_secs() as i64;
        let due: Vec<PendingRetry> = {
            let mut st = self.lock_state();
            let mut due = Vec::new();
            let mut keep = Vec::new();
            for it in st.pending_retries.drain(..) {
                if it.due <= now {
                    due.push(it);
                } else {
                    keep.push(it);
                }
            }
            st.pending_retries = keep;
            due
        };
        if due.is_empty() {
            return;
        }
        if self
            .run_lock
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // 正在巡检就别撞车：放回队列，下一轮再来
            let mut st = self.lock_state();
            for mut it in due {
                it.due = now + 60;
                st.pending_retries.push(it);
            }
            return;
        }
        let _guard = RunLockGuard(&self.run_lock);
        for it in due {
            self.run_one_retry(it).await;
        }
    }

    /// 补跑一个账号的一项任务。
    async fn run_one_retry(&self, item: PendingRetry) {
        let id = uid8(&item.uid);
        let accounts = self.executor.accounts();
        let Some(acc) = accounts.iter().find(|a| a.uid == item.uid) else {
            self.log(&format!("! 账号 [{id}] 已不在账号池，取消补跑"));
            return;
        };
        if !acc.enabled {
            self.log(&format!("! 账号 [{id}] 已停用，取消补跑"));
            return;
        }
        let today = self.clock.local_now().date();
        let res = match item.task {
            RetryTask::Checkin => {
                // 门禁复查：期间可能已被用户手动做过，别再打一次
                if !acc.can_checkin(&today) {
                    self.log(&format!("账号 [{id}] 签到已完成，取消补跑"));
                    return;
                }
                self.executor.checkin(acc).await
            }
            RetryTask::DailyChat => {
                if !acc.can_daily_chat(&today) {
                    self.log(&format!("账号 [{id}] 每日活跃已完成，取消补跑"));
                    return;
                }
                self.executor.daily_chat(acc).await
            }
        };
        if truthy_ok(&res) {
            self.log(&format!(
                "✓ 账号 [{id}] {} 补跑成功: {}",
                item.task.label(),
                str_field(&res, "msg")
            ));
            return;
        }
        let detail = err_or_msg(&res);
        self.log(&format!(
            "! 账号 [{id}] {} 补跑仍未成功: {}",
            item.task.label(),
            detail
        ));
        // 还是网络故障 ⇒ 按上限再排一次；业务类失败不再排（重试只会吃限流）
        if item.tries < RETRY_MAX_TRIES && is_network_failure(&detail) {
            let due = self.clock.epoch_secs() as i64 + RETRY_DELAY_SECS;
            self.lock_state().pending_retries.push(PendingRetry {
                uid: item.uid.clone(),
                task: item.task,
                due,
                tries: item.tries + 1,
            });
            self.log(&format!(
                "账号 [{id}] {} 仍是网络故障，{} 分钟后再试（第 {}/{} 次）",
                item.task.label(),
                RETRY_DELAY_SECS / 60,
                item.tries + 1,
                RETRY_MAX_TRIES
            ));
        }
    }

    /// `_execute_cycle()`（`wb_scheduler.py:122-129`）：**非阻塞**抢锁。
    ///
    /// 抢不到就记「跳过本次巡检」并返回全零计数；抢到则返回后释放（含 panic
    /// 展开）——由 `RunLockGuard` 的 `Drop` 保证。
    pub async fn execute_cycle(&self, trigger_reason: &str) -> CycleCounts {
        if self
            .run_lock
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.log(&format!("跳过本次巡检 ({trigger_reason})：上一轮仍在执行"));
            return CycleCounts::default();
        }
        let _guard = RunLockGuard(&self.run_lock);
        self.run_cycle(trigger_reason).await
    }

    /// `_run_cycle()`（`wb_scheduler.py:131-220`）。
    pub async fn run_cycle(&self, trigger_reason: &str) -> CycleCounts {
        let mut counts = CycleCounts::default();
        let now = self.clock.local_now();
        self.lock_state().last_run_time = Some(now.timestamp());
        self.log(&format!("开始执行任务 ({trigger_reason})..."));

        let accounts = self.executor.accounts();
        if accounts.is_empty() {
            self.log("暂无可用的活跃账号，跳过本次巡检");
            return counts;
        }
        let today = now.date();

        for acc in &accounts {
            // 面板里禁用（enabled=False）的账号不参与自动巡检。
            if !acc.enabled {
                continue;
            }
            let id = uid8(&acc.uid);

            // 1. 保活
            if needs_keepalive(acc.expires_at, self.clock.epoch_secs()) {
                self.log(&format!("账号 [{id}] Token 即将到期，执行主动保活刷新..."));
                if self.executor.refresh(acc, "scheduler").await {
                    counts.refreshed += 1;
                    self.log(&format!("✓ 账号 [{id}] Token 自动保活刷新成功"));
                } else {
                    self.log(&format!("! 账号 [{id}] Token 保活刷新失败"));
                }
            }

            // 2. 国内版
            if acc.realm == "cn" {
                if acc.can_checkin(&today) {
                    self.log(&format!(
                        "检测到国内版账号 [{id}] 今日尚未签到，执行自动签到..."
                    ));
                    let res = self.executor.checkin(acc).await;
                    if truthy_ok(&res) {
                        counts.checkin += 1;
                        self.log(&format!(
                            "✓ 账号 [{id}] 自动签到成功: {}",
                            str_field(&res, "msg")
                        ));
                    } else {
                        let detail = err_or_msg(&res);
                        self.log(&format!("! 账号 [{id}] 自动签到未成功: {detail}"));
                        // 链路类故障：等几分钟补跑一次，别让这一天的签到白丢
                        if is_network_failure(&detail) {
                            self.queue_retry(&acc.uid, RetryTask::Checkin);
                        }
                    }
                    self.sleeper.sleep(CN_STEP_PAUSE_SECS).await;
                }

                if acc.can_report_activity(&today) {
                    self.log(&format!(
                        "检测到国内版账号 [{id}] 今日尚未活跃上报，执行对话活跃上报..."
                    ));
                    let res = self.executor.report_activity(acc).await;
                    if truthy_ok(&res) {
                        counts.report += 1;
                        self.log(&format!("✓ 账号 [{id}] {}", str_field(&res, "msg")));
                    } else {
                        self.log(&format!(
                            "! 账号 [{id}] 对话活跃上报失败: {}",
                            err_or_msg(&res)
                        ));
                    }
                    self.sleeper.sleep(CN_STEP_PAUSE_SECS).await;
                }

                // 猫猫旅行
                let tr = self.executor.cat_travel(acc).await;
                let action = tr.get("action").and_then(Value::as_str).unwrap_or("");
                if action == "claim" || action == "depart" {
                    counts.travel += 1;
                    self.log(&format!(
                        "🐱 账号 [{id}] 猫猫日常处理: {}",
                        str_field(&tr, "msg")
                    ));
                }
                self.sleeper.sleep(CN_STEP_PAUSE_SECS).await;

                // 连登管家闭环
                let bonus = self.executor.streak_bonus(acc).await;
                for line in log_lines(&bonus) {
                    self.log(&format!("★ 账号 [{id}] {line}"));
                    self.sleeper.sleep(CN_STEP_PAUSE_SECS).await;
                }

                // 01:00 夜猫子专属任务（用当前小时判，不预设）
                if CAT_HOURS.contains(&self.clock.local_now().hour) {
                    let night = self.executor.night_growth(acc).await;
                    for line in log_lines(&night) {
                        self.log(&format!("🌙 {line}"));
                    }
                    self.sleeper.sleep(CN_STEP_PAUSE_SECS).await;
                }
            }

            // 3. 国际版每日活跃对话
            if acc.realm == "intl" && acc.can_daily_chat(&today) {
                self.log(&format!(
                    "检测到国际版账号 [{id}] 今日尚未活跃，执行每日活跃打卡对话..."
                ));
                let res = self.executor.daily_chat(acc).await;
                if truthy_ok(&res) {
                    counts.daily_chat += 1;
                    self.log(&format!(
                        "✓ 账号 [{id}] 每日活跃对话成功: {}",
                        str_field(&res, "msg")
                    ));
                } else {
                    let detail = err_or_msg(&res);
                    self.log(&format!("! 账号 [{id}] 每日活跃对话失败: {detail}"));
                    // 同上：链路类故障稍后补跑
                    if is_network_failure(&detail) {
                        self.queue_retry(&acc.uid, RetryTask::DailyChat);
                    }
                }
                self.sleeper.sleep(INTL_STEP_PAUSE_SECS).await;
            }
        }

        self.log(&counts.summary_line());
        counts
    }

    /// `trigger_now()`（`wb_scheduler.py:115-120`）：异步起一轮；锁被占则拒。
    ///
    /// 返回体逐字对齐：`{ok, msg}`。
    pub fn trigger_now(self: &Arc<Self>) -> Value
    where
        Self: 'static,
    {
        if self.run_lock.load(Ordering::SeqCst) {
            return json!({"ok": false, "msg": "已有巡检正在执行，请稍候再试"});
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.execute_cycle("手动立即触发").await;
        });
        json!({"ok": true, "msg": "已触发后台调度执行"})
    }

    /// `trigger_now` 的同步版本（测试/非 runtime 上下文）：回执形状一致，
    /// 并把本轮计数一并交回。生产接口仍用 [`Self::trigger_now`]。
    pub async fn trigger_now_blocking(&self) -> (Value, CycleCounts) {
        if self.run_lock.load(Ordering::SeqCst) {
            return (
                json!({"ok": false, "msg": "已有巡检正在执行，请稍候再试"}),
                CycleCounts::default(),
            );
        }
        let counts = self.execute_cycle("手动立即触发").await;
        (json!({"ok": true, "msg": "已触发后台调度执行"}), counts)
    }

    /// `status()`（`wb_scheduler.py:222-231`）：七个键，顺序与 Python 一致。
    pub fn status(&self) -> Value {
        let st = self.lock_state();
        let tail: Vec<String> = st
            .logs
            .iter()
            .rev()
            .take(STATUS_LOG_TAIL)
            .rev()
            .cloned()
            .collect();
        json!({
            "enabled": st.enabled,
            "mode": MODE_TEXT,
            "mode_cn": MODE_TEXT,
            "mode_intl": MODE_INTL_TEXT,
            "last_run_time": st
                .last_run_time
                .clone()
                .unwrap_or_else(|| LAST_RUN_NEVER.to_string()),
            "next_run_time": st
                .next_run_time
                .clone()
                .unwrap_or_else(|| NEXT_RUN_PENDING.to_string()),
            "logs": tail,
        })
    }

    /// 面板开关切换（`wb_proxy.py:17457-17460`）：翻转后记日志并回 `status()`。
    pub fn toggle_enabled(&self) -> Value {
        let enabled = {
            let mut st = self.lock_state();
            st.enabled = !st.enabled;
            st.enabled
        };
        let label = if enabled { "启用" } else { "暂停" };
        self.log(&format!("用户切换调度器状态为: {label}"));
        self.status()
    }
}

/// 抢到锁后释放（panic 也释放，等价 Python 的 `try/finally`）。
struct RunLockGuard<'a>(&'a AtomicBool);

impl Drop for RunLockGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 结果取值（逐字对齐 Python 的 `res.get(...)`）
// ---------------------------------------------------------------------------

/// Python 真值语义：`res.get("ok")`。
fn truthy_ok(res: &Value) -> bool {
    match res.get("ok") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Null) | None => false,
    }
}

/// `res.get("msg")`；缺失时 Python 打印 `None`，这里同样给 `"None"`。
fn str_field(res: &Value, key: &str) -> String {
    match res.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "None".to_string(),
    }
}

/// `res.get("error") or res.get("msg")`。
fn err_or_msg(res: &Value) -> String {
    match res.get("error") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Null) | None => str_field(res, "msg"),
        Some(v) => v.to_string(),
    }
}

/// `res.get("logs", [])` 里的字符串条目。
fn log_lines(res: &Value) -> Vec<String> {
    res.get("logs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: i32, mi: i32, s: i32) -> LocalNow {
        LocalNow::new(y, m, d, h, mi, s)
    }

    #[test]
    fn all_hours_is_sorted_union() {
        let mut all: Vec<i32> = CHECKIN_HOURS
            .iter()
            .chain(TRAVEL_HOURS.iter())
            .chain(KEEPALIVE_HOURS.iter())
            .chain(CAT_HOURS.iter())
            .copied()
            .collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all, ALL_HOURS.to_vec());
    }

    #[test]
    fn calc_next_fire_picks_strictly_greater_hour() {
        assert_eq!(
            calc_next_fire(&at(2026, 10, 17, 8, 30, 0)),
            "2026-10-17 09:00:00"
        );
        assert_eq!(
            calc_next_fire(&at(2026, 10, 17, 9, 30, 0)),
            "2026-10-17 21:00:00"
        );
        assert_eq!(
            calc_next_fire(&at(2026, 10, 17, 22, 1, 0)),
            "2026-10-18 01:00:00"
        );
    }

    #[test]
    fn calc_next_fire_same_hour_window() {
        assert_eq!(
            calc_next_fire(&at(2026, 10, 17, 9, 0, 9)),
            "2026-10-17 09:00:00"
        );
        assert_eq!(
            calc_next_fire(&at(2026, 10, 17, 9, 0, 10)),
            "2026-10-17 21:00:00"
        );
        assert_eq!(
            calc_next_fire(&at(2026, 10, 17, 9, 1, 0)),
            "2026-10-17 21:00:00"
        );
    }

    #[test]
    fn calc_next_fire_rolls_over_month_and_year() {
        assert_eq!(
            calc_next_fire(&at(2026, 12, 31, 23, 0, 0)),
            "2027-01-01 01:00:00"
        );
        assert_eq!(
            calc_next_fire(&at(2026, 2, 28, 23, 0, 0)),
            "2026-03-01 01:00:00"
        );
        assert_eq!(
            calc_next_fire(&at(2028, 2, 28, 23, 0, 0)),
            "2028-02-29 01:00:00"
        );
    }

    #[test]
    fn hour_hit_requires_enabled_and_minute_zero() {
        assert!(is_hour_hit(true, &at(2026, 10, 17, 21, 0, 40)));
        assert!(!is_hour_hit(false, &at(2026, 10, 17, 21, 0, 40)));
        assert!(!is_hour_hit(true, &at(2026, 10, 17, 21, 1, 0)));
        assert!(
            !is_hour_hit(true, &at(2026, 10, 17, 3, 0, 0)),
            "3 点不在表内"
        );
    }

    #[test]
    fn keepalive_threshold() {
        assert!(needs_keepalive(Some(1000.0), 0.0));
        assert!(!needs_keepalive(Some(10_000.0), 1000.0));
        assert!(!needs_keepalive(Some(0.0), 0.0), "exp=0 视为无过期信息");
        assert!(!needs_keepalive(None, 0.0));
        assert!(needs_keepalive(Some(7199.0), 0.0));
        assert!(!needs_keepalive(Some(7200.0), 0.0), "严格小于才刷新");
    }

    #[test]
    fn log_tag_routing() {
        assert_eq!(
            derive_log_tag("检测到国内版账号 [abc] 今日尚未签到"),
            "tasks"
        );
        assert_eq!(derive_log_tag("后台定时调度器已启动"), "scheduler");
        assert_eq!(derive_log_tag("🐱 猫猫日常处理: departed"), "tasks");
        assert_eq!(derive_log_tag("巡检完成：Token保活 0 个"), "scheduler");
    }

    #[test]
    fn uid8_truncates_by_chars() {
        assert_eq!(uid8("abc"), "abc");
        assert_eq!(uid8("0123456789abcdef"), "01234567");
        assert_eq!(uid8(""), "?");
        assert_eq!(uid8("张三李四王五赵六钱七"), "张三李四王五赵六");
    }

    #[test]
    fn status_has_seven_keys_in_python_order() {
        let exec = NullExecutor;
        let sleeper = crate::growth_runner::NoopSleeper::default();
        let clock = LocalScheduleClock;
        let sched = Scheduler::new(&exec, &sleeper, &clock);
        let st = sched.status();
        let obj = st.as_object().expect("status 必须是对象");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "enabled",
                "mode",
                "mode_cn",
                "mode_intl",
                "last_run_time",
                "next_run_time",
                "logs"
            ]
        );
        assert_eq!(st["last_run_time"], LAST_RUN_NEVER);
        assert!(st["logs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn log_ring_caps_at_60_and_status_tail_at_20() {
        let exec = NullExecutor;
        let sleeper = crate::growth_runner::NoopSleeper::default();
        let clock = LocalScheduleClock;
        let sched = Scheduler::new(&exec, &sleeper, &clock);
        for i in 0..75 {
            sched.log(&format!("line {i}"));
        }
        let logs = sched.logs();
        assert_eq!(logs.len(), LOG_RING_CAPACITY);
        assert!(logs[0].ends_with("line 15"), "环形只保留最末 60 条");
        assert!(logs[59].ends_with("line 74"));
        let tail = sched.status()["logs"].as_array().unwrap().clone();
        assert_eq!(tail.len(), STATUS_LOG_TAIL);
        assert!(tail[19].as_str().unwrap().ends_with("line 74"));
    }
}
