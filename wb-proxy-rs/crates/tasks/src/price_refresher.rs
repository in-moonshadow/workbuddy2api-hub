//! 关卡 4.19（线 A）：`PriceRefresher` 后台线程 —— Python `wb_pricing.PriceRefresher`
//! 的移植。
//!
//! ## Python 语义（逐条对齐，行号取自 `wb_pricing.py`）
//!
//! - **默认间隔 5 分钟**：`DEFAULT_REFRESH_MINUTES = 5`（`:2303`）；实际间隔读
//!   settings 的 `pricing_refresh_minutes`（`wb_settings.py:912-935`），
//!   `0` = 关闭刷新，上限 `MAX_PRICING_REFRESH_MINUTES = 24*30*60`
//!   （`wb_settings.py:32`）。单位换算只在一处发生：`interval_seconds()`
//!   （`:2336-2338`）= `max(0.0, minutes) * 60.0`。
//! - **总开关 `pricing_enabled`**（`wb_settings.py:980-989`）：**缺键 = 开**
//!   （`True if value is None else value is True`）。总开关关着时
//!   **park 不计时**——不取价、不写盘、不走计时器（`:2477-2484`）。
//! - **首次启动立刻抓一次**（`:2465-2508` 的 `run()`）：
//!   `should_fetch = came_from_off or not load_timeline()` ⇒ **只有「没有任何
//!   价格历史」时才立即抓**。已经取过价的正常重启**不重复抓**（先等一个
//!   间隔）。总开关从关切回开时（`came_from_off`）**一定**立即抓一次——这是
//!   补算的关键：关闭期间没取价，打开后这一份要落到那段时间的请求上。
//! - **抓取失败沿用上一版价格、不写盘**（`:2389-2398`）：
//!   `fetch_openrouter()` 抛异常 ⇒ 记 `last_error`、记一行日志、**直接返回**，
//!   **不碰策略表**。⇒ 盘上仍是上一版价格。
//! - **抓到了但一个都没匹配上**（`:2415-2419`）：记 `last_error` =
//!   「抓到了模型列表，但一个都没匹配上」、**不写盘**。
//! - **落盘**（Python `record_policies`，`:1940-2008`）：策略表
//!   `pricing-policies.jsonl` + 时间线 `pricing-timeline.jsonl`，都在**数据
//!   目录**（`usage/`）下（`_data_paths()`，`:1456-1468`）。
//! - **`status()`**（`:2511-2554`）至少要回：`interval_minutes` / `enabled` /
//!   `master_enabled` / `last_run` / `last_error` / `logs`；`enabled` 是
//!   **总开关与间隔的相与**（`master and interval_minutes > 0`），
//!   `master_enabled` 单独回报开关本身。
//!
//! ## Rust 侧的刻意设计
//!
//! - **数据源可注入**（[`PriceSource`]）：抓取 / 落盘 / 时钟 / 数据目录 /
//!   总开关 / 取价清单全部走 trait ⇒ **测试不发任何真实上游请求**（M5 计划
//!   硬约束）。写法照抄 `crates/tasks/src/update_checker.rs` 的 `UpdateSource`
//!   与 `crates/tasks/src/credits_refresher.rs` 的 `CreditsSource`
//!   （`Pin<Box<dyn Future>>` 手工实现 async，**不引入 `async-trait`**）。
//! - **异步而非线程**：Python 用 `threading.Thread` + 阻塞 `wait`；这里用
//!   `tokio::spawn` + `JoinHandle`（骨架照抄 `Scheduler::start/stop/shutdown/
//!   run_loop`）。
//! - **塑形复用既有判定层**：抓回来的原文交给
//!   [`crate::price::build_snapshot`]（Python `build_snapshot`，`:1368-1408`）
//!   塑形，本模块不重复实现匹配逻辑。
//!
//! ## 与 Python 的已知差异（登记在案，不粉饰）
//!
//! 1. **取价清单只覆盖静态目录，live 目录那一半仍缺**：Python 的
//!    `priced_model_ids()`（`:1157-1192`）= 内置静态目录
//!    （`wb_catalog.STATIC_*_MODELS`）∪ 网关 live 目录新增。**静态目录已于
//!    关卡 4.20 落地**（[`crate::catalog`]：54 个 id，与 Python 逐字逐序
//!    一致，有跨语言 parity 测试盯着）；**live 目录那一半仍缺**——它需要
//!    「远端产品目录抓取 + `is_chat_model` 过滤 + 变体整理」整条链路，Rust
//!    侧没有。按 Python 的语义，缺它只是**少覆盖**（那部分模型落进
//!    `unpriced`），不会让覆盖变小。
//! 2. **策略表的行粒度不同**：Python 的策略表是**一个价格一行**（按内容
//!    哈希去重，`policy_id()`，`:1612-1622`），时间线只在**指派真的变了**时
//!    才追加一行。本移植按关卡 4.19 的接口约定：策略表**每轮追加整份快照
//!    一行**（`SnapshotBuild.doc`），时间线**每轮追加一行时间戳条目**。
//!    ⇒ 稳态下两个文件都会每轮增长（Python 不会）。**读这两个文件的消费端
//!    必须按本移植的格式实现**，不能照抄 Python 的读取逻辑。
//! 3. **`run_once` 多查一次间隔**：Python 的 `run_once()` 只看总开关，间隔是
//!    循环自己 park 的；本移植的 `run_once()` 额外在**间隔为 0** 时直接返回
//!    （关卡 4.19 的接口约定要求「间隔为 0 时不刷」）。
//! 4. **并发保护用非阻塞标志**：Python 用会**阻塞**的 `threading.Lock`
//!    （`:2391`）；Rust 侧若已有一次刷新在跑，本次直接返回 `None`（跳过）。
//! 5. **`wake()` 的唤醒延迟**：Python 用 `threading.Event` 立刻唤醒；Rust 侧
//!    没有 `tokio::sync` 依赖（`tokio` 只开了 `time`/`macros`/`rt`），改成
//!    **按片轮转**等待 ⇒ 唤醒延迟 ≤ [`WAKE_SLICE_SECONDS`]。
//!
//! ## 接线提示
//!
//! - **间隔**由调用方从 settings 读出来传进 [`PriceRefresher::new`]；设置变更
//!   时调 [`PriceRefresher::set_interval_minutes`]（语义等价 Python 的
//!   `set_interval()`，`:2340-2343`）。
//! - **总开关**由 [`PriceSource::master_enabled`] 提供（**缺键 = 开**）。
//! - **HTTP 那一半**在 `wb_gateway::pricing::fetch_openrouter_models`。

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// 策略表文件名。Python `wb_pricing.py:1457`（`_data_paths()` 的第一个返回值）。
pub const POLICIES_FILE: &str = "pricing-policies.jsonl";

/// 时间线文件名。Python `wb_pricing.py:1458`（`_data_paths()` 的第二个返回值）。
pub const TIMELINE_FILE: &str = "pricing-timeline.jsonl";

/// 默认刷新间隔（分钟）。Python `DEFAULT_REFRESH_MINUTES = 5`（`:2303`）。
pub const DEFAULT_REFRESH_MINUTES: f64 = 5.0;

/// 等待被唤醒时的轮转粒度（秒）。
///
/// Rust 侧没有 `tokio::sync::Notify` 依赖（`tokio` 只开了 `time`/`macros`/
/// `rt`），所以「睡到被唤醒」用**按片轮转**实现 ⇒ 唤醒延迟 ≤ 一片
/// （Python 是立刻）。
pub const WAKE_SLICE_SECONDS: f64 = 0.2;

/// 内部日志环形缓冲容量。Python `self.logs` 保留 40 条（`:2330-2333`）。
pub const LOG_RING_CAPACITY: usize = 40;

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// 可注入的数据源
// ---------------------------------------------------------------------------

/// 取价输入清单（Python `priced_model_ids()` 的 Rust 侧等价物）。
///
/// Python 的清单是「内置静态目录 ∪ 网关 live 目录新增」，去重保序、静态在前
/// （`:1157-1192`）。**静态目录已落地**（[`crate::catalog`]，54 个 id）；
/// **live 目录那一半仍缺**（见模块文档的差异 1）⇒ 默认是空清单。
#[derive(Debug, Clone)]
pub struct PriceCatalog {
    /// 要取价的 hub 模型 id（去重保序）。
    pub hub_ids: Vec<String>,
    /// 面板手填的 hub → OpenRouter id 映射。
    ///
    /// Python 的 `runtime_overrides()`（`:1511-1525`）读的是数据目录里的
    /// `pricing-overrides.json`。
    pub runtime_overrides: HashMap<String, String>,
    /// 变体后缀继承是否打开。**缺键 = 开**（Python
    /// `variant_inherit_enabled()`，`:1254-1259`）。
    pub variants: bool,
}

impl Default for PriceCatalog {
    fn default() -> Self {
        PriceCatalog {
            hub_ids: Vec::new(),
            runtime_overrides: HashMap::new(),
            variants: true,
        }
    }
}

/// 定价刷新的数据源。
///
/// 全部可注入 ⇒ **测试不发任何真实上游请求**（M5 计划硬约束）。写法与
/// `crates/tasks/src/update_checker.rs` 的 `UpdateSource` 一致。
pub trait PriceSource: Send + Sync {
    /// 抓一份上游模型列表（**原文**，不做任何塑形）。
    ///
    /// `Err(msg)` = 抓取失败（网络错误 / 非 2xx / 坏 JSON）。调用方据此
    /// **沿用上一版价格**、不写盘（Python `:2395-2398`）。
    fn fetch_models(&self) -> BoxFut<'_, Result<Value, String>>;

    /// 把一份价格快照落盘。
    ///
    /// 默认实现写数据目录下的两个文件（策略表 + 时间线），见
    /// [`write_snapshot_files`]。实现方可以覆盖（例如接到别处的存储）。
    fn save_snapshot(&self, doc: &Value) -> Result<(), String> {
        write_snapshot_files(self.usage_dir(), doc, self.now())
    }

    /// 当前 epoch 秒（等价 Python 的 `time.time()`）。
    fn now(&self) -> f64;

    /// 数据目录（Python 的 `data_dir()`，`:1428-1437`）——策略表与时间线
    /// 落在这里。
    fn usage_dir(&self) -> &Path;

    /// 定价功能总开关。**缺键 = 开**（Python `wb_settings.py:980-989`）。
    ///
    /// 默认实现给 `true`（= 缺键口径）；生产实现要读 settings 的
    /// `pricing_enabled` 键（Rust 侧的口径是 `crate::settings::pricing_enabled`）。
    fn master_enabled(&self) -> bool {
        true
    }

    /// 取价输入清单。默认**空清单**——生产实现见
    /// [`crate::price_source::SettingsPriceSource`]：静态目录已落地
    /// （[`crate::catalog`]，54 个 id），live 目录那一半仍缺（模块文档的
    /// 差异 1）。
    fn catalog(&self) -> PriceCatalog {
        PriceCatalog::default()
    }
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

/// 刷新器的内存状态（对应 Python 的 `self.last_run` / `self.last_error` /
/// `self.last_models` / `self.last_unpriced` / `self.next_run` / `self.logs`）。
#[derive(Debug, Default)]
struct State {
    /// 上次**成功**取价的时刻（epoch 秒）。Python `:2327`。
    last_run: Option<f64>,
    /// 上次失败的错误文案。Python `:2328`。
    last_error: Option<String>,
    /// 上次成功取价时定价到的模型数。Python `:2329`。
    last_models: usize,
    /// 上次没匹配到价格的模型 id。Python `:2331`。
    last_unpriced: Vec<String>,
    /// 上次命中人工覆盖表的模型 id。Python `:2431` 的 `overridden`。
    last_overridden: Vec<String>,
    /// 下次刷新时刻（epoch 秒）。park 中为 `None`。Python `:2332`。
    next_run: Option<f64>,
    /// 环形日志。Python `:2333`。
    logs: VecDeque<String>,
}

impl State {
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

/// 后台的定价刷新线程。
///
/// 骨架照抄 `crates/tasks/src/scheduler.rs` 的 `Scheduler` 与
/// `crates/tasks/src/update_checker.rs` 的 `UpdateChecker`。
pub struct PriceRefresher {
    /// 数据源（可注入 ⇒ 测试不触网）。
    source: Arc<dyn PriceSource>,
    /// 当前刷新间隔（秒）。`0` = 关闭刷新。
    interval_seconds: Mutex<f64>,
    /// 运行期状态。
    state: Mutex<State>,
    /// 停止位（Python `self._stop_event`，`:2350`）。
    stop_event: AtomicBool,
    /// 唤醒位（Python `self._wake`，`:2346`）。
    wake_event: AtomicBool,
    /// 防止两次刷新并发（Python 的 `self._run_lock`，`:2391`）。
    run_lock: AtomicBool,
    /// 后台任务句柄。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for PriceRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriceRefresher")
            .field("interval_seconds", &self.interval_seconds())
            .finish_non_exhaustive()
    }
}

impl PriceRefresher {
    /// 构造。
    ///
    /// `interval_seconds` 是刷新间隔（秒）；`0`（或负数）= **关闭刷新**。
    /// 生产接线方从 settings 的 `pricing_refresh_minutes` 读出分钟数后乘 60
    /// 传进来（Python 的 `interval_seconds()`，`:2336-2338`）。
    pub fn new(deps: Arc<dyn PriceSource>, interval_seconds: f64) -> Self {
        PriceRefresher {
            source: deps,
            interval_seconds: Mutex::new(interval_seconds.max(0.0)),
            state: Mutex::new(State::default()),
            stop_event: AtomicBool::new(false),
            wake_event: AtomicBool::new(false),
            run_lock: AtomicBool::new(false),
            task: Mutex::new(None),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前刷新间隔（秒）。`0` = 关闭。
    pub fn interval_seconds(&self) -> f64 {
        *self
            .interval_seconds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 设置刷新间隔（分钟）。`0` = 关闭刷新。
    ///
    /// Python `set_interval()`（`:2340-2343`）：写新间隔并**叫醒**正在等待的
    /// 循环 ⇒ 新的间隔立刻生效（不用等旧间隔走完）。
    pub fn set_interval_minutes(&self, minutes: f64) {
        let minutes = if minutes.is_finite() && minutes > 0.0 {
            minutes
        } else {
            0.0
        };
        *self
            .interval_seconds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = minutes * 60.0;
        self.wake();
    }

    /// 设置刷新间隔（秒）。`0` = 关闭刷新。
    pub fn set_interval_seconds(&self, seconds: f64) {
        let seconds = if seconds.is_finite() && seconds > 0.0 {
            seconds
        } else {
            0.0
        };
        *self
            .interval_seconds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = seconds;
        self.wake();
    }

    /// 叫醒停着的循环（Python `wake()`，`:2345-2347`）。
    ///
    /// 总开关从关拨到开时用。唤醒延迟 ≤ [`WAKE_SLICE_SECONDS`]。
    pub fn wake(&self) {
        self.wake_event.store(true, Ordering::SeqCst);
    }

    /// 策略表路径。
    pub fn policies_path(&self) -> PathBuf {
        self.source.usage_dir().join(POLICIES_FILE)
    }

    /// 时间线路径。
    pub fn timeline_path(&self) -> PathBuf {
        self.source.usage_dir().join(TIMELINE_FILE)
    }

    /// 跑一轮取价。
    ///
    /// 返回值：
    /// - `None` = 本轮**没有产出**：
    ///   1. 总开关关着（Python `:2393-2394`，文案「价估算已关闭」）；
    ///   2. 间隔为 0（关卡 4.19 的接口约定；Python 是循环 park 而不调
    ///      `run_once`）；
    ///   3. 抓取失败且**盘上没有上一版可保留**；
    ///   4. 已经有一轮在跑（跳过）。
    /// - `Some(msg)` = 本轮跑了：`msg` 为空表示成功，非空为错误文案
    ///   （`last_error` 同时记录）。
    ///
    /// **抓取失败时绝不写盘**（Python `:2395-2398`）⇒ 盘上仍是上一版价格。
    pub async fn run_once(&self) -> Option<String> {
        // 非阻塞互斥（Python `:2391` 的 `with self._run_lock`）
        if self
            .run_lock
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        let _guard = RunLockGuard(&self.run_lock);

        // 总开关关着 ⇒ 什么都不做（Python `:2393-2394`）。
        if !self.source.master_enabled() {
            self.log("价估算已关闭");
            return None;
        }
        // 间隔为 0 ⇒ 关闭刷新（Python 的循环会 park，这里直接返回）。
        if self.interval_seconds() <= 0.0 {
            self.log("定价刷新间隔为 0，不取价");
            return None;
        }

        // 抓取（Python `:2395-2398`：异常等价于一次失败）。
        let raw = match self.source.fetch_models().await {
            Ok(raw) => raw,
            Err(err) => {
                let text = if err.trim().is_empty() {
                    "未知错误".to_string()
                } else {
                    err
                };
                self.lock_state().last_error = Some(text.clone());
                self.log(&format!("抓取失败，继续用上一版价格：{text}"));
                // 盘上还有上一版 ⇒ 沿用它（本轮没有产出，但仍报告错误）。
                return if self.has_previous_snapshot() {
                    Some(text)
                } else {
                    None
                };
            }
        };

        // 塑形（Python `build_snapshot()`，`:1368-1408`）。
        let table = openrouter_table(&raw);
        let catalog = self.source.catalog();
        let built = crate::price::build_snapshot(
            Some(&table),
            &catalog.hub_ids,
            None,
            &catalog.runtime_overrides,
            catalog.variants,
            &local_timestamp(),
        );
        let models = built
            .doc
            .get("models")
            .and_then(Value::as_object)
            .map(|m| m.len())
            .unwrap_or(0);

        // 抓到了但一个都没匹配上（Python `:2415-2419`）：不写盘。
        if models == 0 {
            let msg = "抓到了模型列表，但一个都没匹配上";
            self.lock_state().last_error = Some(msg.to_string());
            self.log(msg);
            return Some(msg.to_string());
        }

        // 落盘（Python `record_policies()`，`:1940-2008`）。
        let now = self.source.now();
        if let Err(err) = self.source.save_snapshot(&built.doc) {
            let text = format!("落盘失败：{err}");
            self.lock_state().last_error = Some(text.clone());
            self.log(&text);
            return Some(text);
        }

        {
            let mut state = self.lock_state();
            state.last_run = Some(now);
            state.last_error = None;
            state.last_models = models;
            state.last_unpriced = built.unpriced.clone();
            state.last_overridden = built.overridden.clone();
        }
        self.log(&format!(
            "已取价：{models} 个模型（人工覆盖 {}，未定价 {}）",
            built.overridden.len(),
            built.unpriced.len()
        ));
        Some(String::new())
    }

    /// 盘上是否已经有**上一版价格**（策略表非空）。
    ///
    /// Python 侧等价物是「策略表里有没有行」。用于区分「抓取失败但有旧价可
    /// 沿用」与「抓取失败且什么都没有」。
    fn has_previous_snapshot(&self) -> bool {
        file_has_content(&self.policies_path())
    }

    /// 是否已经有价格历史（时间线非空）。
    ///
    /// Python `not load_timeline()`（`:2490`）——「没有任何价格历史」是
    /// 「启动后立刻抓一次」的判据。
    fn has_timeline(&self) -> bool {
        file_has_content(&self.timeline_path())
    }

    /// 记一条日志（Python `log()`，`:2326-2334`）。
    fn log(&self, msg: &str) {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        self.lock_state().push_log(format!("[{stamp}] {msg}"));
    }

    /// 记下次刷新时刻（park 中为 `None`）。
    fn set_next_run(&self, at: Option<f64>) {
        self.lock_state().next_run = at;
    }

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
        self.log("后台定价刷新器已启动");
    }

    /// 停止（骨架照抄 `Scheduler::stop`）：置停止位并中断后台任务。
    pub fn stop(&self) {
        self.stop_event.store(true, Ordering::SeqCst);
        self.wake();
        let mut guard = self.task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = guard.as_ref() {
            h.abort();
        }
        *guard = None;
    }

    /// 停止后台任务（等价 [`Self::stop`]）。
    pub fn shutdown(&self) {
        self.stop();
    }

    /// 停止并等待后台任务真正退出（测试收尾用）。
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

    /// 后台循环（Python `run()`，`:2465-2508`）。
    ///
    /// 1. 总开关关着 ⇒ **park 不计时**（不取价、不写盘、不走计时器）；
    /// 2. 间隔为 0 ⇒ 同样 park；
    /// 3. 首次（或从关闭切回打开）时**按需立刻抓一次**：
    ///    「从关闭切回打开」一定抓；「首次启动」只在**没有任何价格历史**时抓；
    /// 4. 否则等一个间隔，再抓一次。
    pub async fn run_loop(&self) {
        let mut first = true;
        let mut came_from_off = false;
        while !self.stop_event.load(Ordering::SeqCst) {
            if !self.source.master_enabled() {
                // 总开关关着：不取价也不计时（Python `:2477-2484`）。
                self.set_next_run(None);
                if self.park().await == SleepOutcome::Stopped {
                    return;
                }
                came_from_off = true;
                continue;
            }
            let seconds = self.interval_seconds();
            if seconds <= 0.0 {
                self.set_next_run(None);
                if self.park().await == SleepOutcome::Stopped {
                    return;
                }
                came_from_off = true;
                continue;
            }
            if first || came_from_off {
                // Python `:2490`：`should_fetch = came_from_off or not load_timeline()`
                let should_fetch = came_from_off || !self.has_timeline();
                first = false;
                came_from_off = false;
                if should_fetch {
                    let _ = self.run_once().await;
                }
            }
            self.set_next_run(Some(self.source.now() + seconds));
            if self.sleep_slices(seconds).await == SleepOutcome::Stopped {
                return;
            }
            if self.stop_event.load(Ordering::SeqCst) {
                return;
            }
            let _ = self.run_once().await;
        }
    }

    /// park（总开关关着 / 间隔为 0 时）：**不计时**，只等唤醒。
    ///
    /// Python 的 `self._wake.wait()`（`:2481`、`:2488`）。
    async fn park(&self) -> SleepOutcome {
        loop {
            if self.stop_event.load(Ordering::SeqCst) {
                return SleepOutcome::Stopped;
            }
            if self.wake_event.swap(false, Ordering::SeqCst) {
                return SleepOutcome::Woken;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(WAKE_SLICE_SECONDS)).await;
        }
    }

    /// 按片睡一段时长；唤醒位被置上就提前返回。
    async fn sleep_slices(&self, total: f64) -> SleepOutcome {
        let mut remaining = total.max(0.0);
        while remaining > 0.0 {
            if self.stop_event.load(Ordering::SeqCst) {
                return SleepOutcome::Stopped;
            }
            if self.wake_event.swap(false, Ordering::SeqCst) {
                return SleepOutcome::Woken;
            }
            let slice = remaining.min(WAKE_SLICE_SECONDS);
            tokio::time::sleep(std::time::Duration::from_secs_f64(slice)).await;
            remaining -= slice;
        }
        if self.stop_event.load(Ordering::SeqCst) {
            SleepOutcome::Stopped
        } else {
            SleepOutcome::Elapsed
        }
    }

    /// 后台任务是否还活着。
    fn is_running(&self) -> bool {
        self.task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// 面板要的状态（Python `status()`，`:2511-2554`）。
    ///
    /// 键名与 Python 逐字一致的部分：`interval_minutes` / `enabled` /
    /// `master_enabled` / `running` / `last_run` / `last_run_label` /
    /// `next_run` / `next_run_label` / `last_error` / `last_models` /
    /// `policies` / `models` / `policies_file` / `timeline` / `logs`。
    ///
    /// Python 的 `current` / `gaps` / `gap_summary` / `overrides_file` /
    /// `variant_inherit` 依赖模型目录与未定价清单，**Rust 侧尚未落地**
    /// （见模块文档的差异 1）⇒ 这里用 `last_unpriced` / `last_overridden`
    /// 顶上，缺口登记在关卡 4.19 的差异清单里。
    pub fn status(&self) -> Value {
        let master = self.source.master_enabled();
        let seconds = self.interval_seconds();
        let state = self.lock_state();
        let logs: Vec<String> = state.logs.iter().cloned().collect();
        json!({
            "interval_minutes": round2(seconds / 60.0),
            "interval_seconds": seconds,
            // 总开关关着时，无论间隔填多少都不取价，所以 enabled 是两者相与
            // （Python `:2521`）；master_enabled 单独回报开关本身。
            "enabled": master && seconds > 0.0,
            "master_enabled": master,
            "running": self.is_running(),
            "last_run": state.last_run,
            "last_run_label": state.last_run.map(local_label),
            "next_run": state.next_run,
            "next_run_label": state.next_run.map(local_label),
            "last_error": state.last_error,
            "last_models": state.last_models,
            "last_unpriced": state.last_unpriced,
            "last_overridden": state.last_overridden,
            "policies": count_lines(&self.policies_path()),
            "models": state.last_models,
            "policies_file": self.policies_path().to_string_lossy(),
            "timeline": self.timeline_path().to_string_lossy(),
            "logs": logs,
        })
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

/// 抢到锁后释放（panic 也释放，等价 Python 的 `try/finally`）。
struct RunLockGuard<'a>(&'a AtomicBool);

impl Drop for RunLockGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

/// 把一份价格快照写进数据目录的两个文件。
///
/// - `pricing-policies.jsonl`：**每行一条快照**（整份 `SnapshotBuild.doc`）；
/// - `pricing-timeline.jsonl`：**每行一条时间戳条目**
///   （`{at, generated_at, models}`）。
///
/// 两者都是 **append**，用 `crate::activity::append_row`（同目录建目录 +
/// 追加一行，坏行不影响其它行）。
///
/// ⚠️ 与 Python 的行粒度不同（见模块文档的差异 2）：Python 的策略表是
/// **一个价格一行**、时间线只在指派变了时才追加。这里按关卡 4.19 的接口
/// 约定，每轮各追加一行。
pub fn write_snapshot_files(usage_dir: &Path, doc: &Value, at: f64) -> Result<(), String> {
    let policies = usage_dir.join(POLICIES_FILE);
    let timeline = usage_dir.join(TIMELINE_FILE);

    crate::activity::append_row(&policies, doc)?;

    let models = doc
        .get("models")
        .and_then(Value::as_object)
        .map(|m| m.len())
        .unwrap_or(0);
    let generated_at = doc
        .get("meta")
        .and_then(|m| m.get("generated_at"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let entry = json!({
        "at": at,
        "generated_at": generated_at,
        "models": models,
    });
    crate::activity::append_row(&timeline, &entry)?;
    Ok(())
}

/// 把上游原文折成 `{id: pricing}` 表。
///
/// Python `fetch_openrouter()`（`:1135-1136`）的返回值语义：
/// `{m.get("id"): (m.get("pricing") or {}) for m in payload.get("data", [])}`。
///
/// 差别：Python 会把缺 `id` 的条目收成 `None` 键，Rust 侧**跳过**这种条目。
pub fn openrouter_table(raw: &Value) -> HashMap<String, Value> {
    let mut table = HashMap::new();
    let Some(data) = raw.get("data").and_then(Value::as_array) else {
        return table;
    };
    for item in data {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let pricing = item
            .get("pricing")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        table.insert(id.to_string(), pricing);
    }
    table
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 本地时间戳（Python 的 `time.strftime("%Y-%m-%dT%H:%M:%S")`）。
fn local_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// 本地时间标签（Python 的 `time.strftime("%Y-%m-%d %H:%M")`）。
fn local_label(epoch_sec: f64) -> String {
    chrono::DateTime::from_timestamp(epoch_sec as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// 文件是否至少有一行非空内容。
fn file_has_content(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().any(|line| !line.trim().is_empty()),
        Err(_) => false,
    }
}

/// 文件有多少行非空内容。
fn count_lines(path: &Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().filter(|line| !line.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

/// Python `round(x, 2)` 的等价物。
fn round2(value: f64) -> f64 {
    format!("{value:.2}").parse().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_table_folds_data_into_id_to_pricing() {
        let raw = json!({
            "data": [
                {"id": "openai/gpt-4o", "pricing": {"prompt": "0.0000025"}},
                {"id": "  ", "pricing": {}},
                {"pricing": {"prompt": "0.1"}},
                {"id": "x/y", "pricing": null},
            ]
        });
        let table = openrouter_table(&raw);
        assert_eq!(table.len(), 2, "缺 id / 空 id 的条目要跳过");
        assert_eq!(table["openai/gpt-4o"]["prompt"], "0.0000025");
        assert_eq!(table["x/y"], json!({}), "pricing 为 null ⇒ 空对象");
    }

    #[test]
    fn openrouter_table_tolerates_missing_data() {
        assert!(openrouter_table(&json!({})).is_empty());
        assert!(openrouter_table(&json!({"data": "x"})).is_empty());
    }

    #[test]
    fn default_catalog_is_empty_but_variants_on() {
        let catalog = PriceCatalog::default();
        assert!(catalog.hub_ids.is_empty());
        assert!(catalog.runtime_overrides.is_empty());
        assert!(catalog.variants, "变体继承缺键 = 开");
    }
}
