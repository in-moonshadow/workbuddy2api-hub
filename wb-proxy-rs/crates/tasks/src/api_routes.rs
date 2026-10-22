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
        // ⚠️ 已知缺口：取价清单为空（Rust 侧无模型目录）⇒ 抓回来的模型列表
        // 匹配不到任何 hub 模型 ⇒ 不写盘。见 `crate::price_source` 的模块文档。
        let pricing_interval = settings.pricing_refresh_minutes.max(0) as f64 * 60.0;
        let price_refresher = crate::price_source::production_price_refresher(
            &task.accounts_dir,
            &task.usage_dir,
            pricing_interval,
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
