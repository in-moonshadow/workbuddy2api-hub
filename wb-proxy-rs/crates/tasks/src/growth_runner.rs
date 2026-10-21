//! 关卡 4.5：`_progress_one` 全判定链 + `run_growth_tasks` + 猫猫旅行（`do_cat_travel`）。
//!
//! 权威参照 `wb_tasks.py`：
//! - [`progress_one`] ← `:525-631`（契约 8 的七条分支按序 + 契约 9 的 6 次轮询）
//! - [`run_growth_tasks`] ← `:634-709`（契约 10 的四步顺序 + 二次接取）
//! - [`build_cat_travel`] ← `:463-522`（含 `_travel_status` / `_travel_depart` / `ensure_buddy`）
//!
//! ## 与 Python 的分叉（本轮只登记，不改 Python）
//! - **`credits` 刷新注入**：Python 第 4 步调 `account.fetch_credits()`，失败时
//!   `credits` 保持旧值。Rust 侧由调用方注入刷新闭包，`None` = 刷新失败 →
//!   本轮不改 `credits`，与 Python 同语义。闭包形态而非直接调用，是因为
//!   `wb-tasks → wb-api` 禁止依赖，而积分形状属于 api 侧。
//! - **日志注入（契约 1）**：Python 的 `_log` 默认 no-op。本模块的日志是返回值
//!   的一部分（`logs: Vec<String>`），不设静默默认。
//!
//! ## 可注入的时间与依赖
//! `time.sleep` / `time.localtime` 在上游硬编码，照搬会让测试每关耗数分钟。
//! 故 [`Sleeper`] 与 [`Clock`] 做成 trait：生产用 [`RealSleeper`]（`tokio::time::sleep`）
//! 与 [`LocalClock`]；测试用 [`NoopSleeper`]（只记录调用）与固定时钟。
//!
//! ## 两条旅行路径的 `/v2` 口径（R1）
//! 本模块的 `_travel_status` 走 `{chat}/activity/growth/buddy/travel/status`
//! （**无** `/v2`），与 `events::growth_summary_travel` 的 `/v2/activity/...` 是两条
//! 独立路径，各自带测试，不抽公共常量。
//!
//! ⚠️ 2026-10-20 修正：本模块此前误写成 `/activity/growth/travel/status`
//! （**漏了 `/buddy/`**），与 Python `wb_tasks.py:412` 不符，且既有单测把同一
//! 错误路径当作期望值。现已按 Python 逐字改正，测试断言同步更新。

use std::time::Duration;

use serde_json::{json, Value};

use crate::events::{build_event, report_events, rotating_expert};
use crate::growth_api::{accept_tasks, claim_task, fetch_growth_tasks, TaskAccount, TaskEndpoints};
use crate::{in_night_window, task_spec, GrowthTask};
use wb_gateway::outbound::UpstreamClient;

/// 猫猫旅行四个出站调用的时限（Python `wb_tasks.py:414/443/450/481` 均为 `timeout=10`）。
///
/// ⚠️ reqwest 的 `RequestBuilder::timeout` 是**总时限**（连接 → 响应体读完），
/// Python 的 `urlopen(timeout=N)` 是**逐次读**的 socket 超时。对几 KB 的 JSON
/// 答复两者没有实质差别；总时限更严格（不会无限挂住）。
pub const TRAVEL_TIMEOUT: Duration = Duration::from_secs(10);

/// 猫猫旅行的默认出发地 id（`config` 拿不到 `locations[0].id` 时回退 1）。
fn default_location_id() -> Value {
    json!(1)
}

// ---------------------------------------------------------------------------
// 可注入依赖
// ---------------------------------------------------------------------------

/// 可注入的「睡」。生产真实休眠，测试只记录时长。
///
/// 返回 `BoxFuture` 而非 `impl Future`：本 trait 以 `&dyn Sleeper` 注入，
/// 只有对象安全的写法才能编译（RPITIT 的 trait 不是 dyn compatible）。
pub trait Sleeper: Send + Sync {
    /// 休眠 `secs` 秒。
    fn sleep(
        &self,
        secs: f64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// 真实休眠（`tokio::time::sleep`），对齐 Python 的 `time.sleep`。
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(
        &self,
        secs: f64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if secs > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            }
        })
    }
}

/// 不睡，只记录调用（测试用；否则 6 次轮询要等约 20 秒）。
#[derive(Default)]
pub struct NoopSleeper {
    calls: std::sync::Mutex<Vec<f64>>,
}

impl NoopSleeper {
    /// 已记录的休眠秒数序列（按调用顺序）。
    pub fn calls(&self) -> Vec<f64> {
        self.calls.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl Sleeper for NoopSleeper {
    fn sleep(
        &self,
        secs: f64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if let Ok(mut v) = self.calls.lock() {
                v.push(secs);
            }
        })
    }
}

/// 可注入的「现在几点」（本地时区小时）；夜猫子窗口判定依赖它。
pub trait Clock: Send + Sync {
    /// 本地时区小时，`0..=23`。
    fn local_hour(&self) -> u32;

    /// 本地时间戳字符串 `%Y-%m-%d %H:%M:%S`，对齐 Python `time.strftime`。
    ///
    /// 签到/活跃上报落盘 `lastCheckin`/`lastActivityReport` 用它；`can_*` 只比对
    /// 前 10 位日期，故格式必须与 `%Y-%m-%d` 前缀兼容。默认实现走真实时钟，
    /// 既有只实现 `local_hour` 的测试 fake 无需改动。
    fn local_now(&self) -> String {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// 生产时钟：`chrono::Local::now().hour()`，对齐 `time.localtime().tm_hour`。
pub struct LocalClock;

impl Clock for LocalClock {
    fn local_hour(&self) -> u32 {
        use chrono::Timelike;
        chrono::Local::now().hour()
    }
}

/// 猫猫旅行钩子：默认真实实现，测试可注入结果（含失败），避免跑真实状态机。
///
/// 同 [`Sleeper`]：以 `&dyn CatTravelRunner` 注入，故返回装箱 future。
pub trait CatTravelRunner: Send + Sync {
    /// 返回 `{ok, action, credit?, msg}`，形状对齐 `do_cat_travel`。
    fn run<'a>(
        &'a self,
        client: &'a UpstreamClient,
        acc: &'a RunnerAccount<'_>,
        ep: &'a TaskEndpoints<'_>,
        logs: &'a mut Vec<String>,
        ctx: &'a RunnerCtx<'_>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>>;
}

/// 真实猫猫旅行（[`build_cat_travel`]）。
pub struct RealCatTravel;

impl CatTravelRunner for RealCatTravel {
    fn run<'a>(
        &'a self,
        client: &'a UpstreamClient,
        acc: &'a RunnerAccount<'_>,
        ep: &'a TaskEndpoints<'_>,
        logs: &'a mut Vec<String>,
        ctx: &'a RunnerCtx<'_>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
        Box::pin(async move { build_cat_travel(client, acc, ep, logs, ctx).await })
    }
}

/// 猫猫旅行开关：`Disabled` 时第 3 步整体跳过（M4-b 之前的默认档位，
/// 因 `ensure_buddy` 的补齐链要等 M4-b 的完整任务集才可靠）。
pub enum CatTravel<'a> {
    /// 不执行第 3 步。
    Disabled,
    /// 执行（真实或注入）。
    Run(&'a dyn CatTravelRunner),
}

/// 运行环境：域名口径（生产常量 / 测试 mock）。
#[derive(Default)]
pub struct RunnerEnv<'a> {
    /// `{chat, web}` 两套 base；测试指向同一个 wiremock。
    pub ep: TaskEndpoints<'a>,
}

/// 一轮运行所需的可注入依赖集合。
pub struct RunnerDeps<'a> {
    /// 睡。
    pub sleeper: &'a dyn Sleeper,
    /// 现在几点。
    pub clock: &'a dyn Clock,
    /// 刷新积分余额。`None` = 刷新失败（**不改**旧值），对齐 `fetch_credits()` 失败分支。
    pub credits_refresh: Box<dyn FnOnce() -> Option<Value> + Send + 'a>,
    /// 猫猫旅行。
    pub cat_travel: CatTravel<'a>,
    /// 任务间隔秒（Python 默认 `gap=1.0`）。
    pub gap: f64,
}

impl<'a> RunnerDeps<'a> {
    /// 仅指定必备项，其余走生产默认（`gap = 1.0`、猫猫旅行启用、积分刷新返回 `None`）。
    pub fn new(sleeper: &'a dyn Sleeper, clock: &'a dyn Clock) -> Self {
        Self {
            sleeper,
            clock,
            credits_refresh: Box::new(|| None),
            cat_travel: CatTravel::Disabled,
            gap: 1.0,
        }
    }
}

/// 单账号上下文：出站身份值 + 编排层判定所需的口径。
///
/// 不在 `growth_api::TaskAccount` 上加字段：那个结构是 4.4 的产出物、已被冻结为
/// 「出站所需的身份值」；`realm` / `is_enterprise` 属于编排层判定（契约 11）。
pub struct RunnerAccount<'a> {
    /// 出站身份值（uid / token / 企业 id / chat 头）。
    pub api: TaskAccount<'a>,
    /// `"cn"` 之外一律按国际版短路。
    pub realm: &'a str,
    /// 昵称；`None` 或空串都回退 `uid[:8]`（对齐 `wb_accounts.py:710`）。
    pub nickname: Option<&'a str>,
    /// 企业账号判定（Python `is_enterprise()` 即 `enterprise_id` 非空）。
    pub is_enterprise: bool,
}

impl<'a> RunnerAccount<'a> {
    /// `nickname or uid[:8]`（按**字符**截断，对齐 Python 切片）。
    pub fn display_name(&self) -> String {
        match self.nickname {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => self.api.uid.chars().take(8).collect(),
        }
    }
}

/// 运行上下文束（内部传递，避免每个函数散装 5 个参数）。
pub struct RunnerCtx<'a> {
    /// 睡。
    pub sleeper: &'a dyn Sleeper,
    /// 现在几点。
    pub clock: &'a dyn Clock,
}

// ---------------------------------------------------------------------------
// `run_growth_tasks` 的返回结构
// ---------------------------------------------------------------------------

/// `run_growth_tasks` 的返回值，键集按 Python 的四档逐字对齐：
///
/// | 场景 | 键 |
/// |---|---|
/// | 国际版 / 企业版 | `ok, msg, logs` |
/// | 拉不到清单 / 接取后为空 | `ok, logs, earned_credit` |
/// | 正常 | `ok, logs, earned_credit, credits` |
#[derive(Debug, Clone, PartialEq)]
pub struct GrowthRunOutcome {
    /// 整体是否视为成功。
    pub ok: bool,
    /// 仅「国际版 / 企业版」档出现。
    pub msg: Option<String>,
    /// 全过程日志（顺序即执行顺序）。
    pub logs: Vec<String>,
    /// 仅「拉不到清单 / 接取后为空 / 正常」三档出现。
    pub earned_credit: Option<i64>,
    /// 仅正常档出现；刷新失败时为 `None`（调用方应保留旧值）。
    pub credits: Option<Value>,
}

impl GrowthRunOutcome {
    /// 序列化为上游/前端口径的 JSON（不输出 `null` 以外的出现档）。
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("ok".into(), json!(self.ok));
        if let Some(m) = &self.msg {
            map.insert("msg".into(), json!(m));
        }
        map.insert("logs".into(), json!(self.logs));
        if let Some(e) = self.earned_credit {
            map.insert("earned_credit".into(), json!(e));
        }
        if let Some(c) = &self.credits {
            map.insert("credits".into(), c.clone());
        }
        Value::Object(map)
    }
}

// ---------------------------------------------------------------------------
// Python 语义小工具
// ---------------------------------------------------------------------------

/// Python 真值判定（用于 `daily_limit_reached`）。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python 的 `d.get("data") or {}`。
fn data_or_empty(v: &Value) -> Value {
    match v.get("data") {
        Some(d) if d.is_object() || d.is_array() => d.clone(),
        _ => json!({}),
    }
}

/// `str(x)` 的近似：字符串原样，其余走 JSON（数字 `200` → `"200"`）。
fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 猫猫旅行：`do_cat_travel`（wb_tasks.py:463-522）
// ---------------------------------------------------------------------------

/// `_travel_status`（`:411-416`）：`GET {chat}/activity/growth/buddy/travel/status`。
///
/// **有 `/buddy/`、无 `/v2`**——与 `events::growth_summary_travel` 的
/// `/v2/activity/growth/buddy/travel/status` 是两条独立路径（R1：不得合并）。
async fn fetch_travel_state(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
) -> Result<Value, String> {
    let url = format!("{}/activity/growth/buddy/travel/status", ep.chat_base);
    let resp = client
        .get_with_timeout(&url, acc.api.chat_headers, TRAVEL_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(data_or_empty(&body))
}

/// `_travel_depart`（`:437-460`）：`POST {chat}/activity/growth/buddy/travel/depart`
/// body `{"location_id": lid}`。返回 `(data, err)`，二者至多一个为 `Some`。
async fn travel_depart(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    lid: &Value,
) -> (Option<Value>, Option<String>) {
    let url = format!("{}/activity/growth/buddy/travel/depart", ep.chat_base);
    let resp = match client
        .post_with_timeout(
            &url,
            acc.api.chat_headers,
            &json!({ "location_id": lid }),
            TRAVEL_TIMEOUT,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return (None, Some(e.to_string())),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Python `_err_detail`: 读 ≤300 字符，优先 `msg`，否则 `HTTP {code}: {detail[:120]}`
        let head: String = text.chars().take(300).collect();
        let msg = match serde_json::from_str::<Value>(&head) {
            Ok(d) => match d.get("msg").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => m.to_string(),
                _ => format!("HTTP {}", status.as_u16()),
            },
            Err(_) => format!(
                "HTTP {}: {}",
                status.as_u16(),
                head.chars().take(120).collect::<String>()
            ),
        };
        return (None, Some(msg));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(d) => {
            if d.get("code").and_then(Value::as_i64) == Some(0) {
                (Some(data_or_empty(&d)), None)
            } else {
                let msg = d
                    .get("msg")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "code={}",
                            d.get("code").map(as_text).unwrap_or_else(|| "None".into())
                        )
                    });
                (None, Some(msg))
            }
        }
        Err(e) => (None, Some(e.to_string())),
    }
}

/// `ensure_buddy`（`:419-435`）：拉清单 → 首个 `first_buddy` → 未领取则推进一次 →
/// 重取 → 判定 `claimed`。
async fn ensure_buddy(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    logs: &mut Vec<String>,
    ctx: &RunnerCtx<'_>,
) -> bool {
    let tasks = fetch_growth_tasks(client, &acc.api, ep).await;
    let Some(fb) = tasks.iter().find(|t| t.task_code == "first_buddy") else {
        return false;
    };
    if fb.status == "claimed" {
        return true;
    }
    let fb = fb.clone();
    let _ = progress_one(client, acc, &fb, logs, ep, ctx, 1.0).await;
    let fresh = fetch_growth_tasks(client, &acc.api, ep).await;
    fresh
        .iter()
        .any(|t| t.task_code == "first_buddy" && t.status == "claimed")
}

/// `do_cat_travel`（`:463-522`）：返回 `{ok, action, credit?, msg}`。
///
/// **不带积分刷新**：等价于 Python 侧的 `account.fetch_credits()` 被省略。
/// 需要与 Python 完全一致的调用方请用 [`build_cat_travel_refreshing`]。
pub async fn build_cat_travel(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    logs: &mut Vec<String>,
    ctx: &RunnerCtx<'_>,
) -> Value {
    build_cat_travel_inner(client, acc, ep, logs, ctx, &mut || None::<Value>).await
}

/// 与 [`build_cat_travel`] 同语义，但在**旅行领奖成功后**刷新一次积分余额
/// —— 对齐 Python `do_cat_travel` 的 `account.fetch_credits()`（`:478`）。
///
/// 闭包形态而非直接调用：`wb-tasks → wb-api` 禁止依赖，积分形状属于 api 侧；
/// 返回 `None` 表示刷新失败（Python 的 `fetch_credits` 失败也不抛）。
pub async fn build_cat_travel_refreshing(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    logs: &mut Vec<String>,
    ctx: &RunnerCtx<'_>,
    credits_refresh: &mut (dyn FnMut() -> Option<Value> + Send),
) -> Value {
    build_cat_travel_inner(client, acc, ep, logs, ctx, credits_refresh).await
}

/// `do_cat_travel` 的实现体（[`build_cat_travel`] / [`build_cat_travel_refreshing`] 共用）。
async fn build_cat_travel_inner(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    logs: &mut Vec<String>,
    ctx: &RunnerCtx<'_>,
    credits_refresh: &mut (dyn FnMut() -> Option<Value> + Send),
) -> Value {
    // 企业版无成长体系：直接 skip。注意 Python 这里仍带 `msg`，
    // 故 `run_growth_tasks` 第 3 步会记一行「猫猫日常: …」，本函数保持同一行为。
    if acc.is_enterprise {
        return json!({"ok": true, "action": "skip", "msg": "企业版账号无成长体系，跳过"});
    }

    let st = match fetch_travel_state(client, acc, ep).await {
        Ok(v) => v,
        Err(exc) => return json!({"ok": false, "msg": format!("查询旅行状态失败: {exc}")}),
    };
    let state = st.get("state").and_then(Value::as_str).unwrap_or("");

    // --- arrived：领奖 ---
    if state == "arrived" {
        let url = format!("{}/activity/growth/buddy/travel/claim", ep.chat_base);
        let resp = match client
            .post_with_timeout(&url, acc.api.chat_headers, &json!({}), TRAVEL_TIMEOUT)
            .await
        {
            Ok(r) => r,
            Err(e) => return json!({"ok": false, "msg": format!("领奖失败: {e}")}),
        };
        if !resp.status().is_success() {
            return json!({
                "ok": false,
                "msg": format!("领奖失败: HTTP {}", resp.status().as_u16())
            });
        }
        return match resp.json::<Value>().await {
            Ok(c_res) => {
                // Python `(c_res.get("data") or {}).get("reward_credit", 0)`
                let credit = c_res
                    .get("data")
                    .and_then(|d| d.get("reward_credit"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // Python `:478` 的 `account.fetch_credits()`：领奖后刷新余额。
                let _ = credits_refresh();
                json!({
                    "ok": true,
                    "action": "claim",
                    "credit": credit,
                    "msg": format!("旅行归来领奖成功！获得 {credit} 积分")
                })
            }
            Err(e) => json!({"ok": false, "msg": format!("领奖失败: {e}")}),
        };
    }

    // --- idle：查 config → depart ---
    if state == "idle" {
        if st.get("daily_limit_reached").is_some_and(truthy) {
            return json!({
                "ok": true,
                "action": "idle",
                "msg": "猫猫今日已完成旅行，明日 00:00 刷新"
            });
        }
        let mut lid: Option<Value> = None;
        let url = format!("{}/activity/growth/buddy/travel/config", ep.chat_base);
        match client
            .get_with_timeout(&url, acc.api.chat_headers, TRAVEL_TIMEOUT)
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                Ok(cfg) => {
                    // Python `locs = (cfg.get("data") or {}).get("locations") or []`
                    lid = cfg
                        .get("data")
                        .and_then(|d| d.get("locations"))
                        .and_then(Value::as_array)
                        .and_then(|l| l.first())
                        .and_then(|l0| l0.get("id"))
                        .cloned();
                }
                // Python 只在 `_log(...)` 记一笔，不影响后续
                Err(exc) => logs.push(format!("查询旅行地点失败: {exc}（可忽略）")),
            },
            Ok(resp) => logs.push(format!(
                "查询旅行地点失败: HTTP {}（可忽略）",
                resp.status().as_u16()
            )),
            Err(exc) => logs.push(format!("查询旅行地点失败: {exc}（可忽略）")),
        }
        let lid = lid.unwrap_or_else(default_location_id);
        let (mut data, mut err) = travel_depart(client, acc, ep, &lid).await;

        // 无 Buddy：先补齐 first_buddy 再重试一次
        let no_buddy = err
            .as_deref()
            .is_some_and(|e| e.to_lowercase().contains("no active buddy"));
        if data.is_none() && no_buddy {
            if ensure_buddy(client, acc, ep, logs, ctx).await {
                (data, err) = travel_depart(client, acc, ep, &lid).await;
            } else {
                return json!({
                    "ok": false,
                    "msg": "还没有 Buddy：需先完成并领取「领取一只 Buddy」任务（新建任务并发起对话）"
                });
            }
        }

        if let Some(d) = data {
            let loc = d
                .get("location")
                .and_then(|l| l.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return json!({
                "ok": true,
                "action": "depart",
                "msg": format!("猫猫已出发前往「{loc}」，预计数小时后归来！")
            });
        }
        return json!({
            "ok": false,
            "msg": format!("派出旅行失败: {}", err.unwrap_or_default())
        });
    }

    // --- traveling / 未知状态 ---
    if state == "traveling" {
        return json!({"ok": true, "action": "traveling", "msg": "猫猫正在旅行途中，请稍后再来查看！"});
    }
    json!({"ok": true, "action": state, "msg": format!("当前状态: {state}")})
}

// ---------------------------------------------------------------------------
// `_progress_one`（wb_tasks.py:525-631）
// ---------------------------------------------------------------------------

/// 推进单个任务并返回本次到账积分。**七条分支顺序不可重排**（契约 8）：
///
/// ① 未知 spec / `unforgeable` → 0
/// ② `claimed` → 0
/// ③ `completed` 或 `cur >= tgt` → 先领奖
/// ④ `DESKTOP_ONLY_TASKS` → 给深链跳过
/// ⑤ 夜猫子窗口外 → 跳过
/// ⑥ `not_accepted` 且非 `auto` → 跳过
/// ⑦ 上报 → 轮询（≤6 次）→ 领奖
///
/// 事件上报固定复用 `acc.api.chat_headers`（与领奖/拉取同源），故不单列参数。
pub async fn progress_one(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    t: &GrowthTask,
    logs: &mut Vec<String>,
    ep: &TaskEndpoints<'_>,
    ctx: &RunnerCtx<'_>,
    gap: f64,
) -> i64 {
    let code = t.task_code.as_str();
    let spec = task_spec(code);

    // ① 未知 spec 或 unforgeable → 0；只有带 reason 的才记一行
    if spec.is_none() || spec.is_some_and(|s| s.unforgeable) {
        if let Some(s) = spec.filter(|s| !s.reason.is_empty()) {
            logs.push(format!(
                "⏭ 任务 [{}] 需真实操作完成: {}, 跳过",
                s.name, s.reason
            ));
        }
        return 0;
    }
    let spec = spec.expect("spec was checked above");
    let name = spec.name;

    // ② claimed → 0（必须早于 ④：已领奖的桌面端任务不该再提示深链，R2）
    if t.status == "claimed" {
        return 0;
    }

    // ③ 已完成或进度已满 → 先领奖（桌面端/夜间任务也可能被真实操作完成）
    if t.status == "completed" || t.current >= t.target {
        let res = claim_task(client, &acc.api, code, ep).await;
        if res.ok {
            logs.push(format!("✓ 任务 [{name}] 领奖成功: +{} 积分", res.credit));
            ctx.sleeper.sleep(gap).await;
            return res.credit;
        }
        let why = res.msg.unwrap_or_else(|| "未知原因".into());
        logs.push(format!("! 任务 [{name}] 领奖失败: {why}"));
        ctx.sleeper.sleep(gap).await;
        return 0;
    }

    // ④ 桌面端唯一真实行为 → 诚实跳过并给深链
    if let Some((_, why)) = crate::DESKTOP_ONLY_TASKS.iter().find(|(c, _)| *c == code) {
        let jump = if t.jump_url.is_empty() {
            "workbuddy://chat"
        } else {
            t.jump_url.as_str()
        };
        logs.push(format!(
            "⏭ 任务 [{name}] 需真实操作完成: {why} (深链 {jump}), 跳过事件伪造"
        ));
        return 0;
    }

    // ⑤ 夜猫子窗口外 → 0
    if crate::NIGHT_TASK_CODES.contains(&code) && !in_night_window(ctx.clock.local_hour()) {
        logs.push(format!(
            "🌙 任务 [{name}] 仅 23:00-08:00 上报计数, 当前不在窗口, 跳过 (每日 01:00 调度器自动执行, 累计 3 天)"
        ));
        return 0;
    }

    // ⑥ 仍未接取（非 auto）→ 0
    if t.status == "not_accepted" && t.task_type != "auto" {
        logs.push(format!("⏭ 任务 [{name}] 仍未接取, 跳过 (先解决接取失败)"));
        return 0;
    }

    // ⑦ 点亮上报
    let need = std::cmp::max(1, t.target - t.current);
    logs.push(format!("正在点亮任务 [{name}] (需上报 {need} 次)..."));
    let mut report_ok = true;
    for i in 0..need {
        let expert = rotating_expert(spec.kind, t.current, i);
        let now_ms = now_ms();
        let ev = build_event(acc.api.uid, spec.kind, i as usize, now_ms, expert);
        if !report_events(client, ep.chat_base, acc.api.chat_headers, &[ev]).await {
            report_ok = false;
        }
        if i < need - 1 {
            ctx.sleeper.sleep(gap).await;
        }
    }
    if !report_ok {
        logs.push(format!(
            "! 任务 [{name}] 部分事件上报失败 (上游拒绝), 继续尝试领奖"
        ));
    }
    ctx.sleeper.sleep(1.5).await;

    // 契约 9：最多 6 次轮询；进度在涨就继续等，连续两次卡住就放弃
    let mut prog = t.current;
    for attempt in 0..6 {
        let fresh = fetch_growth_tasks(client, &acc.api, ep).await;
        if let Some(ft) = fresh.iter().find(|x| x.task_code == code) {
            prog = ft.current;
            if prog >= t.target || ft.status == "completed" || ft.status == "claimed" {
                break;
            }
            if prog > t.current {
                ctx.sleeper.sleep(2.5).await;
                continue;
            }
        }
        if attempt < 2 {
            ctx.sleeper.sleep(1.5).await;
        } else {
            break;
        }
    }
    if prog < t.target {
        logs.push(format!(
            "? 任务 [{name}] 已上报但进度 {prog}/{} 未达成, 领奖顺延到下次运行",
            t.target
        ));
        ctx.sleeper.sleep(gap).await;
        return 0;
    }

    let res = claim_task(client, &acc.api, code, ep).await;
    if res.ok {
        logs.push(format!(
            "✓ 任务 [{name}] 点亮并领奖成功: +{} 积分",
            res.credit
        ));
        ctx.sleeper.sleep(gap).await;
        return res.credit;
    }
    let why = res.msg.unwrap_or_else(|| "未知原因".into());
    logs.push(format!(
        "! 任务 [{name}] 进度已达 {prog}/{} 但领奖失败: {why}",
        t.target
    ));
    ctx.sleeper.sleep(gap).await;
    0
}

/// 当前毫秒时间戳（事件 id 用）。
///
/// `pub(crate)`：`night_growth` 模块的猫猫事件 id 复用同一时间源
/// （Python `build_event` 的 `int(time.time() * 1000)`）。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// `run_growth_tasks`（wb_tasks.py:634-709）
// ---------------------------------------------------------------------------

/// `_needs_accept`（`:659-661`）：`not_accepted && !unforgeable && task_type != "auto"`。
fn needs_accept(t: &GrowthTask) -> bool {
    t.status == "not_accepted" && !t.unforgeable && t.task_type != "auto"
}

/// 一轮完整跑：auto → 接取（含二次）→ 逐个推进 → 猫猫旅行 → 刷新积分。
pub async fn run_growth_tasks(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    env: &RunnerEnv<'_>,
    deps: RunnerDeps<'_>,
) -> GrowthRunOutcome {
    // 契约 11：国际版 ok:false，企业版 ok:true + 跳过；两者不可混同，且都不打日志
    if acc.realm != "cn" {
        return GrowthRunOutcome {
            ok: false,
            msg: Some("国际版不适用国内成长任务中心".into()),
            logs: Vec::new(),
            earned_credit: None,
            credits: None,
        };
    }
    if acc.is_enterprise {
        return GrowthRunOutcome {
            ok: true,
            msg: Some("企业版账号无成长体系，跳过".into()),
            logs: Vec::new(),
            earned_credit: None,
            credits: None,
        };
    }

    let ctx = RunnerCtx {
        sleeper: deps.sleeper,
        clock: deps.clock,
    };
    let ep = &env.ep;
    let gap = deps.gap;

    let mut logs = vec![format!(
        "开始为账号 {} 运行成长任务自动化...",
        acc.display_name()
    )];
    let mut tasks = fetch_growth_tasks(client, &acc.api, ep).await;
    if tasks.is_empty() {
        logs.push("未能获取到任务清单，请检查网络或账号状态".into());
        return GrowthRunOutcome {
            ok: false,
            msg: None,
            logs,
            earned_credit: Some(0),
            credits: None,
        };
    }

    let mut total_earned = 0i64;

    // 0. auto 类先跑（first_buddy 必须先领，否则 single 接取会报
    //    prerequisite not met: first_buddy）
    let auto_tasks: Vec<GrowthTask> = tasks
        .iter()
        .filter(|t| t.task_type == "auto")
        .cloned()
        .collect();
    for t in &auto_tasks {
        total_earned += progress_one(client, acc, t, &mut logs, ep, &ctx, gap).await;
    }

    // 1. 批量接取 → 复核 → 二次接取
    let unaccepted: Vec<String> = tasks
        .iter()
        .filter(|t| needs_accept(t))
        .map(|t| t.task_code.clone())
        .collect();
    if !unaccepted.is_empty() {
        logs.push(format!(
            "发现 {} 个待接取任务，正在批量接取...",
            unaccepted.len()
        ));
        let acc_res = accept_tasks(client, &acc.api, &unaccepted, 20, ep).await;
        if !acc_res.failed.is_empty() {
            // `msg` 是空串而非 None；Python `res.get("msg") or '; '.join(reasons)[:160]`
            let raw_why = if acc_res.msg.is_empty() {
                None
            } else {
                Some(acc_res.msg.clone())
            };
            let why = raw_why.or_else(|| {
                let joined = acc_res.reasons.join("; ");
                let cut = truncate_chars(&joined, 160);
                if cut.is_empty() {
                    None
                } else {
                    Some(cut)
                }
            });
            let shown: Vec<&str> = acc_res.failed.iter().take(5).map(String::as_str).collect();
            let mut line = format!(
                "! 接取未成功 {} 个: {}",
                acc_res.failed.len(),
                shown.join(", ")
            );
            if acc_res.failed.len() > 5 {
                line.push_str(" ...");
            }
            if let Some(w) = why {
                line.push_str(&format!(" ({w})"));
            }
            logs.push(line);
        }
        if !acc_res.accepted.is_empty() {
            logs.push(format!("✓ 已接取 {} 个任务", acc_res.accepted.len()));
        }
        ctx.sleeper.sleep(gap).await;
        tasks = fetch_growth_tasks(client, &acc.api, ep).await;

        let still: Vec<String> = tasks
            .iter()
            .filter(|t| needs_accept(t))
            .map(|t| t.task_code.clone())
            .collect();
        if !still.is_empty() {
            logs.push(format!("仍有 {} 个未接取，重试接取一次...", still.len()));
            let retry = accept_tasks(client, &acc.api, &still, 20, ep).await;
            if !retry.accepted.is_empty() {
                logs.push(format!("✓ 重试接取成功 {} 个", retry.accepted.len()));
            }
            ctx.sleeper.sleep(gap).await;
            tasks = fetch_growth_tasks(client, &acc.api, ep).await;
        }

        if tasks.is_empty() {
            logs.push("! 接取后无法获取任务清单，本轮中止".into());
            return GrowthRunOutcome {
                ok: false,
                msg: None,
                logs,
                earned_credit: Some(total_earned),
                credits: None,
            };
        }
        let still_pending = tasks.iter().filter(|t| needs_accept(t)).count();
        if still_pending > 0 {
            logs.push(format!(
                "! 仍有 {still_pending} 个任务处于未接取状态，对未接取任务上报事件不会计入进度，本轮跳过这些任务"
            ));
        }
    }

    // 2. 逐个推进非 auto 任务
    let non_auto: Vec<GrowthTask> = tasks
        .iter()
        .filter(|t| t.task_type != "auto")
        .cloned()
        .collect();
    for t in &non_auto {
        total_earned += progress_one(client, acc, t, &mut logs, ep, &ctx, gap).await;
    }

    // 3. 猫猫旅行（顺手检查；积分计入本轮累计）
    let tr_res = match deps.cat_travel {
        CatTravel::Disabled => Value::Null,
        CatTravel::Run(runner) => runner.run(client, acc, ep, &mut logs, &ctx).await,
    };
    if let Some(msg) = tr_res.get("msg").and_then(Value::as_str) {
        if !msg.is_empty() {
            logs.push(format!("猫猫日常: {msg}"));
        }
    }
    if let Some(credit) = tr_res.get("credit").and_then(Value::as_i64) {
        total_earned += credit;
    }

    // 4. 刷新积分（失败 → 不改 credits，对齐 Python `fetch_credits` 失败分支）
    let credits = (deps.credits_refresh)();
    let remain = credits
        .as_ref()
        .and_then(|c| c.get("remain"))
        .map(as_text)
        .unwrap_or_else(|| "0".into());
    logs.push(format!(
        "🎉 全部完成！本次累计新增到账: +{total_earned} 积分，当前总剩余: {remain} 积分"
    ));

    GrowthRunOutcome {
        ok: true,
        msg: None,
        logs,
        earned_credit: Some(total_earned),
        credits,
    }
}

/// 按字符截断（不切坏 UTF-8）。
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---- 测试夹具 ----

    struct Ids {
        uid: String,
        token: String,
        enterprise_id: String,
        chat_headers: HashMap<String, String>,
        realm: String,
        nickname: Option<String>,
        is_enterprise: bool,
    }

    impl Ids {
        fn cn() -> Self {
            let mut h = HashMap::new();
            h.insert("X-Domain".into(), "copilot.tencent.com".into());
            Ids {
                uid: "u-test".into(),
                token: "tok-test".into(),
                enterprise_id: String::new(),
                chat_headers: h,
                realm: "cn".into(),
                nickname: None,
                is_enterprise: false,
            }
        }

        fn acc(&self) -> RunnerAccount<'_> {
            RunnerAccount {
                api: TaskAccount {
                    uid: &self.uid,
                    token: &self.token,
                    enterprise_id: &self.enterprise_id,
                    chat_headers: &self.chat_headers,
                },
                realm: &self.realm,
                nickname: self.nickname.as_deref(),
                is_enterprise: self.is_enterprise,
            }
        }
    }

    /// 把 chat/web base 都指到同一个 wiremock（泄漏一次字符串换 `'static`；
    /// 每测试一次，可接受）。**绝不向真实上游发请求。**
    fn ep(server: &MockServer) -> TaskEndpoints<'static> {
        let base: &'static str = Box::leak(server.uri().into_boxed_str());
        TaskEndpoints {
            chat_base: base,
            web_base: base,
        }
    }

    struct FixedClock(u32);
    impl Clock for FixedClock {
        fn local_hour(&self) -> u32 {
            self.0
        }
    }

    fn task(code: &str, status: &str, ty: &str, current: i64, target: i64) -> GrowthTask {
        GrowthTask {
            task_code: code.into(),
            name: String::new(),
            description: String::new(),
            jump_url: String::new(),
            status: status.into(),
            task_type: ty.into(),
            current,
            target,
            reward_credit: 0,
            reward_energy: 0,
            unforgeable: false,
            reason: String::new(),
        }
    }

    /// 注入式猫猫旅行：返回固定 JSON。
    struct FakeCat(Value);
    impl CatTravelRunner for FakeCat {
        fn run<'a>(
            &'a self,
            _c: &'a UpstreamClient,
            _a: &'a RunnerAccount<'_>,
            _e: &'a TaskEndpoints<'_>,
            _l: &'a mut Vec<String>,
            _ctx: &'a RunnerCtx<'_>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
            let v = self.0.clone();
            Box::pin(async move { v })
        }
    }

    fn deps<'a>(
        sleeper: &'a NoopSleeper,
        clock: &'a FixedClock,
        credits: Option<Value>,
        cat: CatTravel<'a>,
    ) -> RunnerDeps<'a> {
        RunnerDeps {
            sleeper,
            clock,
            credits_refresh: Box::new(move || credits),
            cat_travel: cat,
            gap: 1.0,
        }
    }

    fn map_of(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        for (k, v) in pairs {
            m.insert((*k).into(), v.clone());
        }
        m
    }

    // ---- 契约 11：国际版 / 企业版短路 ----

    #[tokio::test]
    async fn intl_realm_short_circuits_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let ids = Ids {
            realm: "intl".into(),
            ..Ids::cn()
        };
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(!out.ok);
        assert_eq!(out.msg.as_deref(), Some("国际版不适用国内成长任务中心"));
        assert!(out.logs.is_empty());
        assert_eq!(out.earned_credit, None);
        assert_eq!(out.credits, None);
        // 键集：ok / msg / logs
        assert_eq!(
            out.to_json(),
            json!({"ok": false, "msg": "国际版不适用国内成长任务中心", "logs": []})
        );
    }

    #[tokio::test]
    async fn enterprise_short_circuits_ok_true_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let ids = Ids {
            is_enterprise: true,
            enterprise_id: "ent-1".into(),
            ..Ids::cn()
        };
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(out.ok);
        assert_eq!(out.msg.as_deref(), Some("企业版账号无成长体系，跳过"));
        assert!(out.logs.is_empty());
        assert_eq!(
            out.to_json(),
            json!({"ok": true, "msg": "企业版账号无成长体系，跳过", "logs": []})
        );
    }

    /// 国际版与企业版不可混同：国际版 `ok:false`，企业版 `ok:true`。
    #[test]
    fn realm_and_enterprise_are_distinguishable() {
        assert_ne!(
            json!({"ok": false, "msg": "国际版不适用国内成长任务中心", "logs": []}),
            json!({"ok": true, "msg": "企业版账号无成长体系，跳过", "logs": []})
        );
    }

    // ---- 契约 8-②：claimed 不触发 desktop-only 判定（R2 钉住顺序） ----

    #[tokio::test]
    async fn claimed_desktop_only_task_never_emits_deep_link_hint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .expect(0)
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        // RichMeow_Chat 在 DESKTOP_ONLY_TASKS 中
        let t = task("RichMeow_Chat", "claimed", "single", 0, 1);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert!(logs.is_empty(), "logs = {logs:?}");
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            0,
            "claimed 分支必须先短路，不得向上游发 claim"
        );
    }

    // ---- 契约 8-④：桌面端任务给深链 ----

    #[tokio::test]
    async fn desktop_only_task_logs_deep_link_and_skips() {
        let server = MockServer::start().await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let mut t = task("RichMeow_Chat", "in_progress", "single", 0, 1);
        t.jump_url = "workbuddy://chat/x".into();
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert_eq!(logs.len(), 1);
        assert!(
            logs[0].contains("需真实操作完成") && logs[0].contains("深链 workbuddy://chat/x"),
            "logs = {logs:?}"
        );
        assert!(logs[0].ends_with("跳过事件伪造"), "logs = {logs:?}");
    }

    #[tokio::test]
    async fn desktop_only_task_without_jump_url_defaults_to_workbuddy_chat() {
        let server = MockServer::start().await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("RichMeow_Chat", "in_progress", "single", 0, 1);
        let mut logs = Vec::new();
        progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert!(logs[0].contains("深链 workbuddy://chat"), "logs = {logs:?}");
    }

    // ---- 契约 8-⑤：夜猫子窗口 ----

    #[tokio::test]
    async fn night_task_outside_window_skips_with_moon_icon() {
        let server = MockServer::start().await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(12);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("black_cat", "in_progress", "single", 0, 3);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert!(logs[0].starts_with("🌙 任务"), "logs = {logs:?}");
    }

    // ---- 契约 8-⑥：未接取 ----

    #[tokio::test]
    async fn not_accepted_non_auto_task_skips() {
        let server = MockServer::start().await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "not_accepted", "single", 0, 5);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert!(logs[0].contains("仍未接取"), "logs = {logs:?}");
    }

    // ---- 契约 8-①：unforgeable ----

    #[tokio::test]
    async fn unforgeable_task_logs_reason_and_returns_zero() {
        let server = MockServer::start().await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        // 找一个 unforgeable 的 code（从 TASK_SPECS 里取），保证与真实表一致
        let code = crate::TASK_SPECS
            .iter()
            .find(|(_, s)| s.unforgeable && !s.reason.is_empty())
            .map(|(c, _)| *c)
            .expect("至少有一个带 reason 的 unforgeable 任务");
        let t = task(code, "in_progress", "single", 0, 1);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert!(logs[0].starts_with("⏭ 任务 ["), "logs = {logs:?}");
        assert!(logs[0].contains("需真实操作完成"), "logs = {logs:?}");
    }

    /// 未知 code：静默返回 0（Python 只在 `spec and spec.get("reason")` 时记日志）。
    #[tokio::test]
    async fn unknown_task_code_is_silent() {
        let server = MockServer::start().await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("no_such_code", "in_progress", "single", 0, 1);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert!(logs.is_empty(), "logs = {logs:?}");
    }

    // ---- 契约 8-③：已完成先领奖 ----

    #[tokio::test]
    async fn completed_task_claims_first_and_returns_credit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/template_5/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 200}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "completed", "single", 5, 5);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 200);
        assert_eq!(logs[0], "✓ 任务 [模板创建任务] 领奖成功: +200 积分");
    }

    /// `cur >= tgt` 但 status 仍是 in_progress：同样先领奖。
    #[tokio::test]
    async fn current_reaching_target_claims_even_when_status_in_progress() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/template_5/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 200}})),
            )
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "in_progress", "single", 5, 5);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 200);
    }

    /// 领奖失败：返回 0 且日志以 `!` 开头、带上游原因。
    #[tokio::test]
    async fn claim_failure_logs_bang_and_returns_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/template_5/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 1, "msg": "already claimed"})),
            )
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "completed", "single", 5, 5);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert_eq!(logs[0], "! 任务 [模板创建任务] 领奖失败: already claimed");
    }

    // ---- 契约 9：上报 → 轮询 → 领奖 ----

    #[tokio::test]
    async fn report_then_poll_then_claim_happy_path() {
        let server = MockServer::start().await;
        // 第一次拉取：current=0、in_progress
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "template_5",
                    "accept_status": "in_progress",
                    "progress": {"current": 0, "target": 5}
                }]}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // 之后：completed / current=5
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "template_5",
                    "accept_status": "completed",
                    "progress": {"current": 5, "target": 5}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/report"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .expect(5)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/template_5/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 200}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "in_progress", "single", 0, 5);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 200);
        assert_eq!(
            logs,
            vec![
                "正在点亮任务 [模板创建任务] (需上报 5 次)...".to_string(),
                "✓ 任务 [模板创建任务] 点亮并领奖成功: +200 积分".to_string(),
            ]
        );
    }

    /// 契约 9：进度始终未达标 → 返回 0、日志用 `?`（顺延，不是失败）、不领奖。
    #[tokio::test]
    async fn progress_never_reaching_target_defers_instead_of_failing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "template_5",
                    "accept_status": "in_progress",
                    "progress": {"current": 0, "target": 5}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/report"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/template_5/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .expect(0)
            .mount(&server)
            .await;

        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "in_progress", "single", 0, 5);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 0);
        assert!(
            logs.iter()
                .any(|l| l.starts_with('?') && l.contains("已上报但进度 0/5 未达成")),
            "logs = {logs:?}"
        );
        assert!(
            logs.iter().all(|l| !l.contains("点亮并领奖成功")),
            "logs = {logs:?}"
        );
        // 恒 0 时：attempt 0、1 各睡 1.5s，attempt 2 起直接 break ⇒ 共 3 次 1.5s
        let sleeps = sleeper.calls();
        let one_five = sleeps.iter().filter(|s| **s == 1.5).count();
        assert_eq!(one_five, 3, "sleeps = {sleeps:?}");
        let claims: Vec<_> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path().contains("/claim"))
            .collect();
        assert!(claims.is_empty(), "进度未达标不得领奖");
    }

    /// 上报被上游拒绝：记 `!` 但不中止，仍继续轮询/领奖。
    #[tokio::test]
    async fn report_rejection_logs_warning_but_still_polls_and_claims() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/report"))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "template_5",
                    "accept_status": "completed",
                    "progress": {"current": 5, "target": 5}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/template_5/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 200}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = RunnerCtx {
            sleeper: &sleeper,
            clock: &clock,
        };
        let t = task("template_5", "in_progress", "single", 0, 1);
        let mut logs = Vec::new();
        let got = progress_one(
            &UpstreamClient::new(),
            &acc,
            &t,
            &mut logs,
            &ep(&server),
            &ctx,
            1.0,
        )
        .await;
        assert_eq!(got, 200);
        assert!(
            logs.iter()
                .any(|l| l.contains("部分事件上报失败 (上游拒绝), 继续尝试领奖")),
            "logs = {logs:?}"
        );
    }

    // ---- 契约 10：run_growth_tasks 编排 ----

    #[tokio::test]
    async fn empty_task_list_reports_network_hint_and_zero_earned() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(!out.ok);
        assert_eq!(out.earned_credit, Some(0));
        assert_eq!(out.msg, None);
        assert_eq!(out.credits, None);
        assert_eq!(
            out.logs,
            vec![
                "开始为账号 u-test 运行成长任务自动化...".to_string(),
                "未能获取到任务清单，请检查网络或账号状态".to_string(),
            ]
        );
        // 键集：ok / logs / earned_credit（无 msg / credits）
        assert_eq!(
            out.to_json(),
            json!({"ok": false, "logs": out.logs, "earned_credit": 0})
        );
    }

    #[test]
    fn display_name_prefers_nickname_then_uid_prefix() {
        let mut ids = Ids::cn();
        ids.uid = "abcdefghijkl".into();
        assert_eq!(ids.acc().display_name(), "abcdefgh");
        ids.nickname = Some("昵称".into());
        assert_eq!(ids.acc().display_name(), "昵称");
        ids.nickname = Some(String::new());
        assert_eq!(ids.acc().display_name(), "abcdefgh");
    }

    /// 第 0 步：auto 先跑、先领奖，且**先于**接取。
    #[tokio::test]
    async fn auto_tasks_run_before_accept_and_contribute_credit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "create_canvas",
                    "accept_status": "completed",
                    "task_type": "auto",
                    "progress": {"current": 1, "target": 1}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/create_canvas/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 300}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(
                &sleeper,
                &clock,
                Some(json!({"remain": 999})),
                CatTravel::Run(&FakeCat(
                    json!({"ok": true, "action": "traveling", "msg": "猫猫正在旅行途中，请稍后再来查看！"}),
                )),
            ),
        )
        .await;
        assert!(out.ok, "logs = {:?}", out.logs);
        assert_eq!(out.earned_credit, Some(300));
        assert_eq!(out.credits, Some(json!({"remain": 999})));
        assert!(out.logs.iter().any(|l| l.contains("+300 积分")));
        assert_eq!(
            out.logs.last().map(String::as_str),
            Some("🎉 全部完成！本次累计新增到账: +300 积分，当前总剩余: 999 积分")
        );
    }

    /// 第 0 步的 auto 任务只跑一次（Rust 不复现 Python 的二次执行缺陷）。
    #[tokio::test]
    async fn auto_task_runs_exactly_once_without_unaccepted_tasks() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "create_canvas",
                    "accept_status": "completed",
                    "task_type": "auto",
                    "progress": {"current": 1, "target": 1}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/create_canvas/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 300}})),
            )
            .expect(1) // 只 1 次：Python 在「无待接取」时会重复执行 auto（缺陷）
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(out.ok);
        assert_eq!(out.earned_credit, Some(300));
    }

    /// 第 1 步：接取 → 复核 → 二次接取；残余只警告不中止；第 2 步跳过。
    #[tokio::test]
    async fn accept_retries_once_and_warns_on_remaining_pending() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "template_5",
                    "accept_status": "not_accepted",
                    "task_type": "single",
                    "progress": {"current": 0, "target": 5}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/activity/growth/tasks/accept"))
            .and(body_json(json!({"task_codes": ["template_5"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"results": [{"task_code": "template_5", "status": "accepted"}]}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(out.ok, "logs = {:?}", out.logs);
        assert!(out
            .logs
            .iter()
            .any(|l| l == "发现 1 个待接取任务，正在批量接取..."));
        assert!(out.logs.iter().any(|l| l == "✓ 已接取 1 个任务"));
        assert!(out
            .logs
            .iter()
            .any(|l| l == "仍有 1 个未接取，重试接取一次..."));
        assert!(out.logs.iter().any(|l| l == "✓ 重试接取成功 1 个"));
        assert!(out
            .logs
            .iter()
            .any(|l| l.contains("仍有 1 个任务处于未接取状态")));
        assert!(out.logs.iter().any(|l| l.contains("仍未接取, 跳过")));
    }

    /// 接取后清单为空 → 本轮中止：`ok:false`、带已累计积分、无 credits。
    #[tokio::test]
    async fn empty_list_after_accept_aborts_with_earned_so_far() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "template_5",
                    "accept_status": "not_accepted",
                    "task_type": "single",
                    "progress": {"current": 0, "target": 5}
                }]}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/activity/growth/tasks/accept"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"results": [{"task_code": "template_5", "status": "accepted"}]}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(!out.ok);
        assert!(out
            .logs
            .iter()
            .any(|l| l == "! 接取后无法获取任务清单，本轮中止"));
        assert_eq!(out.earned_credit, Some(0));
        assert_eq!(out.credits, None);
    }

    /// 第 3 步猫猫旅行积分计入累计；第 4 步刷新失败 → `credits: None`、remain 回显 0。
    #[tokio::test]
    async fn travel_credit_counts_and_refresh_failure_keeps_old_credits() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "create_canvas",
                    "accept_status": "completed",
                    "task_type": "auto",
                    "progress": {"current": 1, "target": 1}
                }]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/create_canvas/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 300}})),
            )
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(
                &sleeper,
                &clock,
                None, // 刷新失败
                CatTravel::Run(&FakeCat(json!({
                    "ok": true, "action": "claim", "credit": 60,
                    "msg": "旅行归来领奖成功！获得 60 积分"
                }))),
            ),
        )
        .await;
        assert!(out.ok);
        assert_eq!(out.earned_credit, Some(360));
        assert_eq!(out.credits, None);
        assert!(out
            .logs
            .iter()
            .any(|l| l == "猫猫日常: 旅行归来领奖成功！获得 60 积分"));
        assert_eq!(
            out.logs.last().map(String::as_str),
            Some("🎉 全部完成！本次累计新增到账: +360 积分，当前总剩余: 0 积分")
        );
    }

    /// 猫猫旅行 `Disabled` 时第 3 步整体跳过（无「猫猫日常」行）。
    #[tokio::test]
    async fn disabled_cat_travel_emits_no_cat_line() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [{
                    "task_code": "create_canvas",
                    "accept_status": "claimed",
                    "task_type": "auto",
                    "progress": {"current": 1, "target": 1}
                }]}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        assert!(out.logs.iter().all(|l| !l.starts_with("猫猫日常:")));
    }

    /// 第 2 步：非 auto 任务的推进顺序与清单顺序一致。
    #[tokio::test]
    async fn non_auto_tasks_are_processed_in_list_order() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"tasks": [
                    {"task_code": "template_5", "accept_status": "claimed", "task_type": "single",
                     "progress": {"current": 5, "target": 5}},
                    {"task_code": "RichMeow_Chat", "accept_status": "in_progress", "task_type": "single",
                     "progress": {"current": 0, "target": 1}}
                ]}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let out = run_growth_tasks(
            &UpstreamClient::new(),
            &acc,
            &RunnerEnv { ep: ep(&server) },
            deps(&sleeper, &clock, None, CatTravel::Disabled),
        )
        .await;
        // 1 开头 + template_5 = claimed（静默）+ 桌面端深链 1 条 + 末尾总结 1 条
        assert_eq!(out.logs.len(), 3, "logs = {:?}", out.logs);
        assert!(out.logs[1].contains("深链"), "logs = {:?}", out.logs);
        assert!(out.logs[2].starts_with('🎉'), "logs = {:?}", out.logs);
    }

    // ---- 猫猫旅行（build_cat_travel） ----

    fn cat_ctx<'a>(s: &'a NoopSleeper, c: &'a FixedClock) -> RunnerCtx<'a> {
        RunnerCtx {
            sleeper: s,
            clock: c,
        }
    }

    #[tokio::test]
    async fn cat_travel_status_failure_reports_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["ok"], false);
        assert_eq!(res["msg"], "查询旅行状态失败: HTTP 500");
    }

    #[tokio::test]
    async fn cat_travel_idle_uses_first_config_location() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "idle"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"locations": [{"id": 7}, {"id": 9}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/buddy/travel/depart"))
            .and(body_json(json!({"location_id": 7})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"location": {"name": "海边"}}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["ok"], true);
        assert_eq!(res["action"], "depart");
        assert_eq!(res["msg"], "猫猫已出发前往「海边」，预计数小时后归来！");
    }

    /// config 拿不到 locations → 回退 `location_id = 1`。
    #[tokio::test]
    async fn cat_travel_idle_falls_back_to_location_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "idle"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/buddy/travel/depart"))
            .and(body_json(json!({"location_id": 1})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"location": {"name": "公园"}}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["action"], "depart");
    }

    /// `no active buddy` + 补不上 `first_buddy` → 可操作的提示。
    #[tokio::test]
    async fn cat_travel_missing_buddy_without_first_buddy_reports_actionable_msg() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "idle"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/buddy/travel/depart"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"msg": "no active buddy"})),
            )
            .mount(&server)
            .await;
        // ensure_buddy 拉的清单里没有 first_buddy
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"tasks": []}})),
            )
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["ok"], false);
        assert_eq!(
            res["msg"],
            "还没有 Buddy：需先完成并领取「领取一只 Buddy」任务（新建任务并发起对话）"
        );
        assert!(logs.is_empty(), "logs = {logs:?}");
    }

    #[tokio::test]
    async fn cat_travel_arrived_claims_and_reports_credit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"state": "arrived"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/buddy/travel/claim"))
            .and(body_json(json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"reward_credit": 30}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["ok"], true);
        assert_eq!(res["action"], "claim");
        assert_eq!(res["credit"], 30);
        assert_eq!(res["msg"], "旅行归来领奖成功！获得 30 积分");
    }

    /// R1：`_travel_status` 的路径**逐字**必须是
    /// `/activity/growth/buddy/travel/status`（**有** `/buddy/`、**无** `/v2`）。
    ///
    /// 此前本模块写成了 `/activity/growth/travel/status`（漏 `/buddy/`），
    /// 且旧单测把同一错误路径当作期望值——**测试跟着实现错，等于没测**。
    /// 这里直接断言 wiremock **实际收到的路径**，防止再漂。
    #[tokio::test]
    async fn travel_status_uses_the_exact_buddy_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"state": "traveling"}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["ok"], true);
        assert_eq!(res["action"], "traveling");

        let requests = server.received_requests().await.unwrap();
        let paths: Vec<&str> = requests.iter().map(|r| r.url.path()).collect();
        assert_eq!(
            paths,
            vec!["/activity/growth/buddy/travel/status"],
            "旅行状态路径必须逐字等于 Python 的 /activity/growth/buddy/travel/status"
        );
    }

    /// Python `do_cat_travel` 领奖成功后调 `account.fetch_credits()`（`:478`）。
    /// [`build_cat_travel_refreshing`] 必须真的调一次刷新闭包。
    #[tokio::test]
    async fn cat_travel_refreshing_calls_credits_refresh_after_claim() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"state": "arrived"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/buddy/travel/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"reward_credit": 12}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let mut refreshed = 0usize;
        {
            let mut refresh = || {
                refreshed += 1;
                Some(json!({"remain": 100}))
            };
            let res = build_cat_travel_refreshing(
                &UpstreamClient::new(),
                &acc,
                &ep(&server),
                &mut logs,
                &ctx,
                &mut refresh,
            )
            .await;
            assert_eq!(res["ok"], true);
            assert_eq!(res["credit"], 12);
        }
        assert_eq!(refreshed, 1, "领奖成功后必须刷新一次积分余额");
    }

    /// 旅行四个出站调用的时限常量：Python `wb_tasks.py` 四处均为 `timeout=10`。
    #[test]
    fn travel_timeout_matches_python_ten_seconds() {
        assert_eq!(TRAVEL_TIMEOUT, std::time::Duration::from_secs(10));
    }

    #[tokio::test]
    async fn cat_travel_idle_with_daily_limit_does_not_depart() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"state": "idle", "daily_limit_reached": true}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["action"], "idle");
        assert_eq!(res["msg"], "猫猫今日已完成旅行，明日 00:00 刷新");
    }

    #[tokio::test]
    async fn cat_travel_traveling_reports_in_transit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            // R1：状态路径**无** /buddy/、**无** /v2
            .and(header("X-Domain", "copilot.tencent.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"state": "traveling"}
            })))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["action"], "traveling");
        assert_eq!(res["msg"], "猫猫正在旅行途中，请稍后再来查看！");
    }

    /// 企业版：直接 skip，不发任何请求（但保留 `msg`，供上层记一行）。
    #[tokio::test]
    async fn cat_travel_enterprise_skips_without_request() {
        let server = MockServer::start().await;
        let ids = Ids {
            is_enterprise: true,
            ..Ids::cn()
        };
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["action"], "skip");
        assert_eq!(res["msg"], "企业版账号无成长体系，跳过");
    }

    /// depart 的 HTTP 400 且 body 不可解析 → `HTTP 400: {detail[:120]}`。
    #[tokio::test]
    async fn depart_unparsable_error_detail_is_inlined() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "idle"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/config"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"locations": [{"id": 3}]}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/buddy/travel/depart"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid request"))
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["ok"], false);
        assert_eq!(res["msg"], "派出旅行失败: HTTP 400: invalid request");
    }

    /// 未知状态 → `当前状态: {state}`。
    #[tokio::test]
    async fn cat_travel_unknown_state_passes_through() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "sleeping"}})),
            )
            .mount(&server)
            .await;
        let ids = Ids::cn();
        let acc = ids.acc();
        let sleeper = NoopSleeper::default();
        let clock = FixedClock(10);
        let ctx = cat_ctx(&sleeper, &clock);
        let mut logs = Vec::new();
        let res =
            build_cat_travel(&UpstreamClient::new(), &acc, &ep(&server), &mut logs, &ctx).await;
        assert_eq!(res["action"], "sleeping");
        assert_eq!(res["msg"], "当前状态: sleeping");
    }

    // ---- Python 语义小工具 ----

    #[test]
    fn truthy_matches_python_rules() {
        assert!(!truthy(&Value::Null));
        assert!(truthy(&json!(true)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(truthy(&json!(1)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!([])));
        assert!(truthy(&json!([1])));
        assert!(!truthy(&json!({})));
    }

    #[test]
    fn to_json_omits_absent_optional_keys() {
        let out = GrowthRunOutcome {
            ok: true,
            msg: None,
            logs: vec!["a".into()],
            earned_credit: Some(5),
            credits: Some(json!({"remain": 9})),
        };
        assert_eq!(
            out.to_json(),
            Value::Object(map_of(&[
                ("ok", json!(true)),
                ("logs", json!(["a"])),
                ("earned_credit", json!(5)),
                ("credits", json!({"remain": 9})),
            ]))
        );
    }
}
