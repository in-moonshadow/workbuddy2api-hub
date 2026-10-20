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
use crate::growth_api::{accept_tasks, claim_task, fetch_growth_tasks, TaskAccount, TaskEndpoints};
use crate::growth_runner::{
    build_cat_travel, run_growth_tasks, CatTravel, RunnerAccount, RunnerCtx, RunnerDeps, RunnerEnv,
};
use crate::growth_runner::{Clock, LocalClock, RealSleeper, Sleeper};
use crate::scheduler::AccountView;
use crate::scheduler::{CycleExecutor, LocalScheduleClock, PanelSink, Scheduler};
use crate::snapshot_cache::{cache_ttl_from_env, growth_snapshot, SnapshotCache};
use crate::GrowthSummary;
use crate::GrowthTask;

// ---------------------------------------------------------------------------
// 生产装配辅助结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PooledTaskAccount {
    pub uid: String,
    pub token: String,
    pub enterprise_id: String,
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
    pub pool: Vec<AccountState>,
    pub pool_present: bool,
    pub ready: Vec<ReadyAccount>,
    pub cache: Arc<SnapshotCache>,
    pub billing_base: String,
    pub endpoints: TaskEndpoints<'static>,
    pub client: UpstreamClient,
    pub clock: LocalClock,
    pub schedule_clock: LocalScheduleClock,
    pub sleeper: RealSleeper,
    pub panel: Arc<dyn PanelSink>,
    pub exec: OnceLock<Arc<dyn CycleExecutor>>,
    pub scheduler: OnceLock<Arc<Scheduler<'static>>>,
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
        let clock = LocalClock;
        let schedule_clock = LocalScheduleClock;
        let sleeper = RealSleeper;

        let task = Arc::new(TaskRuntime {
            accounts_dir,
            pool,
            pool_present,
            ready,
            cache,
            billing_base,
            endpoints,
            client,
            clock,
            schedule_clock,
            sleeper,
            panel: panel.clone(),
            exec: OnceLock::new(),
            scheduler: OnceLock::new(),
        });

        let exec: Arc<dyn CycleExecutor> = Arc::new(ProductionExecutor {
            task: Arc::downgrade(&task),
        });
        let _ = task.exec.set(exec.clone());

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
        let runtime = Arc::new(TaskRuntime {
            accounts_dir: PathBuf::new(),
            pool: vec![],
            pool_present: false,
            ready,
            cache: Arc::new(SnapshotCache::with_ttl(0.0)),
            billing_base: String::new(),
            endpoints,
            client: UpstreamClient::new(),
            clock: LocalClock,
            schedule_clock: LocalScheduleClock,
            sleeper: RealSleeper,
            panel,
            exec: OnceLock::new(),
            scheduler: OnceLock::new(),
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
                clock: &task.clock as &dyn Clock,
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
                clock: &task.clock as &dyn Clock,
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
                clock: &task.clock,
            };
            let res =
                build_cat_travel(&task.client, &runner_acc, &task.endpoints, &mut logs, &ctx).await;
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
            let runner_acc = ra.runner_account();
            let url = format!("{}/activity/growth/streak/claim", task.endpoints.chat_base);
            let empty = json!({});
            match task
                .client
                .post(&url, runner_acc.api.chat_headers, &empty)
                .await
            {
                Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                    Ok(d) if d.get("code").and_then(Value::as_i64) == Some(0) => {
                        let data = d.get("data").cloned().unwrap_or(json!({}));
                        let logs = data
                            .get("logs")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        json!({"ok": true, "logs": logs})
                    }
                    Ok(d) => {
                        let msg = d
                            .get("msg")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                format!(
                                    "code={}",
                                    d.get("code").map(|c| c.to_string()).unwrap_or_default()
                                )
                            });
                        json!({"ok": false, "error": msg})
                    }
                    Err(e) => json!({"ok": false, "error": format!("streak bonus failed: {e}")}),
                },
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    json!({
                        "ok": false,
                        "error": format!("HTTP {}: {}", status.as_u16(), text)
                    })
                }
                Err(e) => json!({"ok": false, "error": format!("streak bonus failed: {e}")}),
            }
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
                    return json!({"ok": false, "error": "account not found", "logs": []});
                }
            };
            let account = ra.creds.as_task_account();
            let ep = &task.endpoints;
            let mut logs: Vec<String> = Vec::new();
            let mut credit: i64 = 0;

            let tasks = fetch_growth_tasks(&task.client, &account, ep).await;
            let night_tasks: Vec<&GrowthTask> = tasks
                .iter()
                .filter(|t| crate::NIGHT_TASK_CODES.contains(&t.task_code.as_str()))
                .collect();

            if night_tasks.is_empty() {
                return json!({"ok": true, "logs": [], "credit": 0});
            }

            for nt in night_tasks {
                if nt.status == "not_accepted" {
                    let codes = vec![nt.task_code.clone()];
                    accept_tasks(&task.client, &account, &codes, 20, ep).await;
                    let tasks2 = fetch_growth_tasks(&task.client, &account, ep).await;
                    if let Some(nt2) = tasks2.iter().find(|t| t.task_code == nt.task_code) {
                        if nt2.status == "completed" {
                            let claim = claim_task(&task.client, &account, &nt.task_code, ep).await;
                            if claim.ok {
                                credit += claim.credit;
                                logs.push("领奖成功".into());
                            } else {
                                logs.push(format!("领奖失败: {}", claim.msg.unwrap_or_default()));
                            }
                        }
                    }
                }
            }

            json!({"ok": true, "logs": logs, "credit": credit})
        })
    }
    fn daily_chat<'a>(
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
            let url = format!("{}/v2/activity/daily-chat", task.endpoints.chat_base);
            let empty = json!({});
            match task
                .client
                .post(&url, runner_acc.api.chat_headers, &empty)
                .await
            {
                Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                    Ok(d) if d.get("code").and_then(Value::as_i64) == Some(0) => {
                        let msg = d
                            .get("msg")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_default();
                        json!({"ok": true, "msg": msg})
                    }
                    Ok(d) => {
                        let msg = d
                            .get("msg")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                format!(
                                    "code={}",
                                    d.get("code").map(|c| c.to_string()).unwrap_or_default()
                                )
                            });
                        json!({"ok": false, "error": msg})
                    }
                    Err(e) => json!({"ok": false, "error": format!("daily chat failed: {e}")}),
                },
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    json!({
                        "ok": false,
                        "error": format!("HTTP {}: {}", status.as_u16(), text)
                    })
                }
                Err(e) => json!({"ok": false, "error": format!("daily chat failed: {e}")}),
            }
        })
    }
    fn accounts(&self) -> Vec<AccountView> {
        let task = self.task.upgrade().expect("runtime alive");
        task.ready.iter().map(|r| r.view.clone()).collect()
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
            clock: &task.clock as &dyn Clock,
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
            clock: &task.clock as &dyn Clock,
            credits_refresh: Box::new(|| None),
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
            clock: &task.clock as &dyn Clock,
        };
        let res =
            build_cat_travel(&task.client, &runner_acc, &task.endpoints, &mut logs, &ctx).await;
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
    let mut deps = ClaimAllDeps::new(
        exec_ref,
        &task.clock as &dyn crate::growth_runner::Clock,
        sleeper_ref,
    );
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

// SnapshotCache invalidate helper trait not needed; call directly.
// GrowthSummary default assumed available.
