//! 关卡 4.19（线 B）：`UpdateChecker` 后台线程 —— Python `wb_updates.UpdateChecker`
//! 的移植。
//!
//! **只发现，不安装**：拉一次 GitHub 的 releases 元数据（API 拿不到就退回网页
//! 端点）、比较版本号、把结论交给面板。**不下载、不解包、不替换文件、不重启
//! 进程**——那些属于后续阶段。
//!
//! ## Python 语义（逐条对齐，行号取自 `wb_updates.py`）
//!
//! - **三条硬约束**（`wb_updates.py:12-21`）：
//!   1. **默认关闭**。每日检查是 operator 显式打开的偏好；缺键、false 都是关闭，
//!      关闭时后台**一个请求都不发**。
//!   2. **失败不致命**。网络超时、HTTP 错误、JSON 坏了都只记一行短原因，
//!      网关照常服务。
//!   3. **只留安全运维字段**。落盘的只有「上次尝试时间 / 上次成功时间 / 见过的
//!      版本号」；URL、请求头、响应体一律不落。
//! - **首次延迟 30 秒**（`FIRST_DELAY_SECONDS`，`:50`）：`run()`（`:348-357`）
//!   第一件事是 `self._stop_event.wait(self._first_delay)` ⇒ **启动 30 秒后**
//!   才第一次检查（不是立刻跑）。目的是让监听端口和账号池先就绪。
//! - **轮询 300 秒**（`POLL_SECONDS`，`:52`）：循环体每次 `self._park(300)`。
//!   「多久看一眼」由它决定，**真正的节流靠 `due()`**。
//! - **实际检查窗口 24 小时**（`SCHEDULED_INTERVAL_SECONDS = 24*3600`，`:48`）：
//!   `due()`（`:250-254`）判「距上次**尝试**是否 ≥ 24h」。没到点就
//!   `skipped = "not due yet"`，**不发请求**。
//! - **开关缺键 = 关**（`wb_settings.py:1354-1362`：`... is True` ⇒ 缺键即
//!   关；`:1362`）。Rust 侧的口径在 `crate::settings::update_check_enabled`。
//! - **manual 绕过开关与窗口**（`check(manual=True)`，`:282-309`）：面板的
//!   「立即检查」必须真的去问一次。Rust 侧见 [`UpdateChecker::check_manual`]。
//! - **只发现不安装**：`_run_check`（`:311-333`）只取结论，不碰任何文件。
//! - **状态十键**（`status()`，`:257-271`）：`current_version / latest_version /
//!   update_available / enabled / checking / last_attempt / last_success /
//!   last_error / release_url / published_at`。Rust 侧的形状由
//!   `crate::update::build_status` 生成（键序与 Python 一致）。
//! - **记账**（`wb_settings.py:1396-1416`）：settings 的 `update_check` 键 =
//!   `{last_attempt, last_success, latest_version}`，**只有这三个字段**。
//!   `last_attempt` / `last_success` 是 **epoch 秒**（浮点）。
//!   - `_load_state()`（`:219-232`）启动时把上次的记账读回来 ⇒ 重启后面板
//!     立刻有东西可看，且**重启不是再查一次的理由**（24h 窗口照旧）。
//!   - 写路径（`wb_settings.py:1396-1416`）：`last_attempt` 每次都写；
//!     `last_success` **只在成功时**写；`latest_version` **只在版本号非空时**
//!     写。⇒ 一次失败**不会**抹掉上次的成功时间与版本号。
//! - **失败不致命**（`:317-322`）：保留上一次的结论（`self._latest` 不动），
//!   只把一行短原因报出去（`describe_failure`，`:115-130`）。
//!
//! ## Rust 侧的刻意设计
//!
//! - **数据源可注入**（[`UpdateSource`]）：抓取 / 时钟 / 开关 / 读写记账全部走
//!   trait ⇒ **测试不发任何真实上游请求**（M5 计划硬约束）。写法照抄
//!   `crates/tasks/src/credits_refresher.rs` 的 `CreditsSource`（`Pin<Box<dyn
//!   Future>>` 手工实现 async，不引入 `async-trait`）。
//! - **异步而非线程**：Python 用 `threading.Thread` + 阻塞 `wait`；这里用
//!   `tokio::spawn` + `JoinHandle`（骨架照抄 `Scheduler::start/stop/shutdown/
//!   run_loop`）。
//! - **生产数据源** [`SettingsUpdateSource`]：settings 走
//!   `wb_storage::read_settings` / `write_settings`（**顶层浅合并**，只碰
//!   `update_check` 一个键），抓取走 `wb_gateway::github::fetch_github_releases`。
//!
//! ## 与 Python 的已知差异（登记在案，不粉饰）
//!
//! 1. **`current_version`**：Python 取 `running_version()`（`wb_proxy.py:18601`，
//!    即 `Handler.server_version` 的版本段，形如 `1.7.1`）。Rust 侧没有这个
//!    概念，默认取 `env!("CARGO_PKG_VERSION")`（= `wb-tasks` 的 `0.1.0`），
//!    接线方可用 [`UpdateChecker::with_current_version`] 换成真实的发布版本号。
//!    ⚠️ 在换过来之前，`update_available` 的比较基准是 `0.1.0` ⇒ 任何正式
//!    稳定版都会被判成「有更新」。
//! 2. **`last_attempt` / `last_success` 的形状**：settings 里是 **epoch 秒**
//!    （与 Python 一致）；但面板读的 `status()` 里是**本地时间文本**
//!    （Python `_stamp`，`:186-189`，形如 `2026-10-21 12:00:00`；没查过时是
//!    `null`）。Rust 侧沿用 Python 的形状（`crate::update::stamp` ⇒
//!    `crate::credits::local_text`），面板直接显示即可。
//! 3. **网络错误文案**：Python 是 `network error: URLError`（带异常类名，
//!    `:129`）；Rust 侧没有可对应的类名，只给 `network error`
//!    （`crate::update::FailureKind::Network`）。
//! 4. **「已经有一次在跑」不排队**：Python 用 `threading.Lock` 但只在检查
//!    `_checking` 标志时短暂持有 ⇒ 语义上就是「跳过」；Rust 侧用
//!    `compare_exchange` 做同样的非阻塞跳过。
//! 5. **`wake()` 的唤醒延迟**：Python 用 `threading.Event` 立刻唤醒；Rust 侧
//!    没有 `tokio::sync` 依赖（`tokio` 只开了 `time`/`macros`/`rt`），改成
//!    **按 1 秒一片**的轮转等待 ⇒ 唤醒延迟 ≤ 1 秒。
//! 6. **`stop()` 会中断在途请求**：Python 的 `stop()` 只置事件、让当前检查
//!    自然结束；Rust 侧照抄 `Scheduler::stop` 的做法，直接 `abort()` 后台任务。
//!
//! ## 接口约定里的两处调整（相对关卡 4.19 的接口草稿）
//!
//! 1. [`UpdateSource`] 除了草稿里的三个方法（`fetch_releases` / `now` /
//!    `save_state`），还必须有 `enabled` 与 `load_state`：草稿要求的
//!    「开关缺键 = 关 ⇒ 一个请求都不发」与「启动时读回上次记账」这两条行为
//!    没有别的来源可问。
//! 2. `check_manual` 是 **`async fn`**：抓取是异步的，同步函数无法 await。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::update::{
    self, build_status, due, is_newer, FailureKind, FIRST_DELAY_SECONDS, POLL_SECONDS, TOKEN_ENV,
};

/// settings 里记账的键名。Python `wb_settings.UPDATE_CHECK_STATE_KEY`
/// （`wb_settings.py:97`）。
pub const UPDATE_CHECK_STATE_KEY: &str = "update_check";

/// 本项目（hub）的**发布版本号** —— Rust 二进制顶替的是 `wb_proxy.py`，
/// 故要报的是 hub 的版本，不是 crate 的版本。
///
/// Python 侧同一个字面量出现在两处，`tests/_test_release_engineering.py`
/// 的 `test_source_version_strings_agree` 盯着它们相等：
/// `wb_proxy.py:15207` 的 `server_version = "wb-proxy/1.7.1"` 与
/// `wb_proxy.py:9729` 的 `"version": "1.7.1"`。
///
/// ⚠️ 不填 `env!("CARGO_PKG_VERSION")`（= `0.1.0`）——那会让
/// `update_available` 把任何正式稳定版都判成「有更新」。接线方可用 env
/// `WB_VERSION` 覆盖（见 `api_routes::TaskRuntime::new`）。
pub const HUB_VERSION: &str = "1.7.1";

/// `current_version` 的默认值：`wb-tasks` 自己的版本号。
///
/// ⚠️ 见模块文档的差异 1：这不是发布版本号，接线方应当用
/// [`UpdateChecker::with_current_version`] 换成真实值。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 轮询间隔的下限（秒）。Python `self._poll_seconds = max(1.0, ...)`
/// （`wb_updates.py:203`）。
pub const MIN_POLL_SECONDS: f64 = 1.0;

/// 等待被唤醒时的轮转粒度（秒）。
///
/// Rust 侧没有 `tokio::sync::Notify` 依赖（`tokio` 只开了 `time`/`macros`/
/// `rt`），所以「睡到被唤醒」用**按片轮转**实现：每片最多 1 秒 ⇒ 唤醒延迟
/// ≤ 1 秒（Python 是立刻）。
pub const WAKE_SLICE_SECONDS: f64 = 1.0;

/// 记账里 `latest_version` 的最大长度。Python `wb_settings.py:1408`
/// （`str(latest_version or "").strip()[:32]`）。
const MAX_VERSION_LEN: usize = 32;

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// 可注入的数据源
// ---------------------------------------------------------------------------

/// 更新检查的数据源。
///
/// 全部可注入 ⇒ **测试不发任何真实上游请求**（M5 计划硬约束）。写法与
/// `crates/tasks/src/credits_refresher.rs` 的 `CreditsSource` 一致。
pub trait UpdateSource: Send + Sync {
    /// 抓一份 releases 文档（**原文**，不做版本判定）。
    ///
    /// `Err(msg)` = 抓取失败（`msg` 是一行短原因：`HTTP 403` /
    /// `invalid JSON` / `network error`）。
    fn fetch_releases(&self) -> BoxFut<'_, Result<Value, String>>;

    /// 当前 epoch 秒（等价 Python 的 `time.time()`）。
    fn now(&self) -> f64;

    /// 每日检查的总开关。**缺键 = 关**（Python `wb_settings.py:1354-1362`）。
    fn enabled(&self) -> bool;

    /// 读回上次记账（settings 的 `update_check` 键）。
    ///
    /// 等价 Python `_load_state()`（`:219-232`）读的那份
    /// `wb_settings.update_check_state()`。
    fn load_state(&self) -> Value;

    /// 写回一次检查的记账。
    ///
    /// `state` 是 `update_check` 键的**值**，只含三个字段：
    /// `last_attempt` / `last_success`（epoch 秒）与 `latest_version`（字符串）。
    /// 实现方**只写这一个顶层键**，settings 的其余键原样保留。
    fn save_state(&self, state: &Value) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

/// 检查器的内存状态（对应 Python 的 `self._latest` / `_last_attempt` /
/// `_last_success` / `_last_error`）。
#[derive(Debug, Default)]
struct State {
    /// 最近一次挑中的稳定版（Python `self._latest`）。
    ///
    /// 形状与 `crate::update::pick_latest` 的返回值一致：
    /// `{version, tag, url, published_at, newer}`。
    latest: Option<Value>,
    /// 上次**尝试**的时刻（epoch 秒）。Python `self._last_attempt`。
    last_attempt: f64,
    /// 上次**成功**的时刻（epoch 秒）。Python `self._last_success`。
    last_success: f64,
    /// 上次失败的原因（一行短原因）。Python `self._last_error`。
    last_error: String,
    /// settings 里 `update_check.latest_version` 的值。
    ///
    /// 与 `latest` 分开：Python 的 `record_update_check` 只在**版本号非空**时
    /// 才改写落盘值（`wb_settings.py:1409-1411`），而内存里的 `self._latest`
    /// 会被一次「没有稳定版」的**成功**检查清成 `None`（`wb_updates.py:325`）。
    /// 两个值因此可以不一致，这里照抄。
    persisted_version: String,
}

// ---------------------------------------------------------------------------
// 检查器
// ---------------------------------------------------------------------------

/// 后台的每日更新检查线程。
///
/// 骨架照抄 `crates/tasks/src/scheduler.rs` 的 `Scheduler` 与
/// `crates/tasks/src/credits_refresher.rs` 的 `CreditsRefresher`。
pub struct UpdateChecker {
    /// 数据源（可注入 ⇒ 测试不触网）。
    source: Arc<dyn UpdateSource>,
    /// 本进程报告的版本号（Python `current_version`）。
    current_version: String,
    /// 启动后先睡多久才第一次检查（Python `first_delay`，`:202`）。
    first_delay: f64,
    /// 轮询间隔（Python `poll_seconds`，`:203`）。
    poll_seconds: f64,
    /// 运行期状态。
    state: Mutex<State>,
    /// 是否正在检查（Python `self._checking`）。
    checking: AtomicBool,
    /// 停止位（Python `self._stop_event`）。
    stop_event: AtomicBool,
    /// 唤醒位（Python `self._wake`）。
    wake_event: AtomicBool,
    /// 后台任务句柄。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for UpdateChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateChecker")
            .field("current_version", &self.current_version)
            .field("first_delay", &self.first_delay)
            .field("poll_seconds", &self.poll_seconds)
            .finish_non_exhaustive()
    }
}

impl UpdateChecker {
    /// 构造（Python `UpdateChecker.__init__`，`:198-217` 的默认参数）。
    ///
    /// `first_delay = 30s`、`poll_seconds = 300s`、`current_version` 取
    /// [`CURRENT_VERSION`]。
    pub fn new(deps: Arc<dyn UpdateSource>) -> Self {
        Self::build(deps, CURRENT_VERSION, FIRST_DELAY_SECONDS, POLL_SECONDS)
    }

    /// 构造：指定首次延迟与轮询间隔（测试用；生产用 [`UpdateChecker::new`]）。
    ///
    /// `poll_seconds` 会被夹到 `[1.0, ...]`（Python `:203`：`max(1.0, ...)`）；
    /// `first_delay` 夹到 `[0.0, ...]`（Python `:202`）。
    pub fn with_timings(deps: Arc<dyn UpdateSource>, first_delay: f64, poll_seconds: f64) -> Self {
        Self::build(
            deps,
            CURRENT_VERSION,
            first_delay.max(0.0),
            poll_seconds.max(MIN_POLL_SECONDS),
        )
    }

    /// 构造：指定本进程报告的版本号（Python 的 `current_version` 入参）。
    ///
    /// 生产接线方**应当**用这个把 `running_version()` 的等价值传进来，否则
    /// `update_available` 的比较基准是 `wb-tasks` 自己的版本号（见模块文档的
    /// 差异 1）。
    pub fn with_current_version(deps: Arc<dyn UpdateSource>, current_version: &str) -> Self {
        Self::build(deps, current_version, FIRST_DELAY_SECONDS, POLL_SECONDS)
    }

    fn build(
        deps: Arc<dyn UpdateSource>,
        current_version: &str,
        first_delay: f64,
        poll_seconds: f64,
    ) -> Self {
        let checker = UpdateChecker {
            source: deps,
            current_version: current_version.to_string(),
            first_delay,
            poll_seconds,
            state: Mutex::new(State::default()),
            checking: AtomicBool::new(false),
            stop_event: AtomicBool::new(false),
            wake_event: AtomicBool::new(false),
            task: Mutex::new(None),
        };
        // 启动时把上次的记账读回来（Python `_load_state`，`:219-232`）。
        checker.load_state();
        checker
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 本进程报告的版本号。
    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    /// 24 小时窗口是否已经放行（Python `due()`，`:250-254`）。
    pub fn due(&self) -> bool {
        due(self.source.now(), self.lock_state().last_attempt)
    }

    /// 把上次的记账读回来（Python `_load_state()`，`:219-232`）。
    fn load_state(&self) {
        let raw = self.source.load_state();
        let last_attempt = read_epoch(&raw, "last_attempt");
        let last_success = read_epoch(&raw, "last_success");
        let version = truncate_version(
            raw.get("latest_version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim(),
        );
        // Python `:226-230`：只有记得住版本号时，内存里才有一份「最新的结论」。
        let latest = if version.is_empty() {
            None
        } else {
            Some(json!({
                "version": version,
                "tag": "",
                "url": "",
                "published_at": "",
                "newer": is_newer(&version, &self.current_version),
            }))
        };
        let mut state = self.lock_state();
        state.last_attempt = last_attempt;
        state.last_success = last_success;
        state.persisted_version = version;
        state.latest = latest;
    }

    // ---- 状态 -----------------------------------------------------------

    /// 面板要的状态（Python `status()`，`:257-271`）：十个键，键名与 Python
    /// 逐字一致。
    pub fn status(&self) -> Value {
        let enabled = self.source.enabled();
        let checking = self.checking.load(Ordering::SeqCst);
        let state = self.lock_state();
        let latest = state.latest.clone();
        build_status(
            &self.current_version,
            latest.as_ref(),
            enabled,
            checking,
            state.last_attempt,
            state.last_success,
            &state.last_error,
        )
        .to_json()
    }

    // ---- 检查 -----------------------------------------------------------

    /// 后台路径的一次检查：受**开关**与 **24h 窗口**约束。
    ///
    /// 开关关着或没到点 ⇒ **一个请求都不发**（Python `check(manual=False)`，
    /// `:282-309`）。
    pub async fn check(&self) -> Value {
        self.run_check(false).await
    }

    /// 面板「立即检查」：**绕过开关与 24h 窗口**，真的去问一次。
    ///
    /// Python `check(manual=True)`（`:282-309`）。
    pub async fn check_manual(&self) -> Value {
        self.run_check(true).await
    }

    async fn run_check(&self, manual: bool) -> Value {
        if !manual {
            if !self.source.enabled() {
                return self.skipped("disabled");
            }
            if !self.due() {
                return self.skipped("not due yet");
            }
        }
        // 已经有一次在跑 ⇒ 跳过（Python `:293-296`）。
        if self
            .checking
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return self.skipped("already running");
        }
        let _guard = CheckingGuard(&self.checking);
        self.run_check_inner().await;
        self.status()
    }

    /// 带 `skipped` 标记的状态（Python `:288` / `:290` / `:295`）。
    fn skipped(&self, reason: &str) -> Value {
        let mut status = self.status();
        status["skipped"] = json!(reason);
        status
    }

    /// 真正跑一次检查（Python `_run_check()`，`:311-333`）。
    ///
    /// 失败**不致命**：保留上一次的结论，只记一行短原因。
    async fn run_check_inner(&self) {
        let now = self.source.now();
        {
            let mut state = self.lock_state();
            // Python `:312`：尝试时间在抓取**之前**就记上 ⇒ 一次失败同样
            // 消耗掉 24 小时窗口（不会因为失败就整天不再试，也不会立刻重试）。
            state.last_attempt = now;
            state.last_error = String::new();
        }

        let document = match self.source.fetch_releases().await {
            Ok(document) => document,
            Err(message) => {
                let kind = classify_failure(&message);
                self.lock_state().last_error = kind.describe();
                self.remember(false, "", now);
                return;
            }
        };

        let Some(entries) = document.as_array() else {
            // Python `:314-315`：`not isinstance(document, list)` ⇒ ValueError。
            self.lock_state().last_error = FailureKind::UnexpectedDocument.describe();
            self.remember(false, "", now);
            return;
        };

        let picked = update::pick_latest(entries, &self.current_version);
        let version = picked
            .as_ref()
            .and_then(|entry| entry.get("version"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        {
            let mut state = self.lock_state();
            // Python `:325-327`：结论换成最新的那份（可能没有稳定版 ⇒ `None`）。
            state.latest = picked;
            state.last_success = now;
            if !version.is_empty() {
                state.persisted_version = truncate_version(&version);
            }
        }
        self.remember(true, &version, now);
    }

    /// 记账（Python `_remember()`，`:234-241`）。
    ///
    /// 写路径口径（`wb_settings.py:1396-1416`）：
    /// - `last_attempt` 每次都写；
    /// - `last_success` **只在成功时**写（失败保留上一次的值）；
    /// - `latest_version` **只在版本号非空时**写（空版本号保留上一次的值）。
    ///
    /// 只写这三个字段；**记账失败不影响一次已经完成的检查**（Python `:236-240`
    /// 把异常吞掉）。
    fn remember(&self, success: bool, version: &str, at: f64) {
        let (last_success, persisted_version) = {
            let state = self.lock_state();
            (state.last_success, state.persisted_version.clone())
        };
        let latest_version = if success && !version.trim().is_empty() {
            truncate_version(version)
        } else {
            persisted_version
        };
        let state = json!({
            "last_attempt": at,
            "last_success": last_success,
            "latest_version": latest_version,
        });
        // 记账失败不致命（Python `:236-240`）。
        let _ = self.source.save_state(&state);
    }

    // ---- 后台线程 -------------------------------------------------------

    /// 起后台循环（骨架照抄 `Scheduler::start`）。
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
    }

    /// 停止（骨架照抄 `Scheduler::stop`）：置停止位并中断后台任务。
    pub fn stop(&self) {
        self.stop_event.store(true, Ordering::SeqCst);
        let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = guard.as_ref() {
            h.abort();
        }
        *guard = None;
    }

    /// 停止后台任务（等价 [`UpdateChecker::stop`]）。
    pub fn shutdown(&self) {
        self.stop();
    }

    /// 停止并等待后台任务真正退出（测试收尾用；骨架照抄 `Scheduler::shutdown`）。
    pub async fn shutdown_and_wait(self: &Arc<Self>) {
        self.stop();
        let handle = {
            let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(h) = handle {
            // abort 之后 join 只会得到 `Err(Cancelled)`，这里仅等它退出。
            let _ = h.await;
        }
    }

    /// 叫醒停着的循环（Python `wake()`，`:335-337`）。
    ///
    /// 偏好刚从关拨到开时用。唤醒延迟 ≤ [`WAKE_SLICE_SECONDS`]（见模块文档的
    /// 差异 5）。
    pub fn wake(&self) {
        self.wake_event.store(true, Ordering::SeqCst);
    }

    /// 后台循环（Python `run()`，`:348-357`）。
    ///
    /// **先睡 30 秒再干活** ⇒ 启动后 30 秒才第一次检查；之后每 300 秒醒一次，
    /// 每次醒来都问一次「到点了吗」（`due()`），没到点就一个请求都不发。
    pub async fn run_loop(&self) {
        if self.sleep_slices(self.first_delay, false).await == SleepOutcome::Stopped {
            return;
        }
        while !self.stop_event.load(Ordering::SeqCst) {
            let _ = self.check().await;
            if self.sleep_slices(self.poll_seconds, true).await == SleepOutcome::Stopped {
                return;
            }
        }
    }

    /// 按片睡一段时长。
    ///
    /// `honor_wake = true` 时，唤醒位被置上就提前返回（Python `_park()`，
    /// `:344-346`）。首次延迟**不看**唤醒位（Python 首段等的是 `_stop_event`，
    /// `:349`）。
    async fn sleep_slices(&self, total: f64, honor_wake: bool) -> SleepOutcome {
        let mut remaining = total.max(0.0);
        while remaining > 0.0 {
            if self.stop_event.load(Ordering::SeqCst) {
                return SleepOutcome::Stopped;
            }
            if honor_wake && self.wake_event.swap(false, Ordering::SeqCst) {
                return SleepOutcome::Woken;
            }
            let slice = remaining.min(WAKE_SLICE_SECONDS);
            tokio::time::sleep(Duration::from_secs_f64(slice)).await;
            remaining -= slice;
        }
        if self.stop_event.load(Ordering::SeqCst) {
            SleepOutcome::Stopped
        } else {
            SleepOutcome::Elapsed
        }
    }
}

/// 后台循环的一次等待的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepOutcome {
    /// 时间到了。
    Elapsed,
    /// 被 `wake()` 叫醒。
    Woken,
    /// 被 `stop()` 叫停。
    Stopped,
}

/// 抢到「正在检查」标志后释放（panic 也释放，等价 Python 的 `try/finally`）。
struct CheckingGuard<'a>(&'a AtomicBool);

impl Drop for CheckingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 失败原因分类
// ---------------------------------------------------------------------------

/// 把一行失败原因归类（Python `describe_failure()`，`:115-130`）。
///
/// 只认三种短原因（`HTTP <码>` / `invalid JSON` / `network error`）——它们
/// 是 `wb_gateway::github` 与本模块约定的**唯一**失败文案，不带 URL、请求头
/// 与响应体。其余一律归为「未知」。
pub fn classify_failure(message: &str) -> FailureKind {
    let text = message.trim();
    if let Some(rest) = text.strip_prefix("HTTP ") {
        if let Ok(code) = rest.trim().parse::<u16>() {
            return FailureKind::Http(code);
        }
    }
    match text {
        "invalid JSON" => FailureKind::InvalidJson,
        "unexpected release document" => FailureKind::UnexpectedDocument,
        "network error" => FailureKind::Network,
        _ => FailureKind::Unknown,
    }
}

// ---------------------------------------------------------------------------
// 生产数据源
// ---------------------------------------------------------------------------

/// 生产数据源：settings 走 `wb_storage`，抓取走 `wb_gateway::github`。
///
/// - 开关：settings 的 `update_check_enabled`（**缺键 = 关**，
///   `crate::settings::update_check_enabled`）；
/// - 记账：settings 的 `update_check` 键（只写这一个顶层键，其余键原样保留）；
/// - 抓取：`wb_gateway::github::fetch_github_releases`（带 `GITHUB_TOKEN`
///   时加 `Authorization`；API 失败自动退回网页端点）。
pub struct SettingsUpdateSource {
    accounts_dir: PathBuf,
    client: wb_gateway::outbound::UpstreamClient,
}

impl SettingsUpdateSource {
    /// 构造（用默认出站客户端）。
    pub fn new(accounts_dir: impl Into<PathBuf>) -> Self {
        Self {
            accounts_dir: accounts_dir.into(),
            client: wb_gateway::outbound::UpstreamClient::new(),
        }
    }

    /// 构造（自带出站客户端，便于复用连接池）。
    pub fn with_client(
        accounts_dir: impl Into<PathBuf>,
        client: wb_gateway::outbound::UpstreamClient,
    ) -> Self {
        Self {
            accounts_dir: accounts_dir.into(),
            client,
        }
    }

    /// 账号目录（settings 所在目录）。
    pub fn accounts_dir(&self) -> &Path {
        &self.accounts_dir
    }

    /// 读一份 settings（读不出来 ⇒ 空对象，Python `wb_settings.load()` 的
    /// 宽容口径）。
    fn settings(&self) -> Value {
        wb_storage::read_settings(&self.accounts_dir).unwrap_or_else(|_| json!({}))
    }
}

impl UpdateSource for SettingsUpdateSource {
    fn fetch_releases(&self) -> BoxFut<'_, Result<Value, String>> {
        Box::pin(async move {
            // token 只从环境变量读，绝不落盘（Python `wb_updates.py:155`）。
            let token = std::env::var(TOKEN_ENV).ok();
            wb_gateway::github::fetch_github_releases(&self.client, token.as_deref()).await
        })
    }

    fn now(&self) -> f64 {
        now_epoch()
    }

    fn enabled(&self) -> bool {
        crate::settings::update_check_enabled(&self.settings())
    }

    fn load_state(&self) -> Value {
        self.settings()
            .get(UPDATE_CHECK_STATE_KEY)
            .cloned()
            .unwrap_or_else(|| json!({}))
    }

    fn save_state(&self, state: &Value) -> Result<(), String> {
        // 顶层浅合并 ⇒ 只动 `update_check` 一个键，settings 的其余键原样保留。
        wb_storage::write_settings(
            &self.accounts_dir,
            &json!({ UPDATE_CHECK_STATE_KEY: state }),
        )
        .map_err(|err| format!("写 settings 失败: {err}"))
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 当前 epoch 秒（等价 Python 的 `time.time()`）。
fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 从记账里读一个 epoch 秒字段。
///
/// Python `wb_settings.update_check_state()`（`:1389-1393`）只接受**数字**且
/// **大于 0** 的值（布尔不算数字）。
fn read_epoch(state: &Value, key: &str) -> f64 {
    state
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .unwrap_or(0.0)
}

/// 版本号截断（Python `wb_settings.py:1394-1395` / `:1409-1411`）。
fn truncate_version(version: &str) -> String {
    version.trim().chars().take(MAX_VERSION_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_reasons_are_classified() {
        assert_eq!(classify_failure("HTTP 403"), FailureKind::Http(403));
        assert_eq!(classify_failure("invalid JSON"), FailureKind::InvalidJson);
        assert_eq!(classify_failure("network error"), FailureKind::Network);
        assert_eq!(
            classify_failure("unexpected release document"),
            FailureKind::UnexpectedDocument
        );
        assert_eq!(classify_failure("boom"), FailureKind::Unknown);
        assert_eq!(classify_failure("HTTP 403").describe(), "HTTP 403");
    }

    #[test]
    fn epoch_fields_only_accept_positive_numbers() {
        let state = json!({"last_attempt": 100.5, "last_success": 0.0});
        assert_eq!(read_epoch(&state, "last_attempt"), 100.5);
        assert_eq!(read_epoch(&state, "last_success"), 0.0);
        assert_eq!(read_epoch(&state, "missing"), 0.0);
        assert_eq!(
            read_epoch(&json!({"last_attempt": true}), "last_attempt"),
            0.0
        );
    }

    #[test]
    fn versions_are_truncated_to_32_chars() {
        let long = "v".repeat(64);
        assert_eq!(truncate_version(&long).len(), MAX_VERSION_LEN);
        assert_eq!(truncate_version("  1.6.18  "), "1.6.18");
    }
}
