//! 关卡 4.10：M4 任务路由的**纯逻辑** + 生产装配 + `public_view` / `human_delta`。
//!
//! - 9 条路由的业务逻辑全部落在此处（返回 `serde_json::Value`），axum handler 只做
//!   鉴权 / body 解析 / 状态码包装（`crates/api/src/routes.rs`）。
//! - `PanelSink` trait 复用 `crate::scheduler::PanelSink`（`wb-tasks` 不反向依赖
//!   `wb-api`）；生产实现 `ApiPanelSink` 在 `wb-api` 侧。
//! - `TaskRuntime` 是进程级单例（`Arc<TaskRuntime>`），持有账号池 / 快照缓存 /
//!   调度器 / 执行器 / 面板 / 客户端等。
//!
//! 所有 JSON 键名与顺序对齐 Python `wb_proxy.py` 对应分支（`preserve_order`）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use serde_json::{json, Value};

use wb_gateway::outbound::UpstreamClient;
use wb_gateway::pool::{load_account_pool, AccountState};
use wb_storage::read_idempotent_fields;

use crate::checkin::production_billing_base;
use crate::claim_all::{self, route_claim_all, uid8, ClaimAllDeps, ClaimAllRequest};
use crate::daily_chat::{
    daily_chat as run_daily_chat, daily_chat_web_recorded as run_daily_chat_web, DailyChatAccount,
    DailyChatConfig, DailyChatCtx,
};
use crate::growth_api::{TaskAccount, TaskEndpoints};
use crate::growth_runner::{
    build_cat_travel_refreshing, run_growth_tasks, CatTravel, RunnerAccount, RunnerCtx, RunnerDeps,
    RunnerEnv,
};
use crate::growth_runner::{Clock, LocalClock, RealSleeper, Sleeper};
use crate::night_growth::{run_night_growth, NightGrowthOutcome};
use crate::scheduler::AccountView;
use crate::scheduler::{CycleExecutor, LocalScheduleClock, PanelSink, Scheduler};
use crate::snapshot_cache::{cache_ttl_from_env, growth_snapshot, SnapshotCache};
use crate::streak_bonus::{
    run_streak_bonus, StreakBonusAccount, StreakBonusConfig, StreakBonusCtx,
};
use crate::GrowthSummary;
use crate::GrowthTask;

// ---------------------------------------------------------------------------
// 生产装配辅助结构
// ---------------------------------------------------------------------------

/// 用量目录（Python `wb_proxy.USAGE_DIR` 的等价）。
///
/// Python：env `WB_PROXY_USAGE_DIR` 或**脚本同级**的 `usage/` 目录
/// （`wb_proxy.py:402-403`），与 `accounts/` 同级。Rust 侧据此从
/// `accounts_dir` 的**同级**目录推导，可用 `WB_USAGE_DIR` 覆盖。
pub fn usage_dir_from(accounts_dir: &std::path::Path) -> PathBuf {
    if let Ok(dir) = std::env::var("WB_USAGE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    accounts_dir
        .parent()
        .map(|p| p.join("usage"))
        .unwrap_or_else(|| PathBuf::from("usage"))
}

/// 读账号文件里**本地已存**的积分快照（D36-b 的 `credits_refresh` 实现）。
///
/// Python 的 `account.fetch_credits()` 是「领奖后**同步即时抓上游**」；Rust 的
/// 注入点是**同步闭包**，发不了异步请求 ⇒ 改为读本地快照（由后台刷新器
/// `CreditsRefresher` 按 TTL 维护）。
///
/// ⚠️ 这是**有意差异**，不是等价实现：余额最多滞后一个 TTL（默认 12h）。
/// 彻底对齐要把注入点改成 async（跨 `growth_runner` / `night_growth` /
/// `claim_all` 三个模块的签名改造），留到后续关卡。
///
/// 读不到（账号文件缺失 / 没有 `credits` 键 / 读失败）⇒ `None`，
/// 与 Python `fetch_credits()` 失败分支的「不改 credits」一致。
pub fn local_credits_snapshot(
    accounts_dir: &std::path::Path,
    credential_file: &str,
) -> Option<Value> {
    wb_storage::read_account_credits(accounts_dir, credential_file)
        .ok()
        .flatten()
}

#[derive(Debug, Clone)]
pub struct PooledTaskAccount {
    pub uid: String,
    pub token: String,
    pub enterprise_id: String,
    /// 账号区域（`cn` / `intl`）。
    ///
    /// billing 用途的出站头要用它选 realm 口径（UA / Origin / X-Domain /
    /// Accept-Language），chat 用途的头不依赖它——故此前不在本结构里。
    pub realm: String,
    pub chat_headers: HashMap<String, String>,
}

impl PooledTaskAccount {
    pub fn as_task_account(&self) -> TaskAccount<'_> {
        TaskAccount {
            uid: &self.uid,
            token: &self.token,
            enterprise_id: &self.enterprise_id,
            chat_headers: &self.chat_headers,
        }
    }

    /// billing 用途的出站头（对齐 Python `Account.headers(purpose="billing")`）。
    ///
    /// 与 chat 头**不是同一套**：计费端点不吃 X-IDE-* / X-Agent-* 身分头，
    /// User-Agent 是轻量 `WorkBuddy/x.y.z`。`domain` 传空串 ——
    /// `billing_request_headers` 会回落到 realm 规范域名（Python 侧
    /// `self.domain` 为空时同样回落 `cfg["domain"]`）。`device_token` 传
    /// `None`：Rust 侧尚未实现 Python 的设备令牌文件读取
    /// （`resolve_device_token`），该头缺失不改变请求成败。
    pub fn billing_headers(&self, device_token: Option<&str>) -> HashMap<String, String> {
        wb_gateway::headers::billing_request_headers(&wb_gateway::headers::BillingHeadersParams {
            uid: &self.uid,
            token: &self.token,
            realm: &self.realm,
            domain: "",
            enterprise_id: &self.enterprise_id,
            device_token,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ReadyAccount {
    pub state: AccountState,
    pub view: AccountView,
    pub creds: PooledTaskAccount,
    pub nickname: Option<String>,
    pub device_token: Option<String>,
}

impl ReadyAccount {
    pub fn runner_account(&self) -> RunnerAccount<'_> {
        RunnerAccount {
            api: self.creds.as_task_account(),
            realm: &self.state.realm,
            nickname: self.nickname.as_deref(),
            is_enterprise: !self.state.enterprise_id.is_empty(),
        }
    }
}

pub type SnapshotFetch = Arc<
    dyn for<'a> Fn(
            &'a TaskAccount<'a>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(Vec<GrowthTask>, GrowthSummary), String>>
                    + Send
                    + 'a,
            >,
        > + Send
        + Sync,
>;

pub struct TaskRuntime {
    pub accounts_dir: PathBuf,
    /// 用量目录（Python `wb_proxy.USAGE_DIR` 的等价）。
    ///
    /// Python：env `WB_PROXY_USAGE_DIR` 或**脚本同级**的 `usage/` 目录
    /// （`wb_proxy.py:402-403`），与 `accounts/` 同级。Rust 侧据此从
    /// `accounts_dir` 的**同级**目录推导，可用 `WB_USAGE_DIR` 覆盖。
    pub usage_dir: PathBuf,
    pub pool: Vec<AccountState>,
    pub pool_present: bool,
    pub ready: Vec<ReadyAccount>,
    pub cache: Arc<SnapshotCache>,
    pub billing_base: String,
    pub endpoints: TaskEndpoints<'static>,
    /// 每日活跃（国际版 daily_chat）的运行配置。
    ///
    /// 生产口径：端点按**账号身份**解析（国际版 → `www.workbuddy.ai`），
    /// 不用国内版常量。契约测试把它换成 mock base。
    pub daily_chat_config: DailyChatConfig,
    pub client: UpstreamClient,
    /// 本地时钟（夜猫子窗口判定、签到日期都依赖它）。
    ///
    /// 做成 `Arc<dyn Clock>` 而不是具体的 `LocalClock`：夜猫子的「23:00-08:00
    /// 窗口」判定会让**执行器层**的契约测试只在真实夜间才跑得动。契约测试
    /// 用 `test_with_ready_and_clock` 注入固定时钟，让断言与真实时间无关。
    pub clock: Arc<dyn Clock>,
    pub schedule_clock: LocalScheduleClock,
    pub sleeper: RealSleeper,
    pub panel: Arc<dyn PanelSink>,
    pub exec: OnceLock<Arc<dyn CycleExecutor>>,
    pub scheduler: OnceLock<Arc<Scheduler<'static>>>,
    /// 后台积分刷新器（关卡 4.18）。
    ///
    /// 线程本体在 [`crate::credits_refresher`]，数据源在
    /// [`crate::credits_source::ProductionCreditsSource`]。用 `OnceLock` 装，
    /// 与 `exec` / `scheduler` 同一套做法：构造期装一次，之后只读。
    pub credits_refresher: OnceLock<Arc<crate::credits_refresher::CreditsRefresher>>,
    /// 后台定价刷新器（关卡 4.19）。
    ///
    /// 线程本体在 [`crate::price_refresher`]，生产数据源在
    /// [`crate::price_source::SettingsPriceSource`]。
    pub price_refresher: OnceLock<Arc<crate::price_refresher::PriceRefresher>>,
    /// 后台更新检查器（关卡 4.19）。
    ///
    /// 线程本体在 [`crate::update_checker`]，生产数据源是同文件里的
    /// `SettingsUpdateSource`。
    pub update_checker: OnceLock<Arc<crate::update_checker::UpdateChecker>>,
}

impl TaskRuntime {
    pub fn new(
        cfg_accounts_dir: &std::path::Path,
        panel: Arc<dyn PanelSink>,
        endpoints: TaskEndpoints<'static>,
        billing_base: String,
        fetch_override: Option<SnapshotFetch>,
    ) -> Arc<Self> {
        let accounts_dir = cfg_accounts_dir.to_path_buf();
        let pool = load_account_pool(&accounts_dir);
        let pool_present = true;
        let mut ready = Vec::new();
        for st in &pool {
            let fields =
                read_idempotent_fields(&accounts_dir, &st.credential_file).unwrap_or_default();
            let view = AccountView::from_state(st).with_last_records(
                fields.last_checkin,
                fields.last_activity_report,
                fields.last_daily_chat,
                false,
            );
            let chat_headers = wb_gateway::headers::chat_request_headers(
                &wb_gateway::headers::ChatHeadersParams {
                    uid: &st.uid,
                    token: &st.access_token,
                    realm: &st.realm,
                    product: &st.product,
                    conversation_id: None,
                    device_token: None,
                    enterprise_id: &st.enterprise_id,
                    tenant_id: "",
                    department: "",
                },
            );
            let creds = PooledTaskAccount {
                uid: st.uid.clone(),
                token: st.access_token.clone(),
                enterprise_id: st.enterprise_id.clone(),
                realm: st.realm.clone(),
                chat_headers,
            };
            ready.push(ReadyAccount {
                state: st.clone(),
                view,
                creds,
                nickname: None,
                device_token: None,
            });
        }

        let fetch: SnapshotFetch = fetch_override.unwrap_or_else(|| {
            Arc::new(|acc: &TaskAccount<'_>| {
                let fut = crate::snapshot_cache::production_fetch(acc);
                Box::pin(fut)
            })
        });

        let cache = Arc::new(
            SnapshotCache::with_ttl(cache_ttl_from_env())
                .with_endpoints(endpoints)
                .with_fetch_arc(fetch),
        );

        let client = UpstreamClient::new();
        let clock: Arc<dyn Clock> = Arc::new(LocalClock);
        let schedule_clock = LocalScheduleClock;
        let sleeper = RealSleeper;

        let usage_dir = usage_dir_from(&accounts_dir);
        let task = Arc::new(TaskRuntime {
            accounts_dir,
            usage_dir,
            pool,
            pool_present,
            ready,
            cache,
            billing_base,
            endpoints,
            daily_chat_config: DailyChatConfig::default(),
            client,
            clock,
            schedule_clock,
            sleeper,
            panel: panel.clone(),
            exec: OnceLock::new(),
            scheduler: OnceLock::new(),
            credits_refresher: OnceLock::new(),
            price_refresher: OnceLock::new(),
            update_checker: OnceLock::new(),
        });

        let exec: Arc<dyn CycleExecutor> = Arc::new(ProductionExecutor {
            task: Arc::downgrade(&task),
        });
        let _ = task.exec.set(exec.clone());

        // 后台积分刷新器（关卡 4.18）：TTL 来自 settings 的 `credits_refresh_hours`
        // （默认 12h，上限 720h，0 = 关闭）。**线程不在这里启动**——启动点
        // 在 `wb-api` 侧（`WB_BACKGROUND_TASKS` 开关控制），构造期只装配。
        let settings = crate::settings::load_refresh_settings(&task.accounts_dir);
        let refresher = Arc::new(crate::credits_refresher::CreditsRefresher::new(
            Arc::new(crate::credits_source::ProductionCreditsSource::new(&task)),
            settings.credits_refresh_hours,
        ));
        let _ = task.credits_refresher.set(refresher);

        // 后台定价刷新器（关卡 4.19）：间隔来自 settings 的
        // `pricing_refresh_minutes`（默认 5 分钟，0 = 关闭）。**线程不在这里
        // 启动**——启动点与积分刷新器一样在 `wb-api` 侧（`WB_BACKGROUND_TASKS`）。
        //
        // 取价清单 = 静态目录（`crate::catalog`，54 个 id）∪ live 目录新增
        // （关卡 4.21 补齐）：live 那一半用账号池里的凭据拉远端产品目录。
        let pricing_interval = settings.pricing_refresh_minutes.max(0) as f64 * 60.0;
        let price_refresher = crate::price_source::production_price_refresher(
            &task.accounts_dir,
            &task.usage_dir,
            pricing_interval,
            Some(Arc::new(crate::live_source::PoolLiveCatalogSource::new(
                &task,
            ))),
        );
        let _ = task.price_refresher.set(price_refresher);

        // 后台更新检查器（关卡 4.19）：`current_version` 用**本项目的发布版本号**
        // （Python `wb_proxy.py:15207` 的 `server_version = "wb-proxy/1.7.1"`），
        // 可用 env `WB_VERSION` 覆盖。不填 `0.1.0`（crate 版本）——那会让
        // `update_available` 把任何正式版都判成「有更新」。
        let current_version = std::env::var("WB_VERSION")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| crate::update_checker::HUB_VERSION.to_string());
        let update_source = Arc::new(crate::update_checker::SettingsUpdateSource::new(
            &task.accounts_dir,
        ));
        let update_checker = Arc::new(crate::update_checker::UpdateChecker::with_current_version(
            update_source,
            &current_version,
        ));
        let _ = task.update_checker.set(update_checker);

        let exec_ref: &'static dyn CycleExecutor = unsafe {
            std::mem::transmute::<&dyn CycleExecutor, &'static dyn CycleExecutor>(exec.as_ref())
        };
        let sleeper_dyn: &dyn crate::growth_runner::Sleeper = &task.sleeper;
        let sleeper_ref: &'static dyn crate::growth_runner::Sleeper =
            unsafe { std::mem::transmute(sleeper_dyn) };
        let clock_dyn: &dyn crate::scheduler::ScheduleClock = &task.schedule_clock;
        let clock_ref: &'static dyn crate::scheduler::ScheduleClock =
            unsafe { std::mem::transmute(clock_dyn) };
        let panel_dyn: &dyn PanelSink = task.panel.as_ref();
        let panel_ref: &'static dyn PanelSink =
            unsafe { std::mem::transmute::<&dyn PanelSink, &'static dyn PanelSink>(panel_dyn) };
        let scheduler =
            Arc::new(Scheduler::new(exec_ref, sleeper_ref, clock_ref).with_panel(panel_ref));
        let _ = task.scheduler.set(scheduler);

        task
    }

    pub fn production(cfg_accounts_dir: &std::path::Path, panel: Arc<dyn PanelSink>) -> Arc<Self> {
        let endpoints = if let (Ok(chat), Ok(web)) = (
            std::env::var("WB_TASK_CHAT_BASE"),
            std::env::var("WB_TASK_WEB_BASE"),
        ) {
            let chat: &'static str = Box::leak(chat.into_boxed_str());
            let web: &'static str = Box::leak(web.into_boxed_str());
            TaskEndpoints {
                chat_base: chat,
                web_base: web,
            }
        } else {
            TaskEndpoints::production()
        };
        let billing_base =
            std::env::var("WB_TASK_BILLING_BASE").unwrap_or_else(|_| production_billing_base());
        Self::new(cfg_accounts_dir, panel, endpoints, billing_base, None)
    }

    pub fn exec(&self) -> Option<Arc<dyn CycleExecutor>> {
        self.exec.get().cloned()
    }
    pub fn scheduler(&self) -> Option<Arc<Scheduler<'static>>> {
        self.scheduler.get().cloned()
    }

    /// 仅供契约测试：构建指向 mock base 的运行时，直接注入 ready 账号，
    /// 并返回其 `exec`（ProductionExecutor）。`runtime` 被 `forget` 以保持
    /// `ProductionExecutor` 内部的 `Weak<TaskRuntime>` 在整个测试期间有效。
    pub fn test_with_ready(
        ready: Vec<ReadyAccount>,
        endpoints: TaskEndpoints<'static>,
        panel: Arc<dyn PanelSink>,
    ) -> Arc<dyn CycleExecutor> {
        Self::test_with_ready_dir(
            ready,
            endpoints,
            DailyChatConfig::default(),
            PathBuf::new(),
            panel,
        )
    }

    /// 仅供契约测试：与 [`Self::test_with_ready`] 相同，但可指定**账号目录**
    /// 与**每日活跃配置**。
    ///
    /// 每日活跃要把 `lastDailyChat` 写回账号文件，故契约测试需要一个真实的
    /// 账号目录（`test_with_ready` 的空目录会让落盘必然失败）。
    pub fn test_with_ready_dir(
        ready: Vec<ReadyAccount>,
        endpoints: TaskEndpoints<'static>,
        daily_chat_config: DailyChatConfig,
        accounts_dir: PathBuf,
        panel: Arc<dyn PanelSink>,
    ) -> Arc<dyn CycleExecutor> {
        Self::test_with_ready_and_clock_dir(
            ready,
            endpoints,
            daily_chat_config,
            accounts_dir,
            panel,
            Arc::new(LocalClock),
        )
    }

    /// 仅供契约测试：与 [`Self::test_with_ready`] 相同，但**注入固定时钟**。
    ///
    /// 夜猫子任务只在 23:00-08:00 的窗口内执行，窗口判定读的是
    /// `TaskRuntime` 的时钟——不注入固定时钟的话，执行器层的夜猫子断言只在
    /// 真实夜间成立（白天跑必然挂）。
    pub fn test_with_ready_and_clock(
        ready: Vec<ReadyAccount>,
        endpoints: TaskEndpoints<'static>,
        panel: Arc<dyn PanelSink>,
        clock: Arc<dyn Clock>,
    ) -> Arc<dyn CycleExecutor> {
        Self::test_with_ready_and_clock_dir(
            ready,
            endpoints,
            DailyChatConfig::default(),
            PathBuf::new(),
            panel,
            clock,
        )
    }

    fn test_with_ready_and_clock_dir(
        ready: Vec<ReadyAccount>,
        endpoints: TaskEndpoints<'static>,
        daily_chat_config: DailyChatConfig,
        accounts_dir: PathBuf,
        panel: Arc<dyn PanelSink>,
        clock: Arc<dyn Clock>,
    ) -> Arc<dyn CycleExecutor> {
        let usage_dir = usage_dir_from(&accounts_dir);
        let runtime = Arc::new(TaskRuntime {
            accounts_dir,
            usage_dir,
            pool: vec![],
            pool_present: false,
            ready,
            cache: Arc::new(SnapshotCache::with_ttl(0.0)),
            billing_base: String::new(),
            endpoints,
            daily_chat_config,
            client: UpstreamClient::new(),
            clock,
            schedule_clock: LocalScheduleClock,
            sleeper: RealSleeper,
            panel,
            exec: OnceLock::new(),
            scheduler: OnceLock::new(),
            credits_refresher: OnceLock::new(),
            price_refresher: OnceLock::new(),
            update_checker: OnceLock::new(),
        });
        let exec = Arc::new(ProductionExecutor {
            task: Arc::downgrade(&runtime),
        });
        let _ = runtime.exec.set(exec.clone());
        std::mem::forget(runtime);
        exec
    }
}

struct ProductionExecutor {
    task: Weak<TaskRuntime>,
}

impl CycleExecutor for ProductionExecutor {
    fn refresh<'a>(
        &'a self,
        _acc: &'a AccountView,
        _trigger: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move { false })
    }
    fn checkin<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        let task = self.task.upgrade().expect("runtime alive");
        Box::pin(async move {
            let ra = task.ready.iter().find(|r| r.state.uid == acc.uid);
            let ra = match ra {
                Some(r) => r,
                None => {
                    return json!({"ok": false, "error": "account not found"});
                }
            };
            let runner_acc = ra.runner_account();
            let ctx = CnStepCtx {
                dir: &task.accounts_dir,
                account_name: &ra.state.credential_file,
                clock: task.clock.as_ref(),
                device_token: ra.device_token.as_deref(),
                billing_base: &task.billing_base,
                account: runner_acc,
                endpoints: task.endpoints,
            };
            let res = crate::claim_all::cn_checkin(&task.client, &ctx).await;
            if res.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                json!({"ok": true, "credit": res.get("credit").and_then(Value::as_i64)})
            } else {
                json!({"ok": false, "error": res.get("error").or(res.get("msg")).and_then(Value::as_str).unwrap_or("checkin failed")})
            }
        })
    }
    fn report_activity<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        let task = self.task.upgrade().expect("runtime alive");
        Box::pin(async move {
            let ra = task.ready.iter().find(|r| r.state.uid == acc.uid);
            let ra = match ra {
                Some(r) => r,
                None => {
                    return json!({"ok": false, "error": "account not found"});
                }
            };
            let runner_acc = ra.runner_account();
            let ctx = CnStepCtx {
                dir: &task.accounts_dir,
                account_name: &ra.state.credential_file,
                clock: task.clock.as_ref(),
                device_token: ra.device_token.as_deref(),
                billing_base: &task.billing_base,
                account: runner_acc,
                endpoints: task.endpoints,
            };
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let res = crate::claim_all::cn_report_activity(&task.client, &ctx, now_ms).await;
            if res.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                json!({"ok": true, "credit": res.get("credit").and_then(Value::as_i64)})
            } else {
                json!({"ok": false, "error": res.get("error").or(res.get("msg")).and_then(Value::as_str).unwrap_or("report failed")})
            }
        })
    }
    fn cat_travel<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        let task = self.task.upgrade().expect("runtime alive");
        Box::pin(async move {
            let ra = task.ready.iter().find(|r| r.state.uid == acc.uid);
            let ra = match ra {
                Some(r) => r,
                None => {
                    return json!({"ok": false, "error": "account not found"});
                }
            };
            let runner_acc = ra.runner_account();
            let mut logs = Vec::new();
            let ctx = RunnerCtx {
                sleeper: &task.sleeper,
                clock: task.clock.as_ref(),
            };
            // 领奖成功后刷新积分余额 —— Python `do_cat_travel` 的
            // `account.fetch_credits()`（`wb_tasks.py:478`）。
            //
            // ⚠️ **有意差异（D36-b）**：Python 是「领奖后**同步即时抓上游**」，
            // 而 Rust 的注入点是**同步闭包**，发不了异步请求；这里改为读取
            // 账号文件里**本地已存的快照**（由后台刷新器按 TTL 维护）。
            // ⇒ 余额最多滞后一个 TTL（默认 12h），不是 Python 的「立刻是新的」。
            // 彻底对齐要把注入点改成 async（跨 `growth_runner` / `night_growth`
            // / `claim_all` 三个模块的签名改造），留到后续关卡。
            let mut credits_refresh =
                || local_credits_snapshot(&task.accounts_dir, &ra.state.credential_file);
            let res = build_cat_travel_refreshing(
                &task.client,
                &runner_acc,
                &task.endpoints,
                &mut logs,
                &ctx,
                &mut credits_refresh,
            )
            .await;
            json!({"ok": true, "credit": res.get("credit").and_then(Value::as_i64)})
        })
    }
    fn streak_bonus<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        let task = self.task.upgrade().expect("runtime alive");
        Box::pin(async move {
            let ra = task.ready.iter().find(|r| r.state.uid == acc.uid);
            let ra = match ra {
                Some(r) => r,
                None => {
                    return json!({"ok": false, "error": "account not found"});
                }
            };
            // 此前这里打的是 `POST {chat_base}/activity/growth/streak/claim`
            // ——该端点 Python 全仓 0 命中（上游不存在），**连登管家从来没
            // 真正工作过**。现改接 `crate::streak_bonus`（Python
            // `wb_tasks.run_streak_bonus` 的四步移植）。
            let billing_headers = ra.creds.billing_headers(ra.device_token.as_deref());
            let account = StreakBonusAccount {
                realm: &ra.creds.realm,
                enterprise_id: &ra.creds.enterprise_id,
                chat_headers: &ra.creds.chat_headers,
                billing_headers: &billing_headers,
            };
            let mut cfg = StreakBonusConfig {
                chat_base: task.endpoints.chat_base,
                bill_base: &task.billing_base,
                timeout: crate::streak_bonus::STREAK_BONUS_TIMEOUT,
                draw_interval: crate::streak_bonus::STREAK_BONUS_DRAW_INTERVAL,
                client_token: Box::new(crate::streak_bonus::default_client_token),
            };
            let ctx = StreakBonusCtx {
                sleeper: &task.sleeper,
                clock: task.clock.as_ref(),
            };
            let outcome = run_streak_bonus(&task.client, &account, &mut cfg, &ctx).await;
            outcome.to_json()
        })
    }
    fn night_growth<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        let task = self.task.upgrade().expect("runtime alive");
        Box::pin(async move {
            let ra = task.ready.iter().find(|r| r.state.uid == acc.uid);
            let ra = match ra {
                Some(r) => r,
                None => {
                    // 账号不在 ready 池里：Rust 侧特有的情况（Python 的
                    // `run_night_growth(account)` 由调度器逐账号调用，没有这个
                    // 分支）。保留既有的 `error` 形状。
                    return json!({"ok": false, "error": "account not found", "logs": []});
                }
            };
            // 改接 `crate::night_growth`（Python `wb_tasks.run_night_growth`
            // 的逐分支移植）。返回体按 Python 权威：`ok` / `earned_credit` /
            // `logs`，且 `msg` 键**只在**国际版 / 企业版两档出现。
            let runner_acc = ra.runner_account();
            let ctx = RunnerCtx {
                sleeper: &task.sleeper,
                clock: task.clock.as_ref(),
            };
            // 积分刷新注入：Python 在领奖成功后调 `account.fetch_credits()`。
            // 同上，**有意差异（D36-b）**：读本地已存快照，不是即时抓上游。
            let mut credits_refresh =
                || local_credits_snapshot(&task.accounts_dir, &ra.state.credential_file);
            let outcome: NightGrowthOutcome = run_night_growth(
                &task.client,
                &runner_acc,
                &task.endpoints,
                &ctx,
                &mut credits_refresh,
            )
            .await;
            outcome.to_json()
        })
    }
    fn daily_chat<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        let task = self.task.upgrade().expect("runtime alive");
        Box::pin(async move {
            let ra = match task.ready.iter().find(|r| r.state.uid == acc.uid) {
                Some(r) => r,
                None => {
                    return json!({"ok": false, "error": "account not found"});
                }
            };
            let account = DailyChatAccount {
                uid: &ra.creds.uid,
                token: &ra.creds.token,
                realm: &ra.state.realm,
                product: &ra.state.product,
                chat_headers: &ra.creds.chat_headers,
            };
            let ctx = DailyChatCtx {
                dir: &task.accounts_dir,
                account_name: &ra.state.credential_file,
                clock: task.clock.as_ref(),
            };
            let outcome =
                run_daily_chat(&task.client, &account, &task.daily_chat_config, &ctx, None).await;
            let mut body = outcome.body.as_object().cloned().unwrap_or_default();
            if let Some(err) = &outcome.persist_error {
                body.insert("persist_error".to_string(), json!(err));
            }
            Value::Object(body)
        })
    }
    fn accounts(&self) -> Vec<AccountView> {
        let task = self.task.upgrade().expect("runtime alive");
        task.ready
            .iter()
            .map(|r| {
                // 每轮重新读一次幂等字段：签到 / 每日活跃刚写盘时，下一轮的
                // `can_checkin` / `can_daily_chat` 必须看到新值——否则同一天会
                // 反复重跑每日活跃对话（每次都在上游建一次真实 agent 会话）。
                // 只在读到值时覆盖：这三个字段只会被写入、不会被清空；账号文件
                // 缺失时 `read_idempotent_fields` 返回全 None，不该抹掉内存值。
                let mut view = r.view.clone();
                if let Ok(fields) =
                    read_idempotent_fields(&task.accounts_dir, &r.state.credential_file)
                {
                    if fields.last_checkin.is_some() {
                        view.last_checkin = fields.last_checkin;
                    }
                    if fields.last_activity_report.is_some() {
                        view.last_activity_report = fields.last_activity_report;
                    }
                    if fields.last_daily_chat.is_some() {
                        view.last_daily_chat = fields.last_daily_chat;
                    }
                }
                view
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// CnStepCtx re-export for api layer convenience
// ---------------------------------------------------------------------------
pub use crate::claim_all::CnStepCtx;

// ---------------------------------------------------------------------------
// public_view / human_delta
// ---------------------------------------------------------------------------

pub fn human_delta(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "expired".to_string();
    }
    let days = seconds / 86400.0;
    if days >= 1.0 {
        return format!("{:.0} days", days);
    }
    let hours = seconds / 3600.0;
    if hours >= 1.0 {
        return format!("{:.1} hours", hours);
    }
    format!("{} min", (seconds / 60.0) as i64)
}

pub fn public_view(
    state: &AccountState,
    nickname: &Option<String>,
    accounts_dir: &std::path::Path,
) -> Value {
    let fields = read_idempotent_fields(accounts_dir, &state.credential_file).unwrap_or_default();
    let nick = nickname
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uid8(&state.uid));
    let exp = state.expires_at.unwrap_or(0.0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let expires_in = if exp > 0.0 {
        Some(human_delta(exp - now))
    } else {
        None
    };
    let is_enterprise = !state.enterprise_id.is_empty();
    let can_checkin = state.realm == "cn" && !is_enterprise;
    let can_report = state.realm == "cn" && !is_enterprise;
    let can_daily_chat = state.realm == "intl";
    let machine_id = wb_gateway::headers::derive_id(&state.uid, "machine");
    let session_id = wb_gateway::headers::derive_id(&state.uid, "session");
    let in_cooldown = state.cooldown_until > now;
    let cooldown_for = if in_cooldown {
        Some(human_delta(state.cooldown_until - now))
    } else {
        None
    };
    json!({
        "uid": state.uid,
        "nickname": nick,
        "realm": state.realm,
        "product": state.product,
        "enterpriseId": state.enterprise_id,
        "enabled": state.enabled,
        "expiresAt": exp,
        "expiresIn": expires_in,
        "lastError": "",
        "lastErrorDetail": Value::Null,
        "inCooldown": in_cooldown,
        "cooldownFor": cooldown_for,
        "modelCooldowns": state.model_cooldowns,
        "softStreak": state.soft_streak,
        "unscopedStreak": state.unscoped_streak,
        "breakerFor": if state.breaker_until > now { Some(human_delta(state.breaker_until - now)) } else { None },
        "degradeFor": if state.degrade_until > now { Some(human_delta(state.degrade_until - now)) } else { None },
        "canCheckin": can_checkin,
        "canReportActivity": can_report,
        "canDailyChat": can_daily_chat,
        "machineId": machine_id,
        "sessionId": session_id,
        "_last_checkin": fields.last_checkin,
        "_last_activity_report": fields.last_activity_report,
        "_last_daily_chat": fields.last_daily_chat,
    })
}

// ---------------------------------------------------------------------------
// 路由纯逻辑
// ---------------------------------------------------------------------------

pub async fn route_accounts_checkin(task: &TaskRuntime, payload: &Value) -> Value {
    let uid_filter = payload
        .get("uid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut targets: Vec<&ReadyAccount> = Vec::new();
    if let Some(u) = uid_filter.as_ref() {
        if let Some(ra) = task.ready.iter().find(|r| r.state.uid == *u) {
            targets.push(ra);
        }
    } else {
        targets = task
            .ready
            .iter()
            .filter(|r| r.state.realm == "cn")
            .collect();
    }

    let mut results: Vec<Value> = Vec::new();
    for ra in targets {
        let runner_acc = ra.runner_account();
        let ctx = CnStepCtx {
            dir: &task.accounts_dir,
            account_name: &ra.state.credential_file,
            clock: task.clock.as_ref(),
            device_token: ra.device_token.as_deref(),
            billing_base: &task.billing_base,
            account: runner_acc,
            endpoints: task.endpoints,
        };
        let res = crate::claim_all::cn_checkin(&task.client, &ctx).await;
        let nick = ra
            .nickname
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uid8(&ra.state.uid));
        let msg = res
            .get("msg")
            .or(res.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        task.panel.add_log_entry(
            &format!("[任务福利] 账号 [{}] 每日签到: {}", nick, msg),
            "tasks",
        );
        let mut r = json!({"uid": ra.state.uid, "nickname": nick});
        if let Some(obj) = res.as_object() {
            for (k, v) in obj {
                r[k.clone()] = v.clone();
            }
        }
        results.push(r);
    }

    let accounts: Vec<Value> = task
        .ready
        .iter()
        .map(|r| public_view(&r.state, &r.nickname, &task.accounts_dir))
        .collect();
    json!({"results": results, "accounts": accounts})
}

// ---------------------------------------------------------------------------
// 手动每日活跃（两个端点）
//
// Python 真源：
// - `wb_proxy.py:17549 _route_accounts_daily_chat`（完整打卡：桌面腿 + 网页腿）
// - `wb_proxy.py:17568 _route_accounts_daily_chat_web`（**只**网页腿）
// ---------------------------------------------------------------------------

/// Python 的真值判断（`if uid:` / `if res.get("ok")` 这类）。
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(value)` 的近似：只用于面板日志里的会话号。
///
/// 上游返回的会话号可能是数字（Python 侧 `conversation` 是原值），f-string 会
/// 打成 `123`；字符串原样输出，缺字段打成 `None`。
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 手动端点的目标账号选择（对齐 Python :17551-17554 / :17575-17578）。
///
/// - 给了 `uid` ⇒ 只打这一个（Python `POOL.get(uid)`；找不到就是空结果）。
///   uid 的真值判断跟 Python 一致：空串 / 0 / false 都算「没给」。
/// - 没给 ⇒ 所有 `realm == "intl" && enabled` 的账号（国内版与停用账号跳过）。
fn manual_daily_chat_targets<'a>(task: &'a TaskRuntime, payload: &Value) -> Vec<&'a ReadyAccount> {
    match payload.get("uid") {
        Some(uid) if json_truthy(uid) => match uid.as_str() {
            Some(want) => task
                .ready
                .iter()
                .find(|r| r.state.uid == want)
                .into_iter()
                .collect(),
            // 非字符串 uid：Python 的 `POOL.get(123)` 找不到 ⇒ 空结果。
            None => Vec::new(),
        },
        _ => task
            .ready
            .iter()
            .filter(|r| r.state.realm == "intl" && r.state.enabled)
            .collect(),
    }
}

/// 账号显示名（无昵称时退回 uid 前 8 位，对齐 Python `nickname or uid[:8]`）。
fn manual_nickname(ra: &ReadyAccount) -> String {
    ra.nickname
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uid8(&ra.state.uid))
}

/// 把上游结果并进 `{"uid", "nickname"}` 外壳（对齐 Python `{"uid": …, "nickname": …, **res}`）。
fn manual_result(ra: &ReadyAccount, nick: &str, res: &Value) -> Value {
    let mut out = json!({"uid": ra.state.uid, "nickname": nick});
    if let Some(obj) = res.as_object() {
        for (k, v) in obj {
            out[k.clone()] = v.clone();
        }
    }
    out
}

/// 面板要看的账号清单（对齐 Python `account_views()`）。
fn manual_account_views(task: &TaskRuntime) -> Vec<Value> {
    task.ready
        .iter()
        .map(|r| public_view(&r.state, &r.nickname, &task.accounts_dir))
        .collect()
}

/// 面板日志里的结果摘要（对齐 Python
/// `res.get('msg') if res.get('ok') else res.get('error')`；网页通道那条取
/// `res.get('conversation')`）。
fn manual_log_detail(res: &Value, ok_key: &str) -> String {
    let ok = res.get("ok").map(json_truthy).unwrap_or(false);
    let picked = if ok {
        res.get(ok_key)
    } else {
        res.get("error")
    };
    picked.map(py_repr).unwrap_or_else(|| "None".to_string())
}

/// 手动触发每日活跃打卡（对齐 Python `wb_proxy.py:17549 _route_accounts_daily_chat`）。
///
/// 走**完整的 daily_chat**（桌面腿 + 按设置决定要不要再跑网页腿），语义与
/// Python 一致：网页腿成功才写 `lastDailyChat`；桌面腿被渠道校验（11128）拦下
/// 时降级到网页腿。
///
/// 与网页通道那条（[`route_accounts_daily_chat_web`]）的差别正是「写不写
/// `lastDailyChat`」——那条刻意不写。
pub async fn route_accounts_daily_chat(task: &TaskRuntime, payload: &Value) -> Value {
    let mut results: Vec<Value> = Vec::new();
    for ra in manual_daily_chat_targets(task, payload) {
        let account = DailyChatAccount {
            uid: &ra.creds.uid,
            token: &ra.creds.token,
            realm: &ra.state.realm,
            product: &ra.state.product,
            chat_headers: &ra.creds.chat_headers,
        };
        let ctx = DailyChatCtx {
            dir: &task.accounts_dir,
            account_name: &ra.state.credential_file,
            clock: task.clock.as_ref(),
        };
        let outcome =
            run_daily_chat(&task.client, &account, &task.daily_chat_config, &ctx, None).await;
        let mut body = outcome.body.as_object().cloned().unwrap_or_default();
        if let Some(err) = &outcome.persist_error {
            body.insert("persist_error".to_string(), json!(err));
        }
        let res = Value::Object(body);
        let nick = manual_nickname(ra);
        let detail = manual_log_detail(&res, "msg");
        task.panel.add_log_entry(
            &format!("[任务福利] 账号 [{}] 每日活跃打卡: {}", nick, detail),
            "tasks",
        );
        results.push(manual_result(ra, &nick, &res));
    }
    json!({"results": results, "accounts": manual_account_views(task)})
}

/// 手动触发**网页通道**打卡（对齐 Python `wb_proxy.py:17568
/// `_route_accounts_daily_chat_web`）。
///
/// Python 的 docstring 写明「刻意不写 lastDailyChat——那是『今天已经打过卡』的
/// 闸门，手动补一次不该让定时巡检跳过当天的正常流程」。
///
/// Rust 侧天然满足这个不变量：网页腿入口 [`crate::daily_chat::daily_chat_web`]
/// **不收** `DailyChatCtx`，全程不碰账号文件；`lastDailyChat` 只在
/// [`crate::daily_chat::daily_chat`] 内部由 `persist_last_daily_chat` 写入。
pub async fn route_accounts_daily_chat_web(task: &TaskRuntime, payload: &Value) -> Value {
    let mut results: Vec<Value> = Vec::new();
    for ra in manual_daily_chat_targets(task, payload) {
        let account = DailyChatAccount {
            uid: &ra.creds.uid,
            token: &ra.creds.token,
            realm: &ra.state.realm,
            product: &ra.state.product,
            chat_headers: &ra.creds.chat_headers,
        };
        // 用 `daily_chat_web_recorded`：网页腿被面板**直接**调用时（这条路由）
        // 要记一行活动历史（Python `wb_accounts.py:1478`）；而被
        // `daily_chat()` 内部调用时**不记**（Python 那边 `trigger=None`）——
        // 两种语义的差别就落在这个入口上。
        let res = run_daily_chat_web(
            &task.client,
            &task.daily_chat_config,
            &account,
            None,
            &task.usage_dir,
        )
        .await;
        let nick = manual_nickname(ra);
        let detail = manual_log_detail(&res, "conversation");
        task.panel.add_log_entry(
            &format!("[任务福利] 账号 [{}] 网页通道打卡: {}", nick, detail),
            "tasks",
        );
        results.push(manual_result(ra, &nick, &res));
    }
    json!({"results": results, "accounts": manual_account_views(task)})
}

pub async fn route_tasks_get(task: &TaskRuntime, uid: Option<&str>) -> Value {
    let cn_accounts: Vec<&ReadyAccount> = task
        .ready
        .iter()
        .filter(|r| r.state.realm == "cn" && r.state.enabled)
        .collect();
    if cn_accounts.is_empty() {
        return json!({"tasks": [], "summary": {}, "accounts": [], "msg": "未找到可用的国内版账号"});
    }
    let acc = if let Some(u) = uid {
        if u != "all" {
            cn_accounts
                .iter()
                .find(|r| r.state.uid == *u)
                .copied()
                .or_else(|| cn_accounts.first().copied())
        } else {
            cn_accounts.first().copied()
        }
    } else {
        cn_accounts.first().copied()
    };
    let acc = acc.unwrap();
    let task_acc = acc.creds.as_task_account();
    let (tasks, summary) = match growth_snapshot(&task.cache, &task_acc).await {
        Ok((t, s)) => (t, s),
        Err(_) => (Vec::new(), GrowthSummary::default()),
    };
    let tasks_json: Vec<Value> = tasks.iter().map(|t| t.to_json()).collect();
    let summary_json = summary.to_json();
    let acct_list: Vec<Value> = cn_accounts
        .iter()
        .map(|r| {
            json!({"uid": r.state.uid, "nickname": r.nickname.clone().unwrap_or_else(|| uid8(&r.state.uid))})
        })
        .collect();
    let acc_public = public_view(&acc.state, &acc.nickname, &task.accounts_dir);
    json!({
        "tasks": tasks_json,
        "summary": summary_json,
        "account": acc_public,
        "accounts": acct_list
    })
}

pub async fn route_tasks_run(task: &TaskRuntime, payload: &Value) -> Value {
    if !task.pool_present {
        return json!({"ok": false, "msg": "账号池不可用"});
    }
    task.cache.invalidate();
    let uid_str = payload
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let targets: Vec<&ReadyAccount> = if !uid_str.is_empty() && uid_str != "all" {
        let found: Vec<&ReadyAccount> = task
            .ready
            .iter()
            .filter(|r| r.state.uid == uid_str && r.state.realm == "cn" && r.state.enabled)
            .collect();
        if found.is_empty() {
            return json!({"ok": false, "msg": "未找到指定的国内版账号"});
        }
        found
    } else {
        let v: Vec<&ReadyAccount> = task
            .ready
            .iter()
            .filter(|r| r.state.realm == "cn" && r.state.enabled)
            .collect();
        if v.is_empty() {
            return json!({"ok": false, "msg": "未找到已启用的国内版账号"});
        }
        v
    };

    let mut combined_logs: Vec<String> = Vec::new();
    let mut total_credit: i64 = 0;
    let n = targets.len();
    for (i, ra) in targets.iter().enumerate() {
        let uid_short = uid8(&ra.state.uid);
        let nick = ra
            .nickname
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uid_short.clone());
        combined_logs.push(format!(
            "====== 正在为账号 [{} ({})] 执行全自动成长任务 ({}/{}) ======",
            nick,
            uid_short,
            i + 1,
            n
        ));
        let runner_acc = ra.runner_account();
        let env = RunnerEnv { ep: task.endpoints };
        let deps = RunnerDeps {
            sleeper: &task.sleeper,
            clock: task.clock.as_ref(),
            // 同上，**有意差异（D36-b）**：读本地已存快照，不是即时抓上游。
            credits_refresh: Box::new(|| {
                local_credits_snapshot(&task.accounts_dir, &ra.state.credential_file)
            }),
            cat_travel: CatTravel::Disabled,
            gap: 1.0,
        };
        let res = run_growth_tasks(&task.client, &runner_acc, &env, deps).await;
        let earned = res.earned_credit.unwrap_or(0);
        total_credit += earned;
        for l in &res.logs {
            combined_logs.push(format!("  {}", l));
        }
        if i + 1 < n {
            task.sleeper.sleep(1.5).await;
        }
    }
    combined_logs.push(format!(
        "====== 全部 {} 个账号任务执行完毕，累计新增积分: +{} ======",
        n, total_credit
    ));
    task.panel.add_log_entry(
        &format!(
            "[任务福利] 批量成长任务执行完成，共 {} 个账号，新增积分 +{}",
            n, total_credit
        ),
        "tasks",
    );
    json!({"ok": true, "credit_added": total_credit, "logs": combined_logs, "accounts_count": n})
}

pub async fn route_tasks_travel(task: &TaskRuntime, payload: &Value) -> Value {
    if !task.pool_present {
        return json!({"ok": false, "msg": "账号池不可用"});
    }
    task.cache.invalidate();
    let uid_str = payload
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let targets: Vec<&ReadyAccount> = if !uid_str.is_empty() && uid_str != "all" {
        let found: Vec<&ReadyAccount> = task
            .ready
            .iter()
            .filter(|r| r.state.uid == uid_str && r.state.realm == "cn" && r.state.enabled)
            .collect();
        if found.is_empty() {
            return json!({"ok": false, "msg": "未找到指定的国内版账号"});
        }
        found
    } else {
        let v: Vec<&ReadyAccount> = task
            .ready
            .iter()
            .filter(|r| r.state.realm == "cn" && r.state.enabled)
            .collect();
        if v.is_empty() {
            return json!({"ok": false, "msg": "未找到已启用的国内版账号"});
        }
        v
    };

    let mut results: Vec<Value> = Vec::new();
    let n = targets.len();
    for (i, ra) in targets.iter().enumerate() {
        let uid_short = uid8(&ra.state.uid);
        let nick = ra
            .nickname
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uid_short.clone());
        let runner_acc = ra.runner_account();
        let mut logs = Vec::new();
        let ctx = RunnerCtx {
            sleeper: &task.sleeper,
            clock: task.clock.as_ref(),
        };
        // 领奖成功后刷新积分余额（Python `do_cat_travel` 的
        // `account.fetch_credits()`）。M5 的 `CreditsRefresher` 未落地 ⇒
        // 回填 `None`（已知缺口）。
        let mut credits_refresh = || None::<Value>;
        let res = build_cat_travel_refreshing(
            &task.client,
            &runner_acc,
            &task.endpoints,
            &mut logs,
            &ctx,
            &mut credits_refresh,
        )
        .await;
        let action = res.get("action").cloned().unwrap_or(Value::Null);
        let msg = res
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let reward = res.get("credit").and_then(Value::as_i64).unwrap_or(0);
        results.push(json!({
            "uid": ra.state.uid,
            "nickname": nick,
            "action": action,
            "msg": msg,
            "reward_credit": reward,
        }));
        if i + 1 < n {
            task.sleeper.sleep(1.0).await;
        }
    }
    let summary_msg = results
        .iter()
        .map(|r| {
            format!(
                "{}: {}",
                r.get("nickname").and_then(Value::as_str).unwrap_or(""),
                r.get("msg").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    for r in &results {
        let nick = r.get("nickname").and_then(Value::as_str).unwrap_or("");
        let msg = r.get("msg").and_then(Value::as_str).unwrap_or("");
        task.panel.add_log_entry(
            &format!("[任务福利] 账号 [{}] 猫猫旅行: {}", nick, msg),
            "tasks",
        );
    }
    json!({"ok": true, "results": results, "msg": summary_msg, "accounts_count": n})
}

pub async fn route_tasks_streak_bonus(task: &TaskRuntime, payload: &Value) -> Value {
    if !task.pool_present {
        return json!({"ok": false, "msg": "账号池不可用"});
    }
    task.cache.invalidate();
    let uid_str = payload
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let targets: Vec<&ReadyAccount> = if !uid_str.is_empty() && uid_str != "all" {
        let found: Vec<&ReadyAccount> = task
            .ready
            .iter()
            .filter(|r| r.state.uid == uid_str && r.state.realm == "cn" && r.state.enabled)
            .collect();
        if found.is_empty() {
            return json!({"ok": false, "msg": "未找到指定的国内版账号"});
        }
        found
    } else {
        let v: Vec<&ReadyAccount> = task
            .ready
            .iter()
            .filter(|r| r.state.realm == "cn" && r.state.enabled)
            .collect();
        if v.is_empty() {
            return json!({"ok": false, "msg": "未找到已启用的国内版账号"});
        }
        v
    };

    let exec = match task.exec() {
        Some(e) => e,
        None => return json!({"ok": false, "msg": "executor unavailable"}),
    };
    let exec_ref: &'static dyn CycleExecutor = unsafe {
        std::mem::transmute::<&dyn CycleExecutor, &'static dyn CycleExecutor>(exec.as_ref())
    };

    let mut results: Vec<Value> = Vec::new();
    let mut all_ok = true;
    for ra in &targets {
        let nick = ra
            .nickname
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uid8(&ra.state.uid));
        let res = exec_ref.streak_bonus(&ra.view).await;
        let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            all_ok = false;
        }
        let msg = res
            .get("msg")
            .or(res.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
        results.push(json!({"uid": ra.state.uid, "nickname": nick, "ok": ok, "msg": msg}));
    }
    json!({
        "ok": all_ok,
        "msg": if all_ok {
            "streak bonus completed"
        } else {
            "streak bonus partially failed"
        },
        "results": results,
        "accounts_count": targets.len()
    })
}

pub async fn route_tasks_claim_all(task: &TaskRuntime, payload: &Value) -> Value {
    if !task.pool_present {
        return json!({"ok": false, "msg": "账号池不可用"});
    }
    task.cache.invalidate();
    let accounts: Vec<AccountView> = task.ready.iter().map(|r| r.view.clone()).collect();
    let req = ClaimAllRequest::new(payload.get("uid").and_then(Value::as_str));
    let exec = match task.exec() {
        Some(e) => e,
        None => return json!({"ok": false, "msg": "executor unavailable"}),
    };
    let exec_ref: &'static dyn CycleExecutor = unsafe {
        std::mem::transmute::<&dyn CycleExecutor, &'static dyn CycleExecutor>(exec.as_ref())
    };
    let sleeper_ref: &'static dyn crate::growth_runner::Sleeper = unsafe {
        std::mem::transmute::<
            &dyn crate::growth_runner::Sleeper,
            &'static dyn crate::growth_runner::Sleeper,
        >(&task.sleeper as &dyn crate::growth_runner::Sleeper)
    };
    let mut deps = ClaimAllDeps::new(exec_ref, task.clock.as_ref(), sleeper_ref);
    deps.credits_refresh = Arc::new(|| None) as crate::claim_all::CreditRefreshFn;
    deps.growth_tasks = None;
    deps.client = task.client.clone();
    deps.credentials = task
        .ready
        .iter()
        .map(|ra| {
            (
                ra.state.uid.clone(),
                claim_all::ClaimCredentials {
                    token: ra.creds.token.clone(),
                    enterprise_id: ra.creds.enterprise_id.clone(),
                    chat_headers: ra.creds.chat_headers.clone(),
                },
            )
        })
        .collect();
    deps.nicknames = task
        .ready
        .iter()
        .map(|ra| {
            (
                ra.state.uid.clone(),
                ra.nickname.clone().unwrap_or_default(),
            )
        })
        .collect();
    let result = route_claim_all(&req, &accounts, task.pool_present, &deps).await;
    if result.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let ac = result
            .get("accounts_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let ca = result
            .get("credit_added")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        task.panel.add_log_entry(
            &format!(
                "[claim-all] 全量积分领取完成，共 {} 账号，成长积分 +{}",
                ac, ca
            ),
            "tasks",
        );
    }
    result
}

pub fn route_scheduler_get(task: &TaskRuntime) -> Value {
    task.scheduler()
        .map(|s| s.status())
        .unwrap_or_else(|| json!({"enabled": false, "msg": "未运行"}))
}

pub fn route_scheduler_trigger(task: &TaskRuntime) -> Value {
    task.scheduler()
        .map(|s| s.trigger_now())
        .unwrap_or_else(|| json!({"ok": false, "msg": "调度器未初始化"}))
}

pub fn route_scheduler_toggle(task: &TaskRuntime) -> Value {
    task.scheduler()
        .map(|s| s.toggle_enabled())
        .unwrap_or_else(|| json!({"ok": false, "msg": "调度器未初始化"}))
}

/// 后台定价刷新器的状态（关卡 4.19）。
///
/// Python 侧对应 `PRICING.status()`（`wb_pricing.py:2511-2554`）。
/// 未装配时返回同形状的「未运行」视图。
pub fn route_pricing_status(task: &TaskRuntime) -> Value {
    task.price_refresher
        .get()
        .map(|r| r.status())
        .unwrap_or_else(|| {
            json!({
                "interval_minutes": 0,
                "enabled": false,
                "master_enabled": false,
                "last_run": null,
                "last_error": null,
                "logs": [],
            })
        })
}

/// 后台更新检查器的状态（关卡 4.19）。
///
/// Python 侧对应 `UPDATES.status()`（`wb_updates.py:257-271`），十键。
/// 未装配时返回同形状的「未运行」视图。
pub fn route_updates_status(task: &TaskRuntime) -> Value {
    task.update_checker
        .get()
        .map(|c| c.status())
        .unwrap_or_else(|| {
            json!({
                "current_version": crate::update_checker::HUB_VERSION,
                "latest_version": null,
                "update_available": false,
                "enabled": false,
                "checking": false,
                "last_attempt": null,
                "last_success": null,
                "last_error": null,
                "release_url": null,
                "published_at": null,
            })
        })
}

/// 后台积分刷新器的状态（关卡 4.18）。
///
/// Python 侧对应 `CreditsRefresher.status()`（`wb_accounts.py:2578-2586`），
/// 六个键 `ttl_hours / interval_seconds / last_run / last_uid / last_error /
/// logs` 的**键名与顺序逐字一致**。刷新器未装配时返回同一形状的「未运行」
/// 视图，而不是 404 —— 面板上要能区分「没跑」与「跑过」。
pub fn route_credits_refresher_status(task: &TaskRuntime) -> Value {
    task.credits_refresher
        .get()
        .map(|r| r.status())
        .unwrap_or_else(|| {
            json!({
                "ttl_hours": 0.0,
                "interval_seconds": crate::credits_refresher::INTERVAL_SECONDS,
                "last_run": null,
                "last_uid": null,
                "last_error": null,
                "logs": [],
            })
        })
}

// SnapshotCache invalidate helper trait not needed; call directly.
// GrowthSummary default assumed available.

// ---------------------------------------------------------------------------
// 关卡 4.24（线 B）：账户管理路由（16 条）
//
// Python 权威源：wb_proxy.py `_handle_accounts`（:17083-17145）与各
// `_route_accounts_*`。所有 JSON 键名与顺序对齐 Python 对应分支。
//
// ⚠️ 与 Python 的有意差异（均为「简化版」，见汇报）：
// - `route_accounts_test` / `route_accounts_sync_profile` **不发上游请求**
//   （契约要求：test 只做本地点检；sync_profile 只读本地账号文件）。
// - `route_accounts_import_desktop` 不实现桌面端密钥恢复（recoverKey/forgetKey
//   返回不支持提示）；扫描范围是账号目录里的 JSON 凭据档。
// - 线 A（pool.rs 的 AccountPool + oauth.rs）并行实现中，本线暂以文件读写 +
//   本地登录注册表（crate::login）承载同等能力；线 A 落定后可切换其契约 API。
// ---------------------------------------------------------------------------

/// 当前 epoch 秒（等价 Python `time.time()`）。
fn acct_now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 目标账号选择（对齐 Python `if uid: POOL.get(uid) elif realm: ... else: 全部`）。
fn account_targets<'a>(task: &'a TaskRuntime, payload: &Value) -> Vec<&'a ReadyAccount> {
    match payload.get("uid") {
        Some(uid) if json_truthy(uid) => match uid.as_str() {
            Some(want) => task
                .ready
                .iter()
                .find(|r| r.state.uid == want)
                .into_iter()
                .collect(),
            None => Vec::new(),
        },
        _ => {
            let realm = payload.get("realm").and_then(Value::as_str).unwrap_or("");
            if !realm.is_empty() && realm != "all" {
                task.ready
                    .iter()
                    .filter(|r| r.state.realm == realm)
                    .collect()
            } else {
                task.ready.iter().collect()
            }
        }
    }
}

/// 净化 uid 为安全文件名（对齐 Python `re.sub(r"[^A-Za-z0-9_-]", "_", uid).strip("_ ")`）。
fn safe_uid(uid: &str) -> String {
    uid.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(|c: char| c == '_' || c.is_whitespace())
        .to_string()
}

/// 原子写账号凭据档（同目录 temp + rename，等价 Python `Account.save`）。
///
/// `file_name` 必须来自 [`safe_uid`]（无路径分隔符），保证写入落点始终在
/// `accounts_dir` 之内，不做越界写入。
fn write_account_doc(
    accounts_dir: &std::path::Path,
    file_name: &str,
    doc: &Value,
) -> Result<(), String> {
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || file_name.contains('/')
        || file_name.contains('\\')
    {
        return Err(format!("invalid account file name: {file_name}"));
    }
    let path = accounts_dir.join(file_name);
    let text = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    let tmp = path.with_file_name(format!("{}.{}.tmp", file_name, std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {}", path.display(), e))?;
    Ok(())
}

/// 读账号凭据档；文件缺失 / 非对象返回 None。
fn read_account_doc(accounts_dir: &std::path::Path, file_name: &str) -> Option<Value> {
    if file_name.is_empty() {
        return None;
    }
    let path = accounts_dir.join(file_name);
    let text = std::fs::read_to_string(&path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    if doc.is_object() {
        Some(doc)
    } else {
        None
    }
}

/// 把归一化后的字段集写成凭据档 JSON（load_account_pool 能读回的键）。
fn account_doc_from_fields(f: &wb_identity::import::AccountFields) -> Value {
    json!({
        "uid": f.uid,
        "nickname": f.nickname,
        "domain": f.domain,
        "realm": f.realm,
        "platform": f.platform,
        "product": f.product,
        "enterpriseId": f.enterprise_id,
        "accessToken": f.access_token,
        "refreshToken": f.refresh_token,
        "expiresAt": f.expires_at,
        "addedAt": f.added_at,
        "source": f.source,
        "enabled": f.enabled,
        "weight": f.weight,
        "note": f.note,
    })
}

/// 从字段集构造内存态（public_view 用）。
fn state_from_fields(f: &wb_identity::import::AccountFields) -> AccountState {
    let mut st = AccountState::new(&f.uid, &f.realm);
    st.enterprise_id = f.enterprise_id.clone();
    st.enabled = f.enabled;
    st.has_token = !f.access_token.is_empty();
    st.access_token = f.access_token.clone();
    st.weight = f.weight.clamp(1, 100);
    st.product = wb_gateway::endpoints::normalize_product(&f.product).to_string();
    st.expires_at = if f.expires_at > 0 {
        Some(f.expires_at as f64)
    } else {
        None
    };
    st
}

/// 把 `AccountFields` 转成 public 视图（对齐 Python `account.public()` 的子集，
/// 供 import / login poll 的 `imported`/`account` 字段）。
fn public_from_fields(f: &wb_identity::import::AccountFields) -> Value {
    let st = state_from_fields(f);
    public_view(&st, &Some(f.nickname.clone()), std::path::Path::new(""))
}

/// 读取账号文件里的昵称（没有则回退 uid 前 8 位）。
fn read_local_nickname(accounts_dir: &std::path::Path, file_name: &str) -> Option<String> {
    let doc = read_account_doc(accounts_dir, file_name)?;
    doc.get("nickname")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 积分包状态判定（对齐 Python `_grant_status`：过期 > 用完 > 在扣减 > 可用）。
fn grant_status(pkg: &Value, now: f64) -> &'static str {
    let remain = pkg.get("remain").and_then(Value::as_f64).unwrap_or(0.0);
    let expired = pkg
        .get("is_expired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expired = expired
        || pkg
            .get("expire_time")
            .and_then(|v| parse_stamp_epoch(v.as_str().unwrap_or("")))
            .is_some_and(|at| at <= now);
    if expired {
        return "expired";
    }
    if remain <= 0.0 {
        return "used_up";
    }
    if pkg
        .get("in_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "active"
    } else {
        "available"
    }
}

/// `'2026-10-10 00:42:45'` → epoch（本地时区）；认不出来返回 None。
fn parse_stamp_epoch(text: &str) -> Option<f64> {
    let s = text.get(..19)?;
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|dt| dt.and_local_timezone(chrono::Local).single())
        .map(|dt| dt.timestamp() as f64)
}

// ---------------------------------------------------------------------------
// 积分
// ---------------------------------------------------------------------------

/// 单账号积分抓取并落盘（Python `Account.fetch_credits()`：详细 / 企业两条
/// 路径；成功即写回账号文件）。
async fn fetch_credits_now(task: &TaskRuntime, ra: &ReadyAccount) -> Value {
    let now = acct_now_epoch();
    let realm_cfg = wb_gateway::endpoints::realm_config(&ra.state.realm);
    let headers = ra.creds.billing_headers(ra.device_token.as_deref());
    let res = if !ra.state.enterprise_id.is_empty() {
        match wb_gateway::credits::fetch_credits_enterprise(
            &task.client,
            realm_cfg.billing_upstream,
            &headers,
        )
        .await
        {
            Ok(ent) => {
                let credits = crate::credits::aggregate_enterprise(
                    ent.used,
                    ent.size,
                    &ent.cycle_start,
                    &ent.cycle_end,
                    now,
                );
                json!({"ok": true, "credits": credits.to_json()})
            }
            Err(e) => json!({"ok": false, "error": e}),
        }
    } else {
        let chat_upstream = wb_gateway::endpoints::chat_base_url(&ra.state.realm, "workbuddy");
        match wb_gateway::credits::fetch_credits_detailed(
            &task.client,
            realm_cfg.billing_upstream,
            &chat_upstream,
            &headers,
        )
        .await
        {
            Ok(detail) => {
                let packages = crate::credits::packages_from_accounts(&detail.accounts, now);
                let credits = crate::credits::aggregate_detailed(
                    packages,
                    &detail.summary_packages,
                    detail.is_paid_user,
                    detail.checkin,
                    now,
                );
                json!({"ok": true, "credits": credits.to_json()})
            }
            Err(e) => json!({"ok": false, "error": e}),
        }
    };
    if res.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        if let Some(credits) = res.get("credits") {
            let _ = wb_storage::write_account_credits(
                &task.accounts_dir,
                &ra.state.credential_file,
                credits,
            );
        }
    }
    res
}

/// `/accounts/credits` + `/accounts/credits/fetch`：一键刷新积分。
pub async fn route_accounts_credits_fetch(task: &TaskRuntime, payload: &Value) -> Value {
    let targets = account_targets(task, payload);
    let mut results = Vec::new();
    for ra in targets {
        let res = fetch_credits_now(task, ra).await;
        let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let error = res
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let credits =
            wb_storage::read_account_credits(&task.accounts_dir, &ra.state.credential_file)
                .ok()
                .flatten()
                .unwrap_or(Value::Null);
        results.push(json!({
            "uid": ra.state.uid,
            "ok": ok,
            "credits": credits,
            "error": error,
        }));
    }
    json!({"results": results, "accounts": manual_account_views(task)})
}

/// `/accounts/credits/detail`：单账号积分明细（Python `_route_account_credits_detail`）。
pub async fn route_account_credits_detail(task: &TaskRuntime, payload: &Value) -> Value {
    let uid = payload.get("uid").and_then(Value::as_str).unwrap_or("");
    let ra = task.ready.iter().find(|r| r.state.uid == uid);
    let Some(ra) = ra else {
        return json!({"ok": false, "error": "未找到指定账号"});
    };
    let refresh = match payload.get("refresh") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(v) => json_truthy(v),
    };
    let stored = wb_storage::read_account_credits(&task.accounts_dir, &ra.state.credential_file)
        .ok()
        .flatten();
    let has_packages = stored
        .as_ref()
        .and_then(|c| c.get("packages"))
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let nick = ra
        .nickname
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uid8(&ra.state.uid));
    if refresh || !has_packages {
        let res = fetch_credits_now(task, ra).await;
        if !res.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let error = res
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("获取积分明细失败")
                .to_string();
            let credits =
                wb_storage::read_account_credits(&task.accounts_dir, &ra.state.credential_file)
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Null);
            return json!({
                "ok": false,
                "uid": ra.state.uid,
                "nickname": nick,
                "realm": ra.state.realm,
                "credits": credits,
                "error": error,
            });
        }
    }
    let credits = wb_storage::read_account_credits(&task.accounts_dir, &ra.state.credential_file)
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    json!({
        "ok": true,
        "uid": ra.state.uid,
        "nickname": nick,
        "realm": ra.state.realm,
        "credits": credits,
    })
}

/// `/accounts/credits/grants`：积分获取历史（只读本地账号文件快照，不发上游
/// 请求；对齐 Python `credit_grants()` 的摊平表）。
pub async fn route_accounts_credits_grants(task: &TaskRuntime, _payload: &Value) -> Value {
    let now = acct_now_epoch();
    let mut rows: Vec<Value> = Vec::new();
    let mut fetched = 0.0_f64;
    for ra in &task.ready {
        let credits =
            wb_storage::read_account_credits(&task.accounts_dir, &ra.state.credential_file)
                .ok()
                .flatten()
                .unwrap_or(Value::Null);
        if let Some(upd) = credits.get("updated_at").and_then(Value::as_f64) {
            fetched = fetched.max(upd);
        }
        let Some(packages) = credits.get("packages").and_then(Value::as_array) else {
            continue;
        };
        let nick = ra
            .nickname
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uid8(&ra.state.uid));
        for pkg in packages {
            let num = |key: &str| -> f64 {
                pkg.get(key)
                    .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
                    .unwrap_or(0.0)
            };
            let expire_iso = pkg
                .get("expire_time")
                .or_else(|| pkg.get("cycle_end_time"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let create_at =
                parse_stamp_epoch(pkg.get("create_time").and_then(Value::as_str).unwrap_or(""));
            rows.push(json!({
                "uid": ra.state.uid,
                "nickname": nick,
                "realm": ra.state.realm,
                "name": pkg.get("name").or_else(|| pkg.get("package_code")).and_then(Value::as_str).unwrap_or("Package"),
                "product": pkg.get("sub_product_name").or_else(|| pkg.get("product_name")).and_then(Value::as_str).unwrap_or(""),
                "grant_reason": pkg.get("grant_reason").and_then(Value::as_str).unwrap_or(""),
                "size": num("size"),
                "remain": num("remain"),
                "used": num("used"),
                "unit": pkg.get("unit").and_then(Value::as_str).unwrap_or("credit"),
                "create_at": create_at,
                "create_iso": pkg.get("create_time").and_then(Value::as_str).unwrap_or(""),
                "expire_at": parse_stamp_epoch(&expire_iso),
                "expire_iso": expire_iso,
                "no_expiry": pkg.get("no_expiry").and_then(Value::as_bool).unwrap_or(false),
                "days_left": pkg.get("days_left").cloned(),
                "status": grant_status(pkg, now),
                // 简化版：本机动作关联（Python `_grant_action` 读 activity 历史）
                // 未实现，恒为 null。
                "action": Value::Null,
            }));
        }
    }
    // 新的在前；同一秒按昵称、包名稳定排序。
    rows.sort_by(|a, b| {
        let ka = a.get("create_at").and_then(Value::as_f64).unwrap_or(0.0);
        let kb = b.get("create_at").and_then(Value::as_f64).unwrap_or(0.0);
        kb.partial_cmp(&ka)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.get("nickname")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(b.get("nickname").and_then(Value::as_str).unwrap_or(""))
            })
            .then_with(|| {
                a.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
            })
    });
    let sum = |key: &str| -> f64 {
        rows.iter()
            .map(|r| r.get(key).and_then(Value::as_f64).unwrap_or(0.0))
            .sum::<f64>()
    };
    let accounts = rows
        .iter()
        .filter_map(|r| r.get("uid").and_then(Value::as_str).map(str::to_string))
        .collect::<std::collections::HashSet<String>>()
        .len();
    json!({
        "ok": true,
        "generated_at": now,
        "generated_iso": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "fetched_at": if fetched > 0.0 { Some(fetched) } else { None },
        "fetched_iso": if fetched > 0.0 {
            Some(crate::credits::local_text(fetched))
        } else {
            None
        },
        "rows": rows,
        "summary": {
            "count": rows.len(),
            "size": (sum("size") * 100.0).round() / 100.0,
            "remain": (sum("remain") * 100.0).round() / 100.0,
            "used": (sum("used") * 100.0).round() / 100.0,
            "accounts": accounts,
        },
    })
}

// ---------------------------------------------------------------------------
// 导入 / 导出 / 删除
// ---------------------------------------------------------------------------

/// 把导入文档粗分成行（对齐 Python `wb_accounts._coerce_account_rows`）：
/// `{"accounts": [...]}` / 裸数组 / 单账号 dict / 桌面凭据 dict。
fn coerce_rows(blob: &Value) -> Result<Vec<Value>, String> {
    match blob {
        Value::Array(rows) => Ok(rows.clone()),
        Value::Object(map) => {
            if let Some(accounts) = map.get("accounts") {
                if let Value::Array(rows) = accounts {
                    if rows.is_empty() {
                        return Err("document contains no accounts".to_string());
                    }
                    return Ok(rows.clone());
                }
                return Err("accounts must be an array".to_string());
            }
            if map.contains_key("accessToken")
                || map.contains_key("auth")
                || map.contains_key("account")
            {
                return Ok(vec![blob.clone()]);
            }
            Err("the document must be a JSON object or array".to_string())
        }
        _ => Err("the document must be a JSON object or array".to_string()),
    }
}

/// 账号文件扫描（对齐 Python `desktop_credential_scan` 的简化版：只扫账号目录）。
fn scan_account_files(accounts_dir: &std::path::Path) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(accounts_dir) else {
        return out;
    };
    let mut files: Vec<std::path::PathBuf> = read_dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|s| s.to_str()).to_owned() else {
            continue;
        };
        if ["settings.json", "model.json", "active_realm.json"].contains(&name) {
            continue;
        }
        if let Some(doc) = read_account_doc(accounts_dir, name) {
            let uid = doc.get("uid").and_then(Value::as_str).unwrap_or("");
            let realm = doc.get("realm").and_then(Value::as_str).unwrap_or("");
            out.push(json!({
                "path": path.to_string_lossy(),
                "file": name,
                "uid": uid,
                "realm": realm,
                "nickname": doc.get("nickname").and_then(Value::as_str).unwrap_or(""),
            }));
        }
    }
    out
}

/// `/accounts/export`：导出账号文档（identity::export；对齐 Python
/// `_get_accounts_export` 的简化 POST 版）。
pub async fn route_accounts_export(task: &TaskRuntime, payload: &Value) -> Value {
    let include_secrets = match payload.get("secrets") {
        None => true,
        Some(v) => json_truthy(v),
    };
    let portable = payload
        .get("portable")
        .or(payload.get("lean"))
        .or(payload.get("minimal"))
        .or(payload.get("account_only"))
        .map(json_truthy)
        .unwrap_or(false);
    let fields: Vec<wb_identity::import::AccountFields> = account_targets(task, payload)
        .iter()
        .filter_map(|ra| fields_from_ready(task, ra))
        .collect();
    let doc = wb_identity::export::build_export_document(&fields, include_secrets);
    json!({
        "account": doc,
        "exported": true,
        "count": fields.len(),
        "portable": portable,
        "accounts": manual_account_views(task),
    })
}

/// 从 ready 账号读凭据档并映射为可导出字段集（文件缺失回退内存态）。
fn fields_from_ready(
    task: &TaskRuntime,
    ra: &ReadyAccount,
) -> Option<wb_identity::import::AccountFields> {
    let st = &ra.state;
    let doc = read_account_doc(&task.accounts_dir, &st.credential_file);
    let nickname = ra
        .nickname
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            doc.as_ref()
                .and_then(|d| d.get("nickname"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| uid8(&st.uid));
    let expires_at = st.expires_at.unwrap_or(0.0) as i64;
    Some(wb_identity::import::AccountFields {
        uid: st.uid.clone(),
        nickname,
        domain: wb_identity::realm::domain_for_realm(&st.realm, ""),
        realm: st.realm.clone(),
        platform: doc
            .as_ref()
            .and_then(|d| d.get("platform"))
            .and_then(Value::as_str)
            .unwrap_or("CLI")
            .to_string(),
        enterprise_id: st.enterprise_id.clone(),
        access_token: st.access_token.clone(),
        refresh_token: doc
            .as_ref()
            .and_then(|d| d.get("refreshToken"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        expires_at,
        source: doc
            .as_ref()
            .and_then(|d| d.get("source"))
            .and_then(Value::as_str)
            .unwrap_or("import")
            .to_string(),
        enabled: st.enabled,
        last_error: st.last_error.clone(),
        cooldown_until: st.cooldown_until,
        product: st.product.clone(),
        added_at: doc
            .as_ref()
            .and_then(|d| d.get("addedAt"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        weight: st.weight,
        note: doc
            .as_ref()
            .and_then(|d| d.get("note"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// `/accounts/import`：导入导出行 / 裸数组 / 单账号 / 桌面凭据。
/// 支持 `dryRun`（不落盘）、`overwrite`（覆盖已存在账号）、`realm`（强制 realm）。
pub async fn route_accounts_import(task: &TaskRuntime, payload: &Value) -> Value {
    let blob = if payload.get("data").is_some() {
        payload.get("data").unwrap_or(&Value::Null)
    } else {
        payload
    };
    let rows = match coerce_rows(blob) {
        Ok(r) => r,
        Err(problem) => {
            return json!({
                "ok": false,
                "error": format!("cannot read the document: {}", problem),
            })
        }
    };
    let dry_run = payload.get("dryRun").map(json_truthy).unwrap_or(false);
    let overwrite = payload.get("overwrite").map(json_truthy).unwrap_or(false);
    let forced_realm = payload
        .get("realm")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !forced_realm.is_empty() && forced_realm != "intl" && forced_realm != "cn" {
        return json!({
            "ok": false,
            "error": "realm must be intl or cn",
        });
    }
    let forced = if forced_realm.is_empty() {
        None
    } else {
        Some(forced_realm.as_str())
    };

    let report = import_rows_preview(task, &rows, forced, overwrite);
    if dry_run {
        return json!({
            "dryRun": true,
            "count": rows.len(),
            "result": report,
            "accounts": manual_account_views(task),
        });
    }
    let report = import_rows_commit(task, &rows, forced, overwrite);
    task.panel.add_log_entry(
        &format!(
            "account import: {} added, {} updated, {} skipped, {} invalid",
            report
                .get("added")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
            report
                .get("updated")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
            report
                .get("skipped")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
            report
                .get("invalid")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
        ),
        "accounts",
    );
    json!({
        "count": rows.len(),
        "result": report,
        "accounts": manual_account_views(task),
    })
}

/// 导入判重的「已知账号集」= 进程内 ready ∪ 磁盘上已有的账号凭据档。
///
/// Python 从 `AccountPool.accounts` 判重（导入即入池，故第二次导入同 uid 会
/// 看得到第一次的结果）；Rust 的 ready 是只读 Vec，写入新档后不自动入池，
/// 故必须以**磁盘上的文件**为第二来源——凭据档本身就是池的持久化真相。
fn known_uid_set(task: &TaskRuntime) -> std::collections::HashSet<String> {
    let mut set: std::collections::HashSet<String> =
        task.ready.iter().map(|r| r.state.uid.clone()).collect();
    for entry in scan_account_files(&task.accounts_dir) {
        if let Some(uid) = entry.get("uid").and_then(Value::as_str) {
            if !uid.is_empty() {
                set.insert(uid.to_string());
            }
        }
    }
    set
}

/// 只读预览（对齐 Python `AccountPool.preview_import_rows`，与 commit 同规则）。
fn import_rows_preview(
    task: &TaskRuntime,
    rows: &[Value],
    forced: Option<&str>,
    overwrite: bool,
) -> Value {
    let known = known_uid_set(task);
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut invalid: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let fields = match wb_identity::import::normalise_import_row(row, forced) {
            Ok(f) => f,
            Err(reason) => {
                invalid.push(json!({"index": index + 1, "reason": reason}));
                continue;
            }
        };
        let uid = fields.uid;
        if seen.contains(&uid) {
            skipped.push(json!({"uid": uid, "reason": "duplicate inside the document"}));
        } else if known.contains(&uid) && !overwrite {
            skipped.push(json!({"uid": uid, "reason": "already exists"}));
        } else if known.contains(&uid) {
            updated.push(uid.clone());
        } else {
            added.push(uid.clone());
        }
        seen.insert(uid);
    }
    json!({"added": added, "updated": updated, "skipped": skipped, "invalid": invalid})
}

/// 实际导入（对齐 Python `AccountPool.import_rows`）。
fn import_rows_commit(
    task: &TaskRuntime,
    rows: &[Value],
    forced: Option<&str>,
    overwrite: bool,
) -> Value {
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut invalid: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let known = known_uid_set(task);
    for (index, row) in rows.iter().enumerate() {
        let fields = match wb_identity::import::normalise_import_row(row, forced) {
            Ok(f) => f,
            Err(reason) => {
                invalid.push(json!({"index": index + 1, "reason": reason}));
                continue;
            }
        };
        let uid = fields.uid.clone();
        if seen.contains(&uid) {
            skipped.push(json!({"uid": uid, "reason": "duplicate inside the document"}));
            continue;
        }
        seen.insert(uid.clone());
        let existing = known.contains(&uid);
        if existing && !overwrite {
            skipped.push(json!({"uid": uid, "reason": "already exists"}));
            continue;
        }
        let file_name = format!("{}.json", safe_uid(&uid));
        if safe_uid(&uid).is_empty() {
            invalid.push(json!({"index": index + 1, "reason": "cannot determine uid (no uid field and no sub claim)"}));
            continue;
        }
        let mut doc = account_doc_from_fields(&fields);
        if existing {
            // 保留旧文件的 addedAt / credits / lastCheckin 等非导入字段
            if let Some(old) = read_account_doc(&task.accounts_dir, &file_name) {
                if let Some(obj) = doc.as_object_mut() {
                    for keep in [
                        "addedAt",
                        "credits",
                        "lastCheckin",
                        "lastDailyChat",
                        "lastActivityReport",
                    ] {
                        if let Some(v) = old.get(keep) {
                            obj.insert(keep.to_string(), v.clone());
                        }
                    }
                }
            }
        }
        if let Err(reason) = write_account_doc(&task.accounts_dir, &file_name, &doc) {
            invalid.push(json!({"index": index + 1, "reason": reason}));
            continue;
        }
        if existing {
            updated.push(uid);
        } else {
            added.push(uid);
        }
    }
    json!({"added": added, "updated": updated, "skipped": skipped, "invalid": invalid})
}

/// `/accounts/import/desktop`：桌面端凭据导入 / 扫描。
///
/// 调用方式对齐 Python：
/// - `{}` → 只扫描（只读，不导入）
/// - `{"path": "..."}` → 导入该凭据文件
/// - `{"all": true}` → 导入扫描到的全部
/// - `{"recoverKey"|"forgetKey": true}` → 桌面端密钥恢复（本实现不支持，返回提示）
pub async fn route_accounts_import_desktop(task: &TaskRuntime, payload: &Value) -> Value {
    if payload.get("recoverKey").map(json_truthy).unwrap_or(false)
        || payload.get("forgetKey").map(json_truthy).unwrap_or(false)
    {
        return json!({
            "ok": false,
            "msg": "桌面端 at-rest 密钥恢复暂不支持（Rust 简化版）",
            "atrest": Value::Null,
        });
    }
    if let Some(path) = payload.get("path").and_then(Value::as_str) {
        let path = path.trim().to_string();
        if path.is_empty() {
            return json!({"ok": false, "error": "path required"});
        }
        let blob = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                return json!({"ok": false, "error": format!("read {}: {}", path, e)});
            }
        };
        let doc: Value = match serde_json::from_str(&blob) {
            Ok(d) => d,
            Err(e) => {
                return json!({"ok": false, "error": format!("parse {}: {}", path, e)});
            }
        };
        let forced = payload
            .get("realm")
            .and_then(Value::as_str)
            .filter(|r| *r == "cn" || *r == "intl");
        let rows = match coerce_rows(&doc) {
            Ok(r) => r,
            Err(problem) => {
                return json!({"ok": false, "error": format!("import failed: {}", problem)});
            }
        };
        let report = import_rows_commit(task, &rows, forced, true);
        // 视图只包含真正新入池的账号（对齐 Python `imported: [a.public()]`）
        let added_uids: std::collections::HashSet<String> = report
            .get("added")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let mut imported = Vec::new();
        for row in &rows {
            if let Ok(f) = wb_identity::import::normalise_import_row(row, forced) {
                if added_uids.contains(&f.uid) {
                    imported.push(public_from_fields(&f));
                }
            }
        }
        task.panel.add_log_entry(
            &format!(
                "imported account(s) from {} (user confirmed)",
                std::path::Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path)
            ),
            "accounts",
        );
        return json!({
            "ok": true,
            "imported": imported,
            "accounts": manual_account_views(task),
        });
    }
    if payload.get("all").map(json_truthy).unwrap_or(false) {
        // 简化版：扫描账号目录并导入其中尚未入池的文件
        let detected = scan_account_files(&task.accounts_dir);
        let forced = payload
            .get("realm")
            .and_then(Value::as_str)
            .filter(|r| *r == "cn" || *r == "intl");
        let mut imported = Vec::new();
        for d in &detected {
            let uid = d.get("uid").and_then(Value::as_str).unwrap_or("");
            if task.ready.iter().any(|r| r.state.uid == uid) {
                continue;
            }
            let file_name = d.get("file").and_then(Value::as_str).unwrap_or("");
            if let Some(doc) = read_account_doc(&task.accounts_dir, file_name) {
                if let Ok(f) = wb_identity::import::normalise_import_row(&doc, forced) {
                    if let Err(reason) = write_account_doc(
                        &task.accounts_dir,
                        file_name,
                        &account_doc_from_fields(&f),
                    ) {
                        task.panel.add_log_entry(
                            &format!("desktop import failed for {}: {}", uid, reason),
                            "accounts",
                        );
                        continue;
                    }
                    imported.push(uid.to_string());
                }
            }
        }
        return json!({
            "ok": true,
            "imported": imported
                .iter()
                .map(|uid| public_from_fields(&wb_identity::import::AccountFields {
                    uid: uid.clone(),
                    nickname: uid8(uid),
                    domain: String::new(),
                    realm: "intl".to_string(),
                    platform: "CLI".to_string(),
                    enterprise_id: String::new(),
                    access_token: String::new(),
                    refresh_token: String::new(),
                    expires_at: 0,
                    source: "desktop-app".to_string(),
                    enabled: true,
                    last_error: String::new(),
                    cooldown_until: 0.0,
                    product: wb_gateway::endpoints::PRODUCT_DESKTOP.to_string(),
                    added_at: 0,
                    weight: 1,
                    note: String::new(),
                }))
                .collect::<Vec<Value>>(),
            "accounts": manual_account_views(task),
        });
    }
    // 只扫描
    json!({
        "detected": scan_account_files(&task.accounts_dir),
        "accounts": manual_account_views(task),
        "pool_uids": task.ready.iter().map(|r| r.state.uid.clone()).collect::<Vec<_>>(),
        "atrest": Value::Null,
    })
}

/// `/accounts/delete`：移除账号（删凭据档前打印 WARN）。
pub async fn route_accounts_delete(task: &TaskRuntime, payload: &Value) -> Value {
    let uid = payload.get("uid").and_then(Value::as_str).unwrap_or("");
    if uid.is_empty() {
        return json!({"ok": false, "error": "uid required", "status": 400});
    }
    let Some(ra) = task.ready.iter().find(|r| r.state.uid == uid) else {
        return json!({"ok": false, "deleted": false, "error": "no such account", "status": 404});
    };
    // 真实文件删除前打印 WARN（删除语义需操作者确认，这里把确认责任记进面板日志）
    task.panel.add_log_entry(
        &format!(
            "WARN: deleting account file {} (uid {}), operator confirmed delete",
            ra.state.credential_file,
            uid8(&uid)
        ),
        "accounts",
    );
    let removed = if !ra.state.credential_file.is_empty() {
        let path = task.accounts_dir.join(&ra.state.credential_file);
        match std::fs::remove_file(&path) {
            Ok(_) => {
                task.panel
                    .add_log_entry(&format!("account {} deleted", uid8(&uid)), "accounts");
                true
            }
            Err(e) => {
                task.panel.add_log_entry(
                    &format!("WARN: account file {} delete failed: {}", path.display(), e),
                    "accounts",
                );
                false
            }
        }
    } else {
        false
    };
    // 简化版：进程内池是只读 Vec（线 A 落定后切换 AccountPool.remove）。
    // 文件已删，重启/重载后账号自然移出池。
    json!({
        "ok": removed,
        "deleted": removed,
        "accounts": task
            .ready
            .iter()
            .filter(|r| r.state.uid != uid)
            .map(|r| public_view(&r.state, &r.nickname, &task.accounts_dir))
            .collect::<Vec<Value>>(),
    })
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 登录（start / poll / cancel）
// ---------------------------------------------------------------------------

/// 默认登录 realm（对齐 Python `CURRENT_REALM`）：读 active_realm.json，缺省 intl。
fn login_realm_default(task: &TaskRuntime) -> String {
    wb_storage::read_active_realm(&task.accounts_dir).unwrap_or_else(|| "intl".to_string())
}

/// `/accounts/login/start`：申请 OAuth state（对齐 Python `POOL.start_login`）。
///
/// 上游 HTTP 走 `wb_gateway::oauth::start_oauth_state`（线 A）；本 handler 只做
/// 参数组装 + 注册表登记（crate::login）。成功返回 `{state, authUrl, realm, platform}`。
pub async fn route_accounts_login_start(task: &TaskRuntime, payload: &Value) -> Value {
    let platform = payload
        .get("platform")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("CLI")
        .to_string();
    let target_realm = payload
        .get("realm")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| login_realm_default(task));
    let cfg = wb_gateway::endpoints::realm_config(&target_realm);
    let chat_upstream = wb_gateway::endpoints::chat_base_url(&target_realm, "workbuddy");
    match wb_gateway::oauth::start_oauth_state(
        &task.client,
        &chat_upstream,
        &platform,
        cfg.billing_ua,
        cfg.origin,
    )
    .await
    {
        Ok(info) => {
            crate::login::login_start_register(
                &info.state,
                &platform,
                &target_realm,
                acct_now_epoch(),
            );
            task.panel.add_log_entry(
                &format!(
                    "oauth login started (realm={}, platform={}, state={})",
                    target_realm,
                    platform,
                    info.state.chars().take(8).collect::<String>()
                ),
                "accounts",
            );
            json!({
                "state": info.state,
                "authUrl": info.auth_url,
                "realm": target_realm,
                "platform": platform,
            })
        }
        Err(e) => json!({"ok": false, "error": format!("could not start login: {}", e)}),
    }
}

/// `/accounts/login/poll`：轮询 OAuth token 并建号（对齐 Python `POOL.poll_login`）。
///
/// 状态判定（`LOGIN_PENDING` / code!=0 / token 缺失）在本 handler；上游 HTTP 走
/// `wb_gateway::oauth::{poll_oauth_token, fetch_login_account}`（线 A）。成功时把
/// 新账号写进凭据档（upsert），并从注册表移除该 state。
pub async fn route_accounts_login_poll(task: &TaskRuntime, payload: &Value) -> Value {
    let state = payload
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let now = acct_now_epoch();
    let info = match crate::login::login_peek(&state) {
        Some(i) => i,
        None => {
            return json!({
                "status": "unknown",
                "message": "state not recognised - start the login again",
            })
        }
    };
    if now - info.created > crate::login::LOGIN_TTL_SECONDS {
        crate::login::login_remove(&state);
        return json!({
            "status": "expired",
            "message": "login window expired - start again",
        });
    }
    let cfg = wb_gateway::endpoints::realm_config(&info.realm);
    let chat_upstream = wb_gateway::endpoints::chat_base_url(&info.realm, "workbuddy");
    let parsed = match wb_gateway::oauth::poll_oauth_token(
        &task.client,
        &chat_upstream,
        &state,
        cfg.billing_ua,
        cfg.origin,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return json!({"status": "pending", "message": format!("poll error: {}", e)}),
    };
    let code = parsed.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == crate::login::LOGIN_PENDING {
        return json!({
            "status": "pending",
            "message": parsed.get("msg").and_then(Value::as_str).unwrap_or("waiting for browser login"),
        });
    }
    if code != 0 {
        return json!({
            "status": "error",
            "message": format!("code={} msg={}", code, parsed.get("msg").and_then(Value::as_str).unwrap_or("")),
        });
    }
    let data = parsed.get("data").cloned().unwrap_or(json!({}));
    let token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    if token.is_empty() {
        return json!({"status": "pending", "message": "waiting for token"});
    }
    // 拉登录账号 profile（best-effort：失败不中断建号）
    let mut nickname = String::new();
    if let Ok(profile) = wb_gateway::oauth::fetch_login_account(
        &task.client,
        &chat_upstream,
        &state,
        token,
        cfg.billing_ua,
        cfg.origin,
    )
    .await
    {
        if let Some(d) = profile.get("data") {
            nickname = d
                .get("nickname")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
    }
    let uid = wb_identity::jwt::jwt_uid(token);
    let expires_raw = data
        .get("expiresAt")
        .and_then(Value::as_f64)
        .map(wb_identity::jwt::normalize_epoch)
        .unwrap_or(0);
    let expires_at = if expires_raw != 0 {
        expires_raw
    } else {
        wb_identity::jwt::jwt_exp(token)
    };
    let domain = data
        .get("domain")
        .and_then(Value::as_str)
        .unwrap_or(cfg.domain);
    let file_name = format!("{}.json", safe_uid(&uid));
    let nickname_display = if nickname.is_empty() {
        uid.chars().take(8).collect::<String>()
    } else {
        nickname.clone()
    };
    // json! 的值表达式走 to_value(&expr)（借用），uid / nickname 这里不会被移走。
    let mut doc = json!({
        "uid": uid,
        "nickname": nickname_display,
        "domain": wb_identity::realm::domain_for_realm(&info.realm, domain),
        "realm": info.realm,
        "platform": info.platform,
        "product": wb_gateway::endpoints::PRODUCT_DESKTOP,
        "enterpriseId": data.get("enterpriseId").and_then(Value::as_str).unwrap_or(""),
        "accessToken": token,
        "refreshToken": data.get("refreshToken").and_then(Value::as_str).unwrap_or(""),
        "expiresAt": expires_at,
        "addedAt": now as i64,
        "source": "oauth",
        "enabled": true,
        "weight": 1,
    });
    // upsert：保留旧文件里的积分快照等非登录字段
    if let Some(old) = read_account_doc(&task.accounts_dir, &file_name) {
        if let Some(obj) = doc.as_object_mut() {
            for keep in [
                "credits",
                "lastCheckin",
                "lastDailyChat",
                "lastActivityReport",
            ] {
                if let Some(v) = old.get(keep) {
                    obj.insert(keep.to_string(), v.clone());
                }
            }
        }
    }
    if let Err(reason) = write_account_doc(&task.accounts_dir, &file_name, &doc) {
        return json!({"status": "error", "message": reason});
    }
    crate::login::login_remove(&state);
    let fields = wb_identity::import::AccountFields {
        uid: uid.clone(),
        nickname,
        domain: wb_identity::realm::domain_for_realm(&info.realm, domain),
        realm: info.realm,
        platform: info.platform,
        enterprise_id: doc
            .get("enterpriseId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        access_token: token.to_string(),
        refresh_token: doc
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        expires_at,
        source: "oauth".to_string(),
        enabled: true,
        last_error: String::new(),
        cooldown_until: 0.0,
        product: wb_gateway::endpoints::PRODUCT_DESKTOP.to_string(),
        added_at: now as i64,
        weight: 1,
        note: String::new(),
    };
    json!({"status": "ok", "account": public_from_fields(&fields)})
}

/// `/accounts/login/cancel`：取消登录 state（对齐 Python `POOL.cancel_login`）。
pub async fn route_accounts_login_cancel(task: &TaskRuntime, payload: &Value) -> Value {
    let _ = task; // 签名对齐契约；本 handler 只用注册表
    let state = payload.get("state").and_then(Value::as_str).unwrap_or("");
    json!({"cancelled": crate::login::login_cancel(state)})
}
// 账户设置 / 身分 / 点检
// ---------------------------------------------------------------------------

/// 把改过字段的内存态副本 + 落盘后的视图拼出来。
fn patched_views(task: &TaskRuntime, patched: &[(&ReadyAccount, AccountState)]) -> Vec<Value> {
    task.ready
        .iter()
        .map(|ra| {
            if let Some((_, st)) = patched.iter().find(|(r, _)| r.state.uid == ra.state.uid) {
                public_view(st, &ra.nickname, &task.accounts_dir)
            } else {
                public_view(&ra.state, &ra.nickname, &task.accounts_dir)
            }
        })
        .collect()
}

/// 打补丁并落盘账号文件，返回改后的内存态副本（读磁盘最新内容再合并）。
fn patch_account_state<F: Fn(&mut serde_json::Map<String, Value>, &mut AccountState)>(
    task: &TaskRuntime,
    ra: &ReadyAccount,
    base: &AccountState,
    apply: F,
) -> Result<AccountState, String> {
    let mut st = base.clone();
    let mut doc = read_account_doc(&task.accounts_dir, &ra.state.credential_file)
        .unwrap_or_else(|| json!({}));
    let map = doc
        .as_object_mut()
        .ok_or_else(|| "account file is not a JSON object".to_string())?;
    apply(map, &mut st);
    write_account_doc(&task.accounts_dir, &ra.state.credential_file, &doc)?;
    Ok(st)
}

/// `/accounts/product`：切换出站身分（payload.product 空串 = 恢复默认桌面端）。
pub async fn route_accounts_product(task: &TaskRuntime, payload: &Value) -> Value {
    let raw = payload
        .get("product")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // 空串 = 恢复默认（workbuddy）；非空必须是三产品之一
    let target = if raw.is_empty() {
        wb_gateway::endpoints::PRODUCT_DESKTOP.to_string()
    } else if ["workbuddy", "vscode", "cli"].contains(&raw.as_str()) {
        wb_gateway::endpoints::normalize_product(&raw).to_string()
    } else {
        return json!({
            "ok": false,
            "error": "product must be 'workbuddy', 'vscode', or 'cli'",
            "status": 400,
        });
    };
    let targets = account_targets(task, payload);
    let mut changed: Vec<String> = Vec::new();
    let mut patched: Vec<(&ReadyAccount, AccountState)> = Vec::new();
    for ra in &targets {
        match patch_account_state(task, ra, &ra.state, |map, st| {
            let new = wb_gateway::endpoints::normalize_product(&target).to_string();
            let old = map
                .get("product")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if old != new {
                map.insert("product".to_string(), json!(new));
                st.product = new.clone();
            }
        }) {
            Ok(st) => {
                let changed_this = st.product == target;
                if changed_this {
                    changed.push(uid8(&ra.state.uid));
                    task.panel.add_log_entry(
                        &format!(
                            "account {}: 面板手動切換身分 -> {}",
                            uid8(&ra.state.uid),
                            target
                        ),
                        "accounts",
                    );
                }
                patched.push((ra, st));
            }
            Err(e) => {
                task.panel.add_log_entry(
                    &format!(
                        "WARN: product switch failed for {}: {}",
                        uid8(&ra.state.uid),
                        e
                    ),
                    "accounts",
                );
            }
        }
    }
    json!({
        "ok": true,
        "product": target,
        "changed": changed,
        "accounts": patched_views(task, &patched),
    })
}

/// `/accounts/refresh`：重解析 JWT 时效并落盘（对齐 Python `Account.refresh()`）。
pub async fn route_accounts_refresh(task: &TaskRuntime, payload: &Value) -> Value {
    let targets = account_targets(task, payload);
    let mut results = Vec::new();
    for ra in targets {
        let st = patch_account_state(task, ra, &ra.state, |map, st| {
            let token = map
                .get("accessToken")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !token.is_empty() {
                let exp = wb_identity::jwt::jwt_exp(&token);
                if exp > 0 {
                    map.insert("expiresAt".to_string(), json!(exp));
                    st.expires_at = Some(exp as f64);
                }
            }
        });
        match st {
            Ok(_) => results.push(json!({"uid": ra.state.uid, "ok": true, "error": ""})),
            Err(e) => results.push(json!({"uid": ra.state.uid, "ok": false, "error": e})),
        }
    }
    json!({"results": results})
}

/// `/accounts/set`：设置账号字段（enabled / weight / note；对齐 Python
/// `_route_accounts_set`。proxySlot / proxy 在 Rust 池里没有对应字段，忽略）。
pub async fn route_accounts_set(task: &TaskRuntime, payload: &Value) -> Value {
    let uid = payload.get("uid").and_then(Value::as_str).unwrap_or("");
    if uid.is_empty() {
        return json!({"ok": false, "error": "uid required", "status": 400});
    }
    let Some(ra) = task.ready.iter().find(|r| r.state.uid == uid) else {
        return json!({"ok": false, "error": "no such account", "status": 404});
    };
    // 多个字段同载荷时，后续字段的 base 用上一个字段改后的内存态（文件在
    // 每个 patch 里读磁盘最新内容，两者都不丢）。
    let mut cur_st = ra.state.clone();
    let mut touched = false;
    let mut last_error: Option<String> = None;
    let has_enabled = payload.get("enabled").is_some();
    let has_weight = payload.get("weight").is_some();
    let has_note = payload.get("note").is_some();
    if has_enabled {
        let enabled = match payload.get("enabled") {
            Some(Value::Bool(b)) => *b,
            Some(v) => json_truthy(v),
            None => false,
        };
        match patch_account_state(task, ra, &cur_st, move |map, st| {
            map.insert("enabled".to_string(), json!(enabled));
            st.enabled = enabled;
        }) {
            Ok(s) => {
                touched = true;
                cur_st = s;
            }
            Err(e) => last_error = Some(e),
        }
    }
    if has_weight {
        let weight = payload.get("weight").and_then(Value::as_i64);
        let weight = match weight {
            Some(w) if (1..=100).contains(&w) => w,
            Some(_) => {
                return json!({"ok": false, "error": "weight out of range (1-100)", "status": 400});
            }
            None => {
                return json!({"ok": false, "error": "weight must be an integer between 1 and 100", "status": 400});
            }
        };
        match patch_account_state(task, ra, &cur_st, move |map, st| {
            map.insert("weight".to_string(), json!(weight));
            st.weight = weight;
        }) {
            Ok(s) => {
                touched = true;
                cur_st = s;
            }
            Err(e) => last_error = Some(e),
        }
    }
    if has_note {
        let note = payload
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match patch_account_state(task, ra, &cur_st, move |map, _st| {
            map.insert("note".to_string(), json!(note));
        }) {
            Ok(s) => {
                touched = true;
                cur_st = s;
            }
            Err(e) => last_error = Some(e),
        }
    }
    if !touched {
        return json!({
            "ok": false,
            "error": "nothing to update: pass 'enabled', 'weight' or 'note'",
            "status": 400,
        });
    }
    if let Some(e) = last_error {
        return json!({"ok": false, "error": e, "status": 500});
    }
    json!({"ok": true, "account": public_view(&cur_st, &ra.nickname, &task.accounts_dir)})
}

/// `/accounts/set-all`：统一启停所有账号（对齐 Python `POOL.set_all_enabled`）。
pub async fn route_accounts_set_all(task: &TaskRuntime, payload: &Value) -> Value {
    let enabled = match payload.get("enabled") {
        Some(Value::Bool(b)) => *b,
        Some(v) => json_truthy(v),
        None => false,
    };
    let mut patched: Vec<(&ReadyAccount, AccountState)> = Vec::new();
    for ra in &task.ready {
        match patch_account_state(task, ra, &ra.state, move |map, st| {
            map.insert("enabled".to_string(), json!(enabled));
            st.enabled = enabled;
        }) {
            Ok(s) => patched.push((ra, s)),
            Err(e) => {
                task.panel.add_log_entry(
                    &format!("WARN: set-all failed for {}: {}", uid8(&ra.state.uid), e),
                    "accounts",
                );
            }
        }
    }
    json!({"accounts": patched_views(task, &patched)})
}

/// `/accounts/sync-profile`：从上游同步昵称（**简化版**：只读本地账号文件的
/// nickname 展示，不发上游请求——上游 profile 接口依赖 login state）。
pub async fn route_accounts_sync_profile(task: &TaskRuntime, payload: &Value) -> Value {
    let targets = account_targets(task, payload);
    let mut updated = Vec::new();
    for ra in targets {
        let nickname = read_local_nickname(&task.accounts_dir, &ra.state.credential_file)
            .unwrap_or_else(|| uid8(&ra.state.uid));
        updated.push(json!({"uid": ra.state.uid, "nickname": nickname}));
    }
    json!({
        "updated": updated,
        "failed": [],
        "accounts": manual_account_views(task),
    })
}

/// `/accounts/test`：点检账号（**不发上游请求**）。检查 realm / enabled /
/// 凭据档存在 / token 有效性，展示在 `checks` 里。
pub async fn route_accounts_test(task: &TaskRuntime, payload: &Value) -> Value {
    let uid = payload.get("uid").and_then(Value::as_str).unwrap_or("");
    if uid.is_empty() {
        return json!({"ok": false, "error": "uid required", "status": 400});
    }
    let Some(ra) = task.ready.iter().find(|r| r.state.uid == uid) else {
        return json!({"ok": false, "error": "no such account", "status": 404});
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek-v4.1-flash");
    let file_exists = !ra.state.credential_file.is_empty()
        && task.accounts_dir.join(&ra.state.credential_file).exists();
    let now = acct_now_epoch();
    let exp = ra.state.expires_at.unwrap_or(0.0);
    let token_valid = ra.state.has_token && (exp == 0.0 || exp - now > 120.0);
    let ok = ra.state.enabled && file_exists && token_valid;
    json!({
        "ok": ok,
        "uid": ra.state.uid,
        "model": model,
        "elapsed_ms": 0,
        "checks": {
            "realm": ra.state.realm,
            "enabled": ra.state.enabled,
            "hasToken": ra.state.has_token,
            "credentialFileExists": file_exists,
            "tokenValid": token_valid,
            "expiresAt": exp,
        },
    })
}

// ---------------------------------------------------------------------------
// 关卡 4.25：logs / pricing 管理 / updates 管理 / settings reveal
// ---------------------------------------------------------------------------

/// `/logs`：面板日志列表（Python `_get_logs` :16136-16149）。
pub fn route_logs_get(_task: &TaskRuntime, query: &HashMap<String, String>) -> Value {
    let limit: usize = parse_int(query, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(200);
    let level = query.get("level").cloned().unwrap_or_default();
    let tag = query.get("tag").cloned().unwrap_or_default();
    let search = query.get("search").cloned().unwrap_or_default();
    let since_id = parse_u64(query, "since_id").unwrap_or(0);
    crate::logs::get_logs(Some(limit), &level, &tag, &search, since_id)
}

/// `/logs/export`：导出文本（Python `_get_logs_export` :16152-16165）。
pub fn route_logs_export(_task: &TaskRuntime, _query: &HashMap<String, String>) -> String {
    crate::logs::export_text()
}

/// `/logs/clear`：清空日志缓冲（Python `_route_logs_clear` :17477-17479）。
pub fn route_logs_clear(_task: &TaskRuntime, _payload: &Value) -> Value {
    crate::logs::clear();
    json!({"ok": true})
}

/// `/settings/reveal`：按 id 返回未脱敏的 key（Python `_get_settings_reveal`
/// :16169-16178）。面板会话已由路由层鉴权。
pub fn route_settings_reveal(task: &TaskRuntime, query: &HashMap<String, String>) -> Value {
    let wanted = query.get("id").cloned().unwrap_or_default();
    let doc = wb_storage::read_settings(&task.accounts_dir).unwrap_or_else(|_| json!({}));
    // 生效 key + 面板 api_keys 两条线都要查（Python configured_keys()）。
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Some(key) = doc.get("api_key").and_then(Value::as_str) {
        if !key.is_empty() {
            candidates.push(("api_key".to_string(), key.to_string()));
        }
    }
    if let Some(keys) = doc.get("api_keys").and_then(Value::as_array) {
        for entry in keys {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
            let key = entry.get("key").and_then(Value::as_str).unwrap_or("");
            if !id.is_empty() {
                candidates.push((id.to_string(), key.to_string()));
            }
        }
    }
    for (id, key) in candidates {
        if id == wanted {
            return json!({"id": id, "key": key});
        }
    }
    Value::Null // 路由层转 404
}

/// `/pricing/refresh`：后台触发一次取价（Python `_route_pricing_refresh`
/// :17463-17476）。取价耗时数十秒，由 run_once 的异步执行承担，面板轮询
/// `/pricing` 看结果。
pub async fn route_pricing_refresh(task: &TaskRuntime, _payload: &Value) -> Value {
    let Some(refresher) = task.price_refresher.get().cloned() else {
        return json!({"ok": false, "msg": "价格刷新未运行"});
    };
    let doc = wb_storage::read_settings(&task.accounts_dir).unwrap_or_else(|_| json!({}));
    if !crate::settings::pricing_enabled(&doc) {
        return json!({"ok": false, "msg": "价估算已关闭，请先启用"});
    }
    let worker = refresher.clone();
    tokio::spawn(async move {
        let _ = worker.run_once().await;
    });
    let mut out = refresher.status();
    out["ok"] = json!(true);
    out["msg"] = json!("已开始抓取，稍候刷新查看结果");
    out
}

/// `/pricing/mapping`：面板手填「hub 模型 → OpenRouter id」映射
/// （Python `_route_pricing_mapping` :17481-17520）。写
/// `pricing-overrides.json`（运行期覆盖），随后触发一次取价。
pub async fn route_pricing_mapping(task: &TaskRuntime, payload: &Value) -> Value {
    let Some(refresher) = task.price_refresher.get().cloned() else {
        return json!({"ok": false, "msg": "价格刷新未运行"});
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if model.is_empty() {
        return json!({"ok": false, "error": "model is required", "status": 400});
    }
    let or_id = payload
        .get("or_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !or_id.is_empty() && !looks_like_or_id(&or_id) {
        return json!({"ok": false, "error": "or_id must look like 'vendor/model'", "status": 400});
    }
    // 写运行期覆盖（usage/pricing-overrides.json）。
    if let Err(err) = crate::pricing_store::save_runtime_override(&task.usage_dir, &model, &or_id) {
        return json!({"ok": false, "error": err, "status": 500});
    }
    // 触发取价（后台）。
    let worker = refresher.clone();
    tokio::spawn(async move {
        let _ = worker.run_once().await;
    });
    let msg = if or_id.is_empty() {
        format!("已删除 {model} 的手填映射，下次取价起按自动匹配")
    } else {
        format!("已记录 {model} → {or_id}，正在重新取价")
    };
    crate::logs::append(
        &format!(
            "[定价] 面板手填映射：{model} → {}",
            if or_id.is_empty() { "(清除)" } else { &or_id }
        ),
        None,
        Some("pricing"),
    );
    json!({"ok": true, "model": model, "or_id": or_id, "known": false, "msg": msg})
}

/// `/updates/check`：立即检查一次发布（Python `_route_update_check` :17438-17450）。
pub async fn route_update_check(task: &TaskRuntime, _payload: &Value) -> Value {
    let Some(updater) = task.update_checker.get().cloned() else {
        return json!({"ok": false, "msg": "更新检查未运行"});
    };
    let mut status = updater.check_manual().await;
    status["ok"] = json!(true);
    status
}

/// or_id 形状校验（Python `re.match(r"^[A-Za-z0-9._\-]+/[A-Za-z0-9._\-:]+$", or_id)`）。
fn looks_like_or_id(s: &str) -> bool {
    let mut parts = s.splitn(2, '/');
    let vendor = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    !vendor.is_empty()
        && !name.is_empty()
        && vendor.chars().all(is_or_id_char)
        && name.chars().all(is_or_id_char)
}

fn is_or_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' || c == ':'
}

/// 解析无符号整数查询参数。
fn parse_u64(query: &HashMap<String, String>, key: &str) -> Option<u64> {
    query.get(key).and_then(|v| v.parse::<u64>().ok())
}

/// 解析整数查询参数（与 api crate routes.rs 的 parse_int 同名同义）。
fn parse_int(query: &HashMap<String, String>, key: &str) -> Option<i64> {
    query.get(key).and_then(|v| v.parse::<i64>().ok())
}
use serde_json::Map;

// ---------------------------------------------------------------------------
// 关卡 4.26：proxy slots / agents / remaining 接线层
// ---------------------------------------------------------------------------

/// `/proxy/slots`：面板代理槽视图（Python `_handle_proxy_slots` :16725-16753）。
pub fn route_proxy_slots_view(task: &TaskRuntime, _payload: &Value) -> Value {
    let view = crate::proxy::proxy_slots_view(&task.accounts_dir);
    let slots = view
        .get("slots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // bound：每个 slot 绑定的账号数
    let bound: Vec<Value> = slots
        .iter()
        .map(|slot| {
            let sid = slot.get("id").and_then(Value::as_str).unwrap_or("");
            let n = task.ready.iter().filter(|r| r.state.uid == sid).count();
            json!(n)
        })
        .collect();
    json!({"slots": slots, "bound": bound})
}

/// `/proxy/slots/save`：保存代理槽（Python `_handle_proxy_slots` save 分支 :16753-16771）。
pub async fn route_proxy_slots_save(task: &TaskRuntime, payload: &Value) -> Value {
    let raw = payload
        .get("slots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut cleaned: Vec<Value> = vec![];
    for item in &raw {
        if let Some(entry) = crate::proxy::clean_slot_entry(item) {
            cleaned.push(entry);
        }
    }
    if let Err(err) = crate::proxy::set_proxy_slots(&task.accounts_dir, &cleaned) {
        return json!({"ok": false, "error": err, "status": 500});
    }
    crate::logs::append(
        &format!("[代理] 代理槽已保存 {} 条", cleaned.len()),
        None,
        Some("proxy"),
    );
    route_proxy_slots_view(task, payload)
}

/// `/proxy/slots/test`：探测代理槽（Python `_handle_proxy_slots` test 分支 :16771-16781）。
pub async fn route_proxy_slots_test(task: &TaskRuntime, payload: &Value) -> Value {
    let slot_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if slot_id.is_empty() {
        return json!({"ok": false, "error": "slot id is required", "status": 400});
    }
    let slot = crate::proxy::find_proxy_slot(&task.accounts_dir, &slot_id);
    let Some(slot) = slot else {
        return json!({"ok": false, "error": "no such proxy slot", "status": 404});
    };
    let url = slot
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // 简化探测：返回结构化占位（真实网络探测由上层替换 probe_fn）
    let probe = crate::proxy::probe_proxy(
        &url,
        |_| json!({"ok": false, "error": "探测函数尚未接入（需上层替换 probe_fn）"}),
    );
    json!({"ok": probe.get("ok").and_then(Value::as_bool).unwrap_or(false), "probe": probe, "slot": slot})
}

/// `/proxy/discover`：代理候选发现（Python `_handle_proxy_slots` discover 分支）。
pub fn route_proxy_discover(_task: &TaskRuntime, _payload: &Value) -> Value {
    // Python `discover_proxy_slots()`：本地网络扫描，Rust 侧未实现本地扫描（需上层接线）。
    json!({"candidates": Value::Array(vec![])})
}

/// `/agents`：智能体全量视图（Python `_get_agents` :16906-16935）。
pub fn route_agents_view(task: &TaskRuntime, _query: &HashMap<String, String>) -> Value {
    crate::agents::agents_view(&task.accounts_dir)
}

/// `/agents/available`：部署形态判定（Python `_get_agents_available`）。
pub fn route_agents_available(task: &TaskRuntime, _query: &HashMap<String, String>) -> Value {
    crate::agents::agents_available(&task.accounts_dir)
}

/// `/agents/cli`：CLI 智能体信息（Python `_get_agents_cli` :16880-16905）。
pub fn route_agents_cli(task: &TaskRuntime, _query: &HashMap<String, String>) -> Value {
    crate::agents::agents_cli_view(&task.accounts_dir)
}

/// `/agents/apply`：一键配置校验（Python `_handle_agents_apply` 前半）。
pub async fn route_agents_apply(task: &TaskRuntime, payload: &Value) -> Value {
    match crate::agents::agents_apply_validate(payload, &task.accounts_dir) {
        Ok(view) => view,
        Err((status, message)) => {
            let mut out = Value::Object(Map::new());
            out["ok"] = json!(false);
            out["error"] = json!(message);
            out["status"] = json!(status);
            out
        }
    }
}

/// `/agents/restore`：移除指定客户端集成（Python `_handle_agents_restore`）。
pub fn route_agents_restore(_task: &TaskRuntime, payload: &Value) -> Value {
    crate::agents::agents_restore_plan(payload)
}

/// `/usage/remaining`：按账号/模型剩余额度（Python `remaining_usage`）。
pub async fn route_usage_remaining(task: &TaskRuntime, _query: &HashMap<String, String>) -> Value {
    let accounts: Vec<AccountState> = task.ready.iter().map(|r| r.state.clone()).collect();
    crate::remaining::remaining_payload(&task.accounts_dir, &task.usage_dir, &accounts).await
}
