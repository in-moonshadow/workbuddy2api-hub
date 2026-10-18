//! 关卡 4.9：claim-all 编排（契约 18-19）。
//!
//! 权威源：`wb_proxy.py:17306-17396` 的 `_route_tasks_claim_all`。
//!
//! ## Python 语义（逐条对齐）
//! - 目标集：`realm == "cn"` / `realm == "intl"` 且 `enabled`，`uid` 为空或 `"all"`
//!   时全取，否则按 `acc.uid == uid` 过滤。
//! - **顺序**：`ordered = intl_targets + cn_targets` —— 国际版排在前面。
//! - 守卫三档（都是 HTTP 200 + `ok:false`）：池不可用 / 未找到指定账号 /
//!   未找到已启用的账号。`uid` 先做 `strip()`，再与 `"all"` 精确比较。
//! - 每账号：intl 只跑 `daily_chat`；cn 依次 `checkin` → 可选 `credit_refresh`
//!   （异常吞掉）→ 可选 `report_activity` → `growth_tasks` → `streak_bonus` →
//!   `cat_travel`（`ok` **恒 True**）。
//! - **单账号异常不中断**：`except Exception` 收成 `("error", False, str(exc))`
//!   一条 step，继续下一个账号。
//! - 间隔：签到后 0.8 / 活跃上报后 0.5 / 成长任务后 1.0 / 连登后 0.8 /
//!   猫猫后 1.0 / 账号之间 1.2（最后一个账号后不睡）。
//! - 返回体**四键**：`{ok, credit_added, accounts_count, logs}`。
//!
//! ## 与 Python 的三处有意差异（记入 4.11 账本）
//! 1. **异常边界**：Rust 没有 `try/except`。各步骤实现（[`CycleExecutor`] 的方法与
//!    [`crate::growth_runner::run_growth_tasks`]）按约定返回 `{ok:false, error}`
//!    而不是 panic，故 Python 的 `except` 分支在本实现里对应「该步骤返回错误形状」。
//!    本函数**不**用 `catch_unwind` 吞 panic —— 吞掉 panic 会掩盖真实缺陷，且
//!    `&mut` 状态在 unwind 后不保证可用。`error` step 因此只在账本里作为
//!    「Python 侧语义」保留。
//! 2. **`steps=` 行文本**：Python 的 `"  steps=%s" % steps` 是 `repr(list[tuple])`
//!    （单引号、`True`/`False`/`None`）；本实现输出 JSON 数组文本
//!    `[["checkin",true,"..."],...]`，形状等价、字面量不同。
//! 3. **`lines` 与 `logs` 的分工**：Python 的 `lines` 收标题行与 `steps=` 行，
//!    `logs` 收子日志 + 完成行。本实现以 [`AccountClaimResult::lines`] 复现
//!    `lines` 的构成（不并入返回体，仅供查证与将来的调试口）。
//!
//! ## 为什么依赖全部注入
//! 编排会打真实上游（签到 / 活跃 / 成长任务 / 连登 / 猫猫），而 M4 计划硬约束
//! 「不得对真实上游发任何请求」。这里沿用 [`crate::scheduler::CycleExecutor`]
//! 的既有钩子（不另造一套接口），再由 [`ClaimAllDeps`] 注入：
//! `credits_refresh`（`acc.credit_refresh()` 钩子）、`growth_tasks`
//! （`run_growth_tasks` 的接缝）、`sleeper`（测试注入零间隔）、`clock`（`today`）。
//!
//! 生产包装 [`cn_checkin`] / [`cn_report_activity`] 复用 [`crate::checkin`] 的既有
//! 函数；成长任务的默认实现复用 [`crate::growth_runner::run_growth_tasks`]。

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};

use wb_gateway::outbound::UpstreamClient;

use crate::checkin::{checkin, report_activity, CheckinCtx};
use crate::growth_api::{TaskAccount, TaskEndpoints};
use crate::growth_runner::{
    run_growth_tasks, CatTravel, Clock, GrowthRunOutcome, RunnerAccount, RunnerDeps, RunnerEnv,
    Sleeper,
};
use crate::scheduler::{AccountView, CycleExecutor};

// ---------------------------------------------------------------------------
// 常量（对齐 Python 原文的数字与文案）
// ---------------------------------------------------------------------------

/// 签到后间隔（`wb_proxy.py:17353`）。
pub const SLEEP_AFTER_CHECKIN_SECS: f64 = 0.8;
/// 活跃上报后间隔（`:17361`）。
pub const SLEEP_AFTER_REPORT_SECS: f64 = 0.5;
/// 成长任务后间隔（`:17368`）。
pub const SLEEP_AFTER_GROWTH_SECS: f64 = 1.0;
/// 连登管家后间隔（`:17372`）。
pub const SLEEP_AFTER_STREAK_SECS: f64 = 0.8;
/// 猫猫旅行后间隔（`:17376`）。
pub const SLEEP_AFTER_CAT_SECS: f64 = 1.0;
/// 账号之间间隔（`:17386`，最后一个账号后不睡）。
pub const SLEEP_BETWEEN_ACCOUNTS_SECS: f64 = 1.2;
/// `run_growth_tasks(acc, gap=1.0)`（`:17362`）。
pub const GROWTH_GAP_SECS: f64 = 1.0;

/// `uid` 的「全量」哨兵值（`if uid in ("", "all")`）。
pub const UID_ALL: &str = "all";

/// 完成行模板（`:17388`）：先积分、后账号数。
pub const DONE_LINE_TEMPLATE: &str =
    "====== claim-all 完成，成长任务累计积分 +{}，共 {} 个账号 ======";
/// 面板日志模板（`:17390`）：**先账号数、后积分**（与完成行相反）。
pub const PANEL_LOG_TEMPLATE: &str = "[claim-all] 全量积分领取完成，共 {} 账号，成长积分 +{}";
/// 面板日志的 tag。
pub const PANEL_LOG_TAG: &str = "tasks";

// ---------------------------------------------------------------------------
// 入参与守卫
// ---------------------------------------------------------------------------

/// 入参：只关心 `uid`（`payload.get("uid")`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaimAllRequest {
    /// 原样保存的 `uid`；`None` 等价于 Python 的缺键 / `null`。
    pub uid: Option<String>,
}

impl ClaimAllRequest {
    /// 只填 `uid`。
    pub fn new(uid: Option<&str>) -> Self {
        Self {
            uid: uid.map(str::to_string),
        }
    }

    /// `(payload.get("uid") or "").strip()`。
    pub fn normalized_uid(&self) -> String {
        self.uid.as_deref().unwrap_or("").trim().to_string()
    }
}

/// 守卫挡下时的返回体（Python 的两档 `{ok:false, msg}`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimAllGuard {
    /// `{"ok": false, "msg": "账号池不可用"}`。
    PoolUnavailable,
    /// `{"ok": false, "msg": "未找到指定账号"}`。
    AccountNotFound,
    /// `{"ok": false, "msg": "未找到已启用的账号"}`。
    NoEnabledAccount,
}

impl ClaimAllGuard {
    /// 逐字对齐 Python 文案。
    pub fn msg(&self) -> &'static str {
        match self {
            Self::PoolUnavailable => "账号池不可用",
            Self::AccountNotFound => "未找到指定账号",
            Self::NoEnabledAccount => "未找到已启用的账号",
        }
    }

    /// `{"ok": false, "msg": <msg>}`。
    pub fn to_json(&self) -> Value {
        json!({"ok": false, "msg": self.msg()})
    }
}

/// 目标集切分：顺序即执行顺序（intl 在前）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClaimAllTargets {
    /// `intl_targets`。
    pub intl: Vec<AccountView>,
    /// `cn_targets`。
    pub cn: Vec<AccountView>,
}

impl ClaimAllTargets {
    /// `ordered = list(intl_targets) + list(cn_targets)`。
    pub fn ordered(&self) -> Vec<AccountView> {
        let mut v = Vec::with_capacity(self.len());
        v.extend(self.intl.iter().cloned());
        v.extend(self.cn.iter().cloned());
        v
    }

    /// `len(ordered)`。
    pub fn len(&self) -> usize {
        self.intl.len() + self.cn.len()
    }

    /// 目标为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 纯函数版目标筛选（不碰全局池，便于测试）。
///
/// `accounts` 是账号快照；只做过滤与切分，保留池内相对顺序。
pub fn select_targets(accounts: &[AccountView], uid: &str) -> ClaimAllTargets {
    let all = uid.is_empty() || uid == UID_ALL;
    let keep = |a: &AccountView| a.enabled && (all || a.uid == uid);
    ClaimAllTargets {
        intl: accounts
            .iter()
            .filter(|a| a.realm == "intl" && keep(a))
            .cloned()
            .collect(),
        cn: accounts
            .iter()
            .filter(|a| a.realm == "cn" && keep(a))
            .cloned()
            .collect(),
    }
}

/// 守卫三档（顺序对齐 Python：先判池、再判指定账号、最后判启用账号）。
pub fn check_guard(
    pool_present: bool,
    uid: &str,
    targets: &ClaimAllTargets,
) -> Option<ClaimAllGuard> {
    if !pool_present {
        return Some(ClaimAllGuard::PoolUnavailable);
    }
    if !uid.is_empty() && uid != UID_ALL && targets.is_empty() {
        return Some(ClaimAllGuard::AccountNotFound);
    }
    if targets.is_empty() {
        return Some(ClaimAllGuard::NoEnabledAccount);
    }
    None
}

// ---------------------------------------------------------------------------
// 步骤表（`steps.append((name, ok, detail))`）
// ---------------------------------------------------------------------------

/// 一条 `steps` 项。
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimStep {
    /// 步骤名（`checkin` / `report_activity` / `growth_tasks` / `streak_bonus` /
    /// `cat_travel` / `daily_chat` / `error`）。
    pub name: &'static str,
    /// Python 的 `bool(...)` 判定结果。
    pub ok: bool,
    /// `msg or error` / `"+%d"` / 固定文案 / `None`。
    pub detail: Option<String>,
}

impl ClaimStep {
    /// 新建。
    pub fn new(name: &'static str, ok: bool, detail: Option<String>) -> Self {
        Self { name, ok, detail }
    }

    /// `[name, ok, detail]` 三元组（顺序与 Python tuple 一致）。
    pub fn to_json(&self) -> Value {
        json!([
            self.name,
            self.ok,
            match &self.detail {
                Some(d) => Value::String(d.clone()),
                None => Value::Null,
            }
        ])
    }
}

/// 单个账号的执行结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AccountClaimResult {
    /// 标题行（`====== [claim-all] nick (uid8) realm=... ======`）。
    pub header: String,
    /// 步骤表（顺序即执行顺序）。
    pub steps: Vec<ClaimStep>,
    /// 追加进返回体 `logs` 的子日志（不含标题行、不含 `steps=` 行）。
    pub logs: Vec<String>,
    /// 本账号贡献的成长积分（`credit += earned`）。
    pub credit: i64,
    /// `"  steps=<...>"` 行（JSON 数组文本，见模块注释差异 2）。
    pub steps_line: String,
}

impl AccountClaimResult {
    /// Python `lines` 里本账号贡献的部分：标题 + `steps=` 行。
    pub fn lines(&self) -> Vec<String> {
        vec![self.header.clone(), self.steps_line.clone()]
    }
}

/// 整体结果：本体四键 + 每账号明细（明细不进返回体）。
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimAllOutcome {
    /// `"ok": True`。
    pub ok: bool,
    /// `"credit_added"`。
    pub credit_added: i64,
    /// `"accounts_count"`（`len(ordered)`）。
    pub accounts_count: usize,
    /// `"logs"`。
    pub logs: Vec<String>,
    /// 每个账号的明细（顺序即 `ordered`）。
    pub accounts: Vec<AccountClaimResult>,
}

impl ClaimAllOutcome {
    /// 四键，键序对齐 Python dict 字面量。
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "credit_added": self.credit_added,
            "accounts_count": self.accounts_count,
            "logs": self.logs,
        })
    }

    /// `add_log_entry` 的入参 `(msg, tag)`（`wb_proxy.py:17390`）。
    pub fn panel_log(&self) -> (String, &'static str) {
        let msg = PANEL_LOG_TEMPLATE.replacen("{}", &self.accounts_count.to_string(), 1);
        let msg = msg.replacen("{}", &self.credit_added.to_string(), 1);
        (msg, PANEL_LOG_TAG)
    }
}

// ---------------------------------------------------------------------------
// 注入接缝
// ---------------------------------------------------------------------------

/// 成长任务的接缝（`run_growth_tasks(acc, gap=1.0)`）。
///
/// `None` ⇒ 用 [`crate::growth_runner::run_growth_tasks`] 的生产实现；
/// 测试注入假结果以覆盖「出错」「拿到积分」等分支而不打上游。
pub type GrowthTasksFn<'a> = Arc<
    dyn for<'c> Fn(
            &'c UpstreamClient,
            &'c RunnerAccount<'c>,
            &'c RunnerEnv<'c>,
        ) -> Pin<Box<dyn Future<Output = GrowthRunOutcome> + Send + 'c>>
        + Send
        + Sync
        + 'a,
>;

/// 积分刷新接缝（`acc.credit_refresh()`）。返回 `None` = 刷新失败。
pub type CreditRefreshFn<'a> = Arc<dyn Fn() -> Option<Value> + Send + Sync + 'a>;

/// 编排依赖。
pub struct ClaimAllDeps<'a> {
    /// 执行钩子（签到 / 活跃 / 连登 / 猫猫 / 每日活跃）。
    pub exec: &'a dyn CycleExecutor,
    /// 本地时间源（`today()` 判定 `can_*`）。
    pub clock: &'a dyn Clock,
    /// 睡。
    pub sleeper: &'a dyn Sleeper,
    /// 积分刷新。
    pub credits_refresh: CreditRefreshFn<'a>,
    /// 成长任务；`None` = 生产实现。
    pub growth_tasks: Option<GrowthTasksFn<'a>>,
    /// 出站客户端（成长任务的生产实现会用）。
    pub client: UpstreamClient,
    /// 账号头（成长任务生产实现用）：uid → 凭据。
    pub credentials: HashMap<String, ClaimCredentials>,
    /// 展示名：uid → 昵称（Python `Account.nickname`）。缺省/空串 ⇒ 用 uid 前 8 位。
    pub nicknames: HashMap<String, String>,
}

/// 生产装配所需的账号凭据。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaimCredentials {
    /// 出站 Bearer token。
    pub token: String,
    /// 企业空间 id。
    pub enterprise_id: String,
    /// chat 请求头。
    pub chat_headers: HashMap<String, String>,
}

impl<'a> ClaimAllDeps<'a> {
    /// 最小装配：执行钩子 + 时钟 + 睡。积分刷新恒 `None`、成长任务走生产实现、
    /// 无凭据（因此成长任务会以空身份请求；测试一律注入 `growth_tasks`）。
    pub fn new(
        exec: &'a dyn CycleExecutor,
        clock: &'a dyn Clock,
        sleeper: &'a dyn Sleeper,
    ) -> Self {
        Self {
            exec,
            clock,
            sleeper,
            credits_refresh: Arc::new(|| None),
            growth_tasks: None,
            client: UpstreamClient::new(),
            credentials: HashMap::new(),
            nicknames: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Python 语义小工具
// ---------------------------------------------------------------------------

/// `bool(v)`（Python 真值语义）。
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `res.get("msg") or res.get("error")`。
///
/// `msg` 为真值就取 `msg`，否则取 `error`；两者都缺失或为假 ⇒ `None`。
/// 非字符串按 JSON 字面量转文本（Python `str()` 的近似）。
pub fn msg_or_error(res: &Value) -> Option<String> {
    for key in ["msg", "error"] {
        match res.get(key) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(Value::String(_)) | None | Some(Value::Null) => {}
            Some(other) => return Some(other.to_string()),
        }
    }
    None
}

/// `bool(r.get("ok", True))`：缺键视为 True（`growth_tasks` 的哨兵默认）。
pub fn ok_or_true(v: &Value) -> bool {
    match v.get("ok") {
        None => true,
        Some(other) => truthy(Some(other)),
    }
}

/// `msg` 的显示文本：Python `"%s" % None` ⇒ `"None"`，缺键同此。
fn text_or_none(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// [`msg_or_error`] 结果的显示文本，同上（`None` ⇒ `"None"`）。
fn text_or_none_str(v: Option<String>) -> String {
    v.unwrap_or_else(|| "None".to_string())
}

/// `nickname or (uid[:8] if uid else "?")`（Python `or` 只看真值）。
///
/// 昵称不在 [`AccountView`] 上：那是 `wb-scheduler` 的既有视图，签到/轮询
/// 都不需要它，扩字段会波及 4.8 的全部构造点。这里改为由调用方传入
/// `uid -> nickname` 查表（Python 的 `Account.nickname`，空串等价「无昵称」）。
pub fn display_name(acc: &AccountView, nicknames: &HashMap<String, String>) -> String {
    match nicknames.get(&acc.uid) {
        Some(n) if !n.is_empty() => n.clone(),
        _ => uid8(&acc.uid),
    }
}

/// `acc.uid[:8] if acc.uid else "?"`（按字符截断）。
pub fn uid8(uid: &str) -> String {
    if uid.is_empty() {
        "?".to_string()
    } else {
        uid.chars().take(8).collect()
    }
}

/// 标题行：`"====== [claim-all] %s (%s) realm=%s ======"`。
pub fn header_line(acc: &AccountView, nicknames: &HashMap<String, String>) -> String {
    format!(
        "====== [claim-all] {} ({}) realm={} ======",
        display_name(acc, nicknames),
        uid8(&acc.uid),
        acc.realm
    )
}

/// 把 `steps` 序列化成 Python `"  steps=%s" % steps` 的等价文本：
/// 两空格 + `steps=` + JSON 数组文本（形状对应 `repr(list[tuple])`）。
fn steps_line(steps: &[ClaimStep]) -> String {
    format!(
        "  steps={}",
        Value::Array(steps.iter().map(ClaimStep::to_json).collect())
    )
}

// ---------------------------------------------------------------------------
// 单账号执行
// ---------------------------------------------------------------------------

/// 成长任务步的产物（`r.get("earned_credit") or 0` / `bool(r.get("ok", True))` / `logs`）。
#[derive(Debug, Clone, PartialEq)]
pub struct GrowthStepOutcome {
    /// `bool(r.get("ok", True))`。
    pub ok: bool,
    /// `int(r.get("earned_credit") or 0)`。
    pub earned: i64,
    /// `r.get("logs") or []`。
    pub logs: Vec<String>,
}

impl GrowthStepOutcome {
    /// 从 `run_growth_tasks` 的四档返回折算。
    pub fn from_outcome(o: &GrowthRunOutcome) -> Self {
        Self {
            ok: o.ok,
            earned: o.earned_credit.unwrap_or(0),
            logs: o.logs.clone(),
        }
    }

    /// 从 `Value` 折算：`bool(r.get("ok", True))` / `int(r.get("earned_credit") or 0)`
    /// / `r.get("logs") or []`。缺口键语义由 [`ok_or_true`] 承载。
    pub fn from_value(v: &Value) -> Self {
        let earned = match v.get("earned_credit") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
            _ => 0,
        };
        let logs = v
            .get("logs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            ok: ok_or_true(v),
            earned,
            logs,
        }
    }
}

/// 单个 intl 账号：只跑 `daily_chat`（`wb_proxy.py:17333-17343`）。
async fn run_intl(acc: &AccountView, today: &str, deps: &ClaimAllDeps<'_>) -> AccountClaimResult {
    let nick = display_name(acc, &deps.nicknames);
    let mut steps = Vec::new();
    let mut logs = Vec::new();
    if acc.can_daily_chat(today) {
        let res = deps.exec.daily_chat(acc).await;
        let ok = truthy(res.get("ok"));
        steps.push(ClaimStep::new("daily_chat", ok, msg_or_error(&res)));
        if ok {
            logs.push(format!(
                "  [{}] 每日活跃 {}",
                nick,
                text_or_none(res.get("msg"))
            ));
        } else {
            logs.push(format!(
                "  [{}] 每日活跃失败 {}",
                nick,
                text_or_none(res.get("error"))
            ));
        }
    } else {
        steps.push(ClaimStep::new(
            "daily_chat",
            false,
            Some("今日已做或不可做".to_string()),
        ));
        logs.push(format!("  [{nick}] 每日活跃跳过（今日已完成）"));
    }
    AccountClaimResult {
        header: header_line(acc, &deps.nicknames),
        steps_line: steps_line(&steps),
        steps,
        logs,
        credit: 0,
    }
}

/// 单个 cn 账号的五步（`wb_proxy.py:17344-17376`）。
async fn run_cn(
    acc: &AccountView,
    today: &str,
    deps: &ClaimAllDeps<'_>,
    growth: GrowthStepOutcome,
) -> AccountClaimResult {
    let nick = display_name(acc, &deps.nicknames);
    let exec = deps.exec;
    let sleeper = deps.sleeper;
    let mut steps = Vec::new();
    let mut logs = Vec::new();

    // ① 签到
    if acc.can_checkin(today) {
        let res = exec.checkin(acc).await;
        let ok = truthy(res.get("ok"));
        steps.push(ClaimStep::new("checkin", ok, msg_or_error(&res)));
        logs.push(format!(
            "  [{}] 签到 {}",
            nick,
            text_or_none_str(msg_or_error(&res))
        ));
        // `acc.credit_refresh()`：Python 的异常被吞掉；这里只取值不使用。
        let _ = (deps.credits_refresh)();
        sleeper.sleep(SLEEP_AFTER_CHECKIN_SECS).await;
    } else {
        steps.push(ClaimStep::new(
            "checkin",
            false,
            Some("今日已签到".to_string()),
        ));
        logs.push(format!("  [{nick}] 签到跳过（今日已完成）"));
    }

    // ② 活跃上报（可选）
    if acc.can_report_activity(today) {
        let res = exec.report_activity(acc).await;
        let ok = truthy(res.get("ok"));
        steps.push(ClaimStep::new("report_activity", ok, msg_or_error(&res)));
        logs.push(format!(
            "  [{}] 活跃上报 {}",
            nick,
            text_or_none_str(msg_or_error(&res))
        ));
        sleeper.sleep(SLEEP_AFTER_REPORT_SECS).await;
    }

    // ③ 成长任务
    steps.push(ClaimStep::new(
        "growth_tasks",
        growth.ok,
        Some(format!("+{}", growth.earned)),
    ));
    for line in &growth.logs {
        logs.push(format!("  {line}"));
    }
    sleeper.sleep(SLEEP_AFTER_GROWTH_SECS).await;

    // ④ 连登管家
    let r2 = exec.streak_bonus(acc).await;
    steps.push(ClaimStep::new(
        "streak_bonus",
        truthy(r2.get("ok")),
        msg_or_error(&r2),
    ));
    logs.push(format!(
        "  [{}] 连登管家 {}",
        nick,
        text_or_none(r2.get("msg"))
    ));
    sleeper.sleep(SLEEP_AFTER_STREAK_SECS).await;

    // ⑤ 猫猫旅行（`ok` 恒 True）
    let tr = exec.cat_travel(acc).await;
    steps.push(ClaimStep::new("cat_travel", true, msg_or_error(&tr)));
    logs.push(format!(
        "  [{}] 猫猫日常 {}",
        nick,
        text_or_none(tr.get("msg"))
    ));
    sleeper.sleep(SLEEP_AFTER_CAT_SECS).await;

    AccountClaimResult {
        header: header_line(acc, &deps.nicknames),
        steps_line: steps_line(&steps),
        steps,
        logs,
        credit: growth.earned,
    }
}

/// 跑成长任务步：有接缝用接缝，否则走生产实现。
async fn run_growth_step(acc: &AccountView, deps: &ClaimAllDeps<'_>) -> GrowthStepOutcome {
    let default_cred = ClaimCredentials::default();
    let cred = deps.credentials.get(&acc.uid).unwrap_or(&default_cred);
    let runner = RunnerAccount {
        api: TaskAccount {
            uid: &acc.uid,
            token: &cred.token,
            enterprise_id: &cred.enterprise_id,
            chat_headers: &cred.chat_headers,
        },
        realm: &acc.realm,
        nickname: deps.nicknames.get(&acc.uid).map(String::as_str),
        is_enterprise: acc.is_enterprise,
    };
    let env = RunnerEnv::default();
    let outcome = match &deps.growth_tasks {
        Some(f) => f(&deps.client, &runner, &env).await,
        None => {
            let rdeps = RunnerDeps {
                sleeper: deps.sleeper,
                clock: deps.clock,
                credits_refresh: Box::new(|| None),
                cat_travel: CatTravel::Disabled,
                gap: GROWTH_GAP_SECS,
            };
            run_growth_tasks(&deps.client, &runner, &env, rdeps).await
        }
    };
    GrowthStepOutcome::from_outcome(&outcome)
}

// ---------------------------------------------------------------------------
// 编排主体
// ---------------------------------------------------------------------------

/// 对已筛好的目标集执行编排（不含守卫）。
///
/// `logs` 顺序严格按 Python：每账号子日志 → 完成行。最后一个账号后不睡。
pub async fn run_claim_all(targets: &ClaimAllTargets, deps: &ClaimAllDeps<'_>) -> ClaimAllOutcome {
    let today = crate::checkin::today(deps.clock);
    let ordered = targets.ordered();
    let mut logs: Vec<String> = Vec::new();
    let mut accounts: Vec<AccountClaimResult> = Vec::with_capacity(ordered.len());
    let mut credit = 0i64;

    for (i, acc) in ordered.iter().enumerate() {
        let result = if acc.realm == "intl" {
            run_intl(acc, &today, deps).await
        } else {
            let growth = run_growth_step(acc, deps).await;
            run_cn(acc, &today, deps, growth).await
        };
        credit += result.credit;
        logs.extend(result.logs.iter().cloned());
        accounts.push(result);
        if i + 1 < ordered.len() {
            deps.sleeper.sleep(SLEEP_BETWEEN_ACCOUNTS_SECS).await;
        }
    }

    let done = DONE_LINE_TEMPLATE.replacen("{}", &credit.to_string(), 1);
    let done = done.replacen("{}", &ordered.len().to_string(), 1);
    logs.push(done);

    ClaimAllOutcome {
        ok: true,
        credit_added: credit,
        accounts_count: ordered.len(),
        logs,
        accounts,
    }
}

/// `_route_tasks_claim_all` 的完整语义。
///
/// 不含 `invalidate_tasks_cache()`（那属于装配层对
/// [`crate::snapshot_cache::SnapshotCache::invalidate`] 的调用）。
/// 守卫命中 ⇒ 两键 `{ok:false,msg}`；否则四键。HTTP 状态两档都是 200。
pub async fn route_claim_all(
    req: &ClaimAllRequest,
    accounts: &[AccountView],
    pool_present: bool,
    deps: &ClaimAllDeps<'_>,
) -> Value {
    let uid = req.normalized_uid();
    let targets = select_targets(accounts, &uid);
    if let Some(guard) = check_guard(pool_present, &uid, &targets) {
        return guard.to_json();
    }
    run_claim_all(&targets, deps).await.to_json()
}

// ---------------------------------------------------------------------------
// 生产步骤装配（供 4.10 接线复用；不改变上面的编排逻辑）
// ---------------------------------------------------------------------------

/// 生产签到 / 活跃上报适配器的所需上下文。
pub struct CnStepCtx<'a> {
    /// 账号池目录。
    pub dir: &'a Path,
    /// 账号文件名（`<safe_uid>.json`）。
    pub account_name: &'a str,
    /// 时钟。
    pub clock: &'a dyn Clock,
    /// 设备令牌。
    pub device_token: Option<&'a str>,
    /// billing 上游基址（生产用 `wb_tasks::checkin::production_billing_base()`）。
    pub billing_base: &'a str,
    /// 账号出站身份。
    pub account: RunnerAccount<'a>,
    /// 业务端点（活跃上报用）。
    pub endpoints: TaskEndpoints<'a>,
}

/// `acc.checkin(trigger="claim-all")` 的生产包装。
pub async fn cn_checkin(client: &UpstreamClient, ctx: &CnStepCtx<'_>) -> Value {
    let c = CheckinCtx {
        dir: ctx.dir,
        account_name: ctx.account_name,
        clock: ctx.clock,
        device_token: ctx.device_token,
    };
    checkin(client, &ctx.account, ctx.billing_base, &c)
        .await
        .to_json()
}

/// `acc.report_activity()` 的生产包装。`now_ms` 为当前毫秒时间戳。
pub async fn cn_report_activity(
    client: &UpstreamClient,
    ctx: &CnStepCtx<'_>,
    now_ms: i64,
) -> Value {
    let c = CheckinCtx {
        dir: ctx.dir,
        account_name: ctx.account_name,
        clock: ctx.clock,
        device_token: ctx.device_token,
    };
    report_activity(client, &ctx.account, &ctx.endpoints, &c, now_ms).await
}
