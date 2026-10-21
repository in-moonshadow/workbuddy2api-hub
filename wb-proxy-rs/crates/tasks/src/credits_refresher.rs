//! 关卡 4.18（线 B）：后台积分刷新线程 —— Python `wb_accounts.CreditsRefresher` 的移植。
//!
//! ## Python 语义（逐条对齐，行号取自 `wb_accounts.py`）
//!
//! - **每 tick 只刷一个最旧的账号**（`:2452-2468` 的类文档）：不是刷全池。
//!   上游计费调用因此有硬上界 —— 默认半小时一 tick ⇒ 每天最多 48 次，与池子
//!   大小无关。
//! - **tick 间隔**：`interval_seconds` 默认 `1800`（`:2471`），构造时夹到
//!   `max(60.0, ...)`（`:2474`）。
//! - **TTL**：`ttl_seconds()`（`:2498-2508`）每次现读 settings 的
//!   `credits_refresh_hours`；TTL ≤ 0 ⇒ `refresh_once()` 直接返回 `None`
//!   （`:2540-2542`），**一个都不刷**。
//! - **候选筛选**（`stalest_account()`，`:2510-2536`）：
//!   1. `enabled` 且有 `access_token`（`:2522-2523`）；
//!   2. 不在失败冷却里（`_parked.get(uid, 0) > now` ⇒ 跳过，`:2524-2525`）；
//!   3. 快照年龄 ≥ TTL（`age < ttl_seconds` ⇒ 跳过，`:2532-2533`）；
//!   4. 多个候选里取**积分快照最旧**的（`age > chosen_age` 才替换 ⇒ 并列时取
//!      先出现的那个，`:2534-2535`）。
//! - **从未抓过 = 无限旧**（`:2527-2531`）：`updated_at` 缺失或不是数字 ⇒
//!   `age = inf` ⇒ 优先于任何已抓过的账号（Python 测试
//!   `tests/_test_expiring_priority.py:387-392` 覆盖了这条）。
//! - **失败冷却 6 小时**（`:2483` `_failed_cooldown = 6 * 3600.0`）。
//!   ⚠️ Python 的失败日志文案写「暂缓一小时」（`:2556-2557`）**是错的** ——
//!   实际冷却是 6 小时（Python 测试 `tests/_test_expiring_priority.py:468-471`
//!   明确断言 `_failed_cooldown == 6 * 3600`）。本移植**不照抄那句文案**。
//! - **成功路径**（`:2559-2564`）：记 `last_run` / `last_uid`、清 `last_error`、
//!   返回被刷账号的 uid。**失败路径**（`:2551-2558`）：记 `last_error`、把该
//!   uid 停放到 `now + 6h`、**不**更新 `last_run` / `last_uid`、返回 `None`。
//! - **线程先睡后干**（`run()`，`:2566-2576`）：循环体第一件事是
//!   `self._wake.wait(timeout=self.interval_seconds)` ⇒ **启动后 30 分钟才第一次
//!   跑**，不是立刻跑。
//! - **`status()` 六个键**（`:2578-2586`）：`ttl_hours` / `interval_seconds` /
//!   `last_run` / `last_uid` / `last_error` / `logs`，`logs` 取末 10 条。
//!
//! ## Rust 侧的刻意设计
//!
//! - **数据源可注入**（[`CreditsSource`]）：抓取 / 落盘 / 账号清单 / 时钟全部
//!   走 trait ⇒ **测试不发任何真实上游请求**（M5 计划硬约束）。写法照抄
//!   `crates/tasks/src/scheduler.rs` 的 `CycleExecutor`（`Pin<Box<dyn Future>>`
//!   手工实现 async，不引入 `async-trait`）。
//! - **`run_once` 是 `async fn`**：`fetch_credits` 是异步的，同步函数无法 await。
//!   （接口草图里的 `pub fn run_once` 只能是笔误，语义不变。）
//! - **异步而非线程**：Python 用 `threading.Thread` + 阻塞 `wait`；这里用
//!   `tokio::spawn` + `JoinHandle`（骨架照抄 `Scheduler::start/stop/shutdown/
//!   run_loop`）。
//! - **冷却表只在内存里**：Python 的 `_parked`（`:2482`）同样只在内存里，
//!   进程重启即清空 —— 这里保持一致（不落盘）。
//!
//! ## 与 Python 的已知差异（登记在案）
//!
//! 1. **成功后清除失败冷却记录**：Python 成功时不清 `_parked`（那条记录此时
//!    必定已过期，行为上等价）；本移植在成功时直接移除该 uid 的记录
//!    （接口约定要求「失败记录清除」）。
//! 2. **并发保护用非阻塞标志**（`AtomicBool` + `compare_exchange`）：Python 用
//!    会**阻塞**的 `threading.Lock`（`:2543`）。Rust 侧若已有一次刷新在跑，
//!    本次直接返回 `None`（跳过），不排队。
//! 3. **`logs` 环形缓冲容量 10**（Python 内部保留 40 条、`status()` 只回末 10
//!    条：`:2488-2489` 与 `:2585`）。`status()` 的可观察输出与 Python 一致。
//! 4. **TTL 由构造参数注入**（`new(deps, ttl_hours)`）：Python 每次 tick 现读
//!    settings（`:2500-2504`），Rust 侧由调用方在设置变更时调
//!    [`CreditsRefresher::set_ttl_hours`]（语义等价，见该方法注释）。

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::scheduler::uid8;
use crate::settings::clamp_credits_refresh_hours;

// ---------------------------------------------------------------------------
// 常量表（`wb_accounts.py:2471-2483`）
// ---------------------------------------------------------------------------

/// tick 间隔（秒）。Python `CreditsRefresher.__init__` 的默认 `interval_seconds=1800`。
pub const INTERVAL_SECONDS: f64 = 1800.0;

/// tick 间隔的下限（秒）。Python `self.interval_seconds = max(60.0, ...)`（`:2474`）。
pub const MIN_INTERVAL_SECONDS: f64 = 60.0;

/// 抓取失败后的冷却（秒）= 6 小时。Python `self._failed_cooldown = 6 * 3600.0`（`:2483`）。
///
/// ⚠️ Python 的失败日志文案写「暂缓一小时」（`:2556`）与这个值矛盾 —— 实际是
/// 6 小时（Python 测试 `tests/_test_expiring_priority.py:468-471` 断言）。
pub const FAILED_COOLDOWN_SECONDS: f64 = 6.0 * 3600.0;

/// `status()["logs"]` 只回末 10 条（Python `:2585` 的 `self.logs[-10:]`）。
pub const STATUS_LOG_TAIL: usize = 10;

/// 内部日志环形缓冲容量。
///
/// Python 内部保留 40 条（`:2488-2489`）、`status()` 只回末 10 条；这里直接用
/// 容量 10 的环形缓冲 —— `status()` 的可观察输出与 Python 一致。
pub const LOG_RING_CAPACITY: usize = STATUS_LOG_TAIL;

// ---------------------------------------------------------------------------
// 可注入的数据源
// ---------------------------------------------------------------------------

/// 刷新器看到的账号视图。
#[derive(Debug, Clone, PartialEq)]
pub struct RefresherAccount {
    /// 账号 uid（失败冷却与日志都按它定位）。
    pub uid: String,
    /// 账号凭据文件名（落盘时定位用）。
    pub file_name: String,
    /// 账号是否启用。停用的账号不参与刷新（Python `:2522`）。
    pub enabled: bool,
    /// 是否有 `access_token`。没 token 的账号刷不了（Python `:2522`）。
    pub has_token: bool,
    /// 积分快照的 `updated_at`（epoch 秒）。
    ///
    /// `None` = 从未抓过（或抓到的快照里没有可用的 `updated_at`）⇒ Python 侧
    /// 视为**无限旧**，优先刷新（`:2527-2531`）。
    pub credits_updated_at: Option<f64>,
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 积分刷新的数据源。
///
/// 全部可注入 ⇒ **测试不发任何真实上游请求**（M5 计划硬约束）。写法与
/// `crates/tasks/src/scheduler.rs` 的 `CycleExecutor` 一致：`Pin<Box<dyn Future>>`
/// 手工实现 async，不引入 `async-trait`。
pub trait CreditsSource: Send + Sync {
    /// 账号池快照（等价 Python 的 `self.pool.accounts`）。
    fn accounts(&self) -> Vec<RefresherAccount>;

    /// 当前 epoch 秒（等价 Python 的 `time.time()`）。
    fn now(&self) -> f64;

    /// 抓取某账号的积分快照。
    ///
    /// `Err(msg)` = 抓取失败（`msg` 为错误文案，Python 的 `error` 字段）；
    /// `Ok(value)` = 成功，`value` 是**要落盘的积分快照**（形状见
    /// [`crate::credits::Credits::to_json`]）。
    fn fetch_credits<'a>(&'a self, acc: &'a RefresherAccount) -> BoxFut<'a, Result<Value, String>>;

    /// 把抓到的积分快照写回账号文件（Python 的 `self.save(...)`，`:1971-1972`）。
    fn save_credits(&self, acc: &RefresherAccount, credits: &Value) -> Result<(), String>;
}

/// 刷新器的内部状态（对应 Python 的 `self.last_run` / `last_uid` /
/// `last_error` / `self.logs` / `self._parked`）。
struct State {
    /// 上次**成功**刷新的时刻（epoch 秒）。Python `:2475`。
    last_run: Option<f64>,
    /// 上次成功刷新的账号 uid。Python `:2476`。
    last_uid: Option<String>,
    /// 上次失败的错误文案。Python `:2477`。
    last_error: Option<String>,
    /// 环形日志（Python `:2478`）。
    logs: VecDeque<String>,
    /// 失败冷却表：uid ⇒ 冷却到期的 epoch 秒。Python `self._parked`（`:2482`）。
    parked: HashMap<String, f64>,
}

impl State {
    fn new() -> Self {
        State {
            last_run: None,
            last_uid: None,
            last_error: None,
            logs: VecDeque::new(),
            parked: HashMap::new(),
        }
    }

    fn push_log(&mut self, msg: String) {
        if self.logs.len() >= LOG_RING_CAPACITY {
            self.logs.pop_front();
        }
        self.logs.push_back(msg);
    }
}

// ---------------------------------------------------------------------------
// 刷新器
// ---------------------------------------------------------------------------

/// 后台积分刷新器。
///
/// 每 tick 只刷**一个**最旧的账号（Python `:2452-2468`）；线程先睡一个 tick
/// 再干活（`:2566-2576`）。
pub struct CreditsRefresher {
    /// 数据源（可注入 ⇒ 测试不触网）。
    source: Arc<dyn CreditsSource>,
    /// 当前 TTL（小时）。`0` = 关闭刷新。
    ttl_hours: Mutex<f64>,
    /// 运行期状态。
    state: Mutex<State>,
    /// 停止位（Python `self._stop_event`，`:2479`）。
    stop_event: AtomicBool,
    /// 防止两次刷新并发（Python 的 `self._lock`，`:2480`）。
    run_lock: AtomicBool,
    /// 后台任务句柄。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for CreditsRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreditsRefresher")
            .field("ttl_hours", &self.ttl_hours())
            .finish_non_exhaustive()
    }
}

impl CreditsRefresher {
    /// 构造。
    ///
    /// `ttl_hours` 会被夹到 `[0, 720]`（Python `wb_settings.credits_refresh_hours()`
    /// 的上限口径，`wb_settings.py:41`）；`0` = **关闭**刷新（`:2540-2542`）。
    pub fn new(deps: Arc<dyn CreditsSource>, ttl_hours: f64) -> Self {
        CreditsRefresher {
            source: deps,
            ttl_hours: Mutex::new(clamp_credits_refresh_hours(ttl_hours)),
            state: Mutex::new(State::new()),
            stop_event: AtomicBool::new(false),
            run_lock: AtomicBool::new(false),
            task: Mutex::new(None),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前 TTL（小时）。`0` = 关闭刷新。
    pub fn ttl_hours(&self) -> f64 {
        *self.ttl_hours.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 设置 TTL（小时）。`0` = 关闭刷新。
    ///
    /// Python 每次 tick 现读 settings（`:2500-2504`），所以设置改了下一次 tick
    /// 就生效；Rust 侧由调用方在设置变更时调这个方法，语义等价。
    pub fn set_ttl_hours(&self, hours: f64) {
        let clamped = clamp_credits_refresh_hours(hours);
        *self.ttl_hours.lock().unwrap_or_else(|e| e.into_inner()) = clamped;
    }

    /// 当前 TTL（秒）。`0` = 关闭。
    fn ttl_seconds(&self) -> f64 {
        self.ttl_hours().max(0.0) * 3600.0
    }

    /// 某账号的失败冷却何时到期（epoch 秒）。未在冷却中 ⇒ `None`。
    ///
    /// 供接线方观察状态与测试断言用（Python 的 `_parked` 是私有属性）。
    pub fn parked_until(&self, uid: &str) -> Option<f64> {
        self.lock_state().parked.get(uid).copied()
    }

    /// 选出本轮要刷的账号：快照最旧且已过 TTL 的那一个。
    ///
    /// 逐条对齐 Python `stalest_account()`（`:2510-2536`）。`None` = 本轮没有
    /// 该刷的账号。
    pub fn stalest_account(&self, now: f64) -> Option<RefresherAccount> {
        let ttl_seconds = self.ttl_seconds();
        if ttl_seconds <= 0.0 {
            return None;
        }
        let state = self.lock_state();
        let mut chosen: Option<(RefresherAccount, f64)> = None;
        for acc in self.source.accounts() {
            // 1. 停用 / 没 token 的账号跳过（Python `:2522-2523`）
            if !acc.enabled || !acc.has_token {
                continue;
            }
            // 2. 失败冷却中跳过（Python `:2524-2525`）
            if state.parked.get(&acc.uid).copied().unwrap_or(0.0) > now {
                continue;
            }
            // 3. 快照年龄：从未抓过 = 无限旧（Python `:2526-2531`）
            let age = match acc.credits_updated_at {
                Some(updated) => now - updated,
                None => f64::INFINITY,
            };
            // 4. 未过 TTL 的跳过（Python `:2532-2533`）
            if age < ttl_seconds {
                continue;
            }
            // 5. 取最旧的；并列时保留先出现的那个（Python `:2534-2535`）
            let better = match &chosen {
                None => true,
                Some((_, best_age)) => age > *best_age,
            };
            if better {
                chosen = Some((acc, age));
            }
        }
        chosen.map(|(acc, _)| acc)
    }

    /// 跑一轮刷新：刷一个账号，返回它的 uid。
    ///
    /// 返回 `None` 的四种情形（与 Python `refresh_once()` 一致）：
    /// 1. TTL ≤ 0 ⇒ 刷新被关闭（Python `:2540-2542`）；
    /// 2. 本轮没有该刷的账号（`:2545-2546`）；
    /// 3. 抓取失败 ⇒ 记 `last_error`、停放该 uid 6 小时（`:2551-2558`）；
    /// 4. 已有一次刷新在跑 ⇒ 跳过（Python 这里是阻塞等待，见模块文档的差异登记）。
    pub async fn run_once(&self) -> Option<String> {
        // 非阻塞互斥（Python `:2543` 的 `with self._lock`）
        if self
            .run_lock
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        let _guard = RunLockGuard(&self.run_lock);

        let ttl_seconds = self.ttl_seconds();
        if ttl_seconds <= 0.0 {
            return None;
        }
        let now = self.source.now();
        let account = self.stalest_account(now)?;
        let uid = account.uid.clone();

        // 抓取（Python `:2547-2550`：异常等价于一次失败）
        let fetched = self.source.fetch_credits(&account).await;

        let credits = match fetched {
            Ok(credits) => credits,
            Err(err) => {
                self.record_failure(&uid, now, &err);
                return None;
            }
        };

        // 落盘。Python 的落盘发生在 `fetch_credits` 内部（`:1971-1972`），
        // 落盘异常会冒泡成一次失败（`:2549-2550`）—— 这里同样算失败。
        if let Err(err) = self.source.save_credits(&account, &credits) {
            self.record_failure(&uid, now, &err);
            return None;
        }

        self.record_success(&uid, now, &credits);
        Some(uid)
    }

    /// 记录一次失败：记 `last_error`、停放该 uid 6 小时。
    ///
    /// 逐条对齐 Python `:2551-2558`：**不**更新 `last_run` / `last_uid`。
    fn record_failure(&self, uid: &str, now: f64, error: &str) {
        let text = if error.trim().is_empty() {
            "unknown error".to_string()
        } else {
            error.to_string()
        };
        let id = uid8(uid);
        {
            let mut state = self.lock_state();
            state
                .parked
                .insert(uid.to_string(), now + FAILED_COOLDOWN_SECONDS);
            state.last_error = Some(text.clone());
            // ⚠️ Python 这里的文案写「暂缓一小时」（`:2556`）是错的：实际冷却
            // 是 6 小时（`:2483`）。这里写正确的文案。
            state.push_log(format!("刷新 {id} 积分失败，暂缓 6 小时: {text}"));
        }
    }

    /// 记录一次成功：记 `last_run` / `last_uid`、清 `last_error`、清冷却记录。
    ///
    /// 逐条对齐 Python `:2559-2564`。
    fn record_success(&self, uid: &str, now: f64, credits: &Value) {
        let id = uid8(uid);
        let updated_iso = credits
            .get("updated_iso")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        {
            let mut state = self.lock_state();
            state.last_run = Some(now);
            state.last_uid = Some(uid.to_string());
            state.last_error = None;
            // Python 不清 `_parked`，但成功时那条记录必定已过期（等冷却到期才
            // 会再被选中），清掉与 Python 可观察行为一致。
            state.parked.remove(uid);
            state.push_log(format!("已刷新 {id} 的积分（{updated_iso}）"));
        }
    }

    /// 起后台循环（`start()`，骨架照抄 `Scheduler::start`）。
    ///
    /// 需要 `Arc<Self>` 且当前处于 tokio runtime 内。
    pub fn start(self: &Arc<Self>) {
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
        self.log("后台积分刷新器已启动");
    }

    /// 停止（`stop()`，骨架照抄 `Scheduler::stop`）：置停止位并中断后台任务。
    pub fn stop(&self) {
        self.stop_event.store(true, Ordering::SeqCst);
        self.log("后台积分刷新器已暂停");
        let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = guard.as_ref() {
            h.abort();
        }
        *guard = None;
    }

    /// 停止后台任务（等价 [`Self::stop`]；保留这个别名是为了与接线方的
    /// `stop` / `shutdown` 命名习惯一致）。
    pub fn shutdown(&self) {
        self.stop();
    }

    /// 停止并等待后台任务真正退出（测试收尾用）。
    ///
    /// 骨架照抄 `Scheduler::shutdown`（`crates/tasks/src/scheduler.rs`）。
    pub async fn shutdown_and_wait(self: &Arc<Self>) {
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

    /// 后台循环（`run()`，`:2566-2576`）。
    ///
    /// **先睡一个 tick 再干活** ⇒ 启动后 30 分钟才第一次刷新。
    pub async fn run_loop(&self) {
        while !self.stop_event.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs_f64(INTERVAL_SECONDS)).await;
            if self.stop_event.load(Ordering::SeqCst) {
                return;
            }
            let _ = self.run_once().await;
        }
    }

    /// 记一条日志（Python `log()`，`:2485-2489`）。
    fn log(&self, msg: &str) {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        self.lock_state().push_log(format!("[{stamp}] {msg}"));
    }

    /// `status()`（`:2578-2586`）：六个键，键名与 Python 逐字一致。
    pub fn status(&self) -> Value {
        let state = self.lock_state();
        let logs: Vec<String> = state.logs.iter().cloned().collect();
        json!({
            "ttl_hours": round2(self.ttl_hours()),
            "interval_seconds": INTERVAL_SECONDS,
            "last_run": state.last_run,
            "last_uid": state.last_uid,
            "last_error": state.last_error,
            "logs": logs,
        })
    }
}

/// 抢到锁后释放（panic 也释放，等价 Python 的 `try/finally`）。
struct RunLockGuard<'a>(&'a AtomicBool);

impl Drop for RunLockGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Python `round(x, 2)` 的等价物。
///
/// Rust 的 `f64::round` 是「四舍五入远离零」，Python 的 `round` 是「最近偶数」
/// —— 在 `.005` 这种边界上两者不同。用十进制格式化（Rust 的浮点格式化是精确
/// 转换 + 就近取偶）来对齐 Python。
fn round2(value: f64) -> f64 {
    format!("{value:.2}").parse().unwrap_or(value)
}
