//! 关卡 4.9 产出物：契约 18-19 —— claim-all 编排。
//!
//! 计划原文：「契约 18-19：目标集、intl 优先顺序、步骤表、异常不中断、
//! 间隔 sleep（测试中注入零间隔）」。权威源 `wb_proxy.py:17306-17396`
//! 的 `_route_tasks_claim_all`。
//!
//! 断言清单：
//! 1. 守卫三档：池不可用 / 未找到指定账号 / 未找到已启用账号 —— 逐字 `msg`、
//!    两键 `{ok,msg}`、`ok` 为 False。
//! 2. `uid` 三态：缺键 / `"all"` / 带空白都等价「全量」；未知 uid 落
//!    「未找到指定账号」。
//! 3. 目标集：只取 `enabled`；`realm` 只认 `cn`/`intl`；未知 realm 不入集。
//! 4. **intl 先于 cn**（`ordered = intl + cn`），且各自保持池内相对顺序。
//! 5. intl 只跑 `daily_chat`；不可做时 `ok:False, detail="今日已做或不可做"`。
//! 6. cn 五步顺序：`checkin → report_activity → growth_tasks → streak_bonus →
//!    cat_travel`；签到/上报不可做时跳过（但不睡）。
//! 7. `cat_travel` 的 step `ok` **恒 True**（即使上游返回 `ok:false`）。
//! 8. `growth_tasks` 的 detail 是 `"+<earned>"`，积分累加进 `credit_added`。
//! 9. 连登/猫猫缺 `msg` 时日志写 `"None"`（Python `"%s" % None`）。
//! 10. **单账号出错不中断**：某账号某步返回 `ok:false` 后，其余账号仍执行。
//! 11. 间隔序列：签到 0.8 / 上报 0.5 / 成长 1.0 / 连登 0.8 / 猫猫 1.0 /
//!     账号间 1.2；**最后一个账号后不睡 1.2**。
//! 12. 返回体四键形状与 `accounts_count`。
//! 13. 完成行「先积分后账号数」；面板日志「先账号数后积分」，tag=`tasks`。
//! 14. `lines()` = 标题 + `steps=` 行。
//!
//! 全程注入 fake executor + `NoopSleeper` + 固定时钟，**不发任何真实上游请求**。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use wb_tasks::claim_all::{
    check_guard, display_name, header_line, msg_or_error, ok_or_true, route_claim_all,
    select_targets, truthy, uid8, ClaimAllDeps, ClaimAllGuard, ClaimAllRequest, ClaimAllTargets,
    ClaimStep, GrowthStepOutcome, UID_ALL,
};
use wb_tasks::growth_runner::{Clock, GrowthRunOutcome, NoopSleeper};
use wb_tasks::scheduler::{AccountView, CycleExecutor};

/// 单步结果的 future 别名（`scheduler` 内部别名未导出）。
type StepFut<'a> = Pin<Box<dyn Future<Output = Value> + Send + 'a>>;

const TODAY: &str = "2026-10-17";

// ---------------------------------------------------------------------------
// 测试支架：固定时钟
// ---------------------------------------------------------------------------

/// 固定「今天 = 2026-10-17 09:00:00」。
struct FixedClock;

impl Clock for FixedClock {
    fn local_hour(&self) -> u32 {
        9
    }

    fn local_now(&self) -> String {
        "2026-10-17 09:00:00".to_string()
    }
}

// ---------------------------------------------------------------------------
// 测试支架：账号视图
// ---------------------------------------------------------------------------

/// 造一个「今天都还没做」的账号。
fn account(uid: &str, realm: &str) -> AccountView {
    AccountView {
        uid: uid.to_string(),
        realm: realm.to_string(),
        enabled: true,
        expires_at: None,
        is_enterprise: false,
        last_checkin: None,
        last_activity_report: None,
        last_daily_chat: None,
        daily_chat_granted_today: false,
    }
}

fn cn(uid: &str) -> AccountView {
    account(uid, "cn")
}

fn intl(uid: &str) -> AccountView {
    account(uid, "intl")
}

/// 「今天已签到」的 cn 账号。
fn cn_checked_in(uid: &str) -> AccountView {
    let mut a = cn(uid);
    a.last_checkin = Some(TODAY.to_string());
    a
}

// ---------------------------------------------------------------------------
// 测试支架：记账 executor
// ---------------------------------------------------------------------------

/// 一次调用记录：`(步骤名, uid)`。
type Call = (String, String);

/// 可编排的 fake：按 `(步骤名, uid)` 决定返回体，并记录调用顺序。
struct FakeExecutor {
    accounts: Vec<AccountView>,
    /// `(步骤名, uid)` → 该步返回的 JSON；未命中走 [`Self::default_for`]。
    scripted: Mutex<Vec<(String, String, Value)>>,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeExecutor {
    fn new(accounts: Vec<AccountView>) -> Self {
        Self {
            accounts,
            scripted: Mutex::new(Vec::new()),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 给某个 `(步骤, uid)` 指定返回体。
    fn script(self, step: &str, uid: &str, value: Value) -> Self {
        self.scripted
            .lock()
            .unwrap()
            .push((step.to_string(), uid.to_string(), value));
        self
    }

    fn take(&self, step: &str, uid: &str) -> Value {
        self.calls
            .lock()
            .unwrap()
            .push((step.to_string(), uid.to_string()));
        let scripted = self.scripted.lock().unwrap();
        for (s, u, v) in scripted.iter() {
            if s == step && u == uid {
                return v.clone();
            }
        }
        Self::default_for(step)
    }

    /// 未编排时的缺省返回：成功、带可辨识文案。
    fn default_for(step: &str) -> Value {
        match step {
            "checkin" => json!({"ok": true, "msg": "签到成功"}),
            "report_activity" => json!({"ok": true, "msg": "上报成功"}),
            "streak_bonus" => json!({"ok": true, "msg": "连登 +1"}),
            "cat_travel" => json!({"ok": true, "msg": "猫咪已出发"}),
            "daily_chat" => json!({"ok": true, "msg": "对话成功"}),
            _ => json!({"ok": true}),
        }
    }

    /// `(步骤名, uid)` 序列。
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// 只取步骤名序列（便于断言顺序）。
    fn steps(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.0).collect()
    }

    /// uid 首次出现顺序（用于断言账号执行顺序）。
    fn first_seen_uids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, uid) in self.calls() {
            if !out.contains(&uid) {
                out.push(uid);
            }
        }
        out
    }
}

impl CycleExecutor for FakeExecutor {
    fn refresh<'a>(
        &'a self,
        _acc: &'a AccountView,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async { false })
    }

    fn checkin<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let v = self.take("checkin", &acc.uid);
        Box::pin(async move { v })
    }

    fn report_activity<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let v = self.take("report_activity", &acc.uid);
        Box::pin(async move { v })
    }

    fn cat_travel<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let v = self.take("cat_travel", &acc.uid);
        Box::pin(async move { v })
    }

    fn streak_bonus<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let v = self.take("streak_bonus", &acc.uid);
        Box::pin(async move { v })
    }

    fn night_growth<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        // claim-all 不跑夜猫子；一旦被调用就留下痕迹便于失败定位。
        let v = self.take("night_growth", &acc.uid);
        Box::pin(async move { v })
    }

    fn daily_chat<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let v = self.take("daily_chat", &acc.uid);
        Box::pin(async move { v })
    }

    fn accounts(&self) -> Vec<AccountView> {
        self.accounts.clone()
    }
}

// ---------------------------------------------------------------------------
// 测试支架：成长任务接缝
// ---------------------------------------------------------------------------

/// 一个成功的成长任务结果。
fn growth_ok(earned: i64) -> GrowthRunOutcome {
    GrowthRunOutcome {
        ok: true,
        msg: None,
        logs: vec!["领了 3 个任务".to_string()],
        earned_credit: Some(earned),
        credits: None,
    }
}

/// 固定积分的成长接缝。
fn growth_returning(earned: i64) -> wb_tasks::claim_all::GrowthTasksFn<'static> {
    Arc::new(move |_, _, _| Box::pin(async move { growth_ok(earned) }))
}

/// 取完整结果（含每账号 steps），用于断言 step 表而不仅是返回体。
async fn out_steps(deps: &ClaimAllDeps<'_>, accounts: &[AccountView]) -> Value {
    let targets = select_targets(accounts, "");
    let out = wb_tasks::claim_all::run_claim_all(&targets, deps).await;
    json!({
        "credit_added": out.credit_added,
        "accounts_count": out.accounts_count,
        "accounts": out.accounts.iter().map(|a| json!({
            "header": a.header,
            "lines": a.lines(),
            "steps": a.steps.iter().map(ClaimStep::to_json).collect::<Vec<_>>(),
            "credit": a.credit,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// 1-2：守卫三档与 uid 三态
// ---------------------------------------------------------------------------

#[test]
fn guard_pool_unavailable_wins_over_everything() {
    let targets = ClaimAllTargets::default();
    let guard = check_guard(false, "", &targets);
    assert_eq!(guard, Some(ClaimAllGuard::PoolUnavailable));
    assert_eq!(
        guard.unwrap().to_json(),
        json!({"ok": false, "msg": "账号池不可用"})
    );
}

#[test]
fn guard_unknown_uid_reports_account_not_found() {
    let accounts = vec![cn("cn1"), intl("i1")];
    let targets = select_targets(&accounts, "nope");
    assert!(targets.is_empty());
    let guard = check_guard(true, "nope", &targets);
    assert_eq!(guard, Some(ClaimAllGuard::AccountNotFound));
    assert_eq!(
        guard.unwrap().to_json(),
        json!({"ok": false, "msg": "未找到指定账号"})
    );
}

#[test]
fn guard_empty_pool_reports_no_enabled_account() {
    let guard = check_guard(true, "", &ClaimAllTargets::default());
    assert_eq!(guard, Some(ClaimAllGuard::NoEnabledAccount));
    assert_eq!(
        guard.unwrap().to_json(),
        json!({"ok": false, "msg": "未找到已启用的账号"})
    );
}

#[test]
fn guard_all_sentinel_with_empty_pool_reports_no_enabled_account() {
    // `uid == "all"` 走「全量」分支，因此落第二档而不是「未找到指定账号」。
    let guard = check_guard(true, UID_ALL, &ClaimAllTargets::default());
    assert_eq!(guard, Some(ClaimAllGuard::NoEnabledAccount));
}

#[test]
fn request_normalizes_uid_by_trimming() {
    assert_eq!(ClaimAllRequest::new(None).normalized_uid(), "");
    assert_eq!(ClaimAllRequest::new(Some("  ")).normalized_uid(), "");
    assert_eq!(ClaimAllRequest::new(Some(" all ")).normalized_uid(), "all");
    assert_eq!(ClaimAllRequest::new(Some(" u1 ")).normalized_uid(), "u1");
    assert_eq!(ClaimAllRequest::default(), ClaimAllRequest::new(None));
}

// ---------------------------------------------------------------------------
// 3-4：目标集筛选与 intl 优先
// ---------------------------------------------------------------------------

#[test]
fn select_targets_keeps_only_enabled_cn_and_intl() {
    let mut disabled = cn("off");
    disabled.enabled = false;
    let accounts = vec![
        cn("c1"),
        intl("i1"),
        disabled,
        account("x1", "eu"),
        intl("i2"),
        cn("c2"),
    ];
    let targets = select_targets(&accounts, "");
    assert_eq!(
        targets
            .intl
            .iter()
            .map(|a| a.uid.as_str())
            .collect::<Vec<_>>(),
        vec!["i1", "i2"],
        "intl 保持池内相对顺序"
    );
    assert_eq!(
        targets
            .cn
            .iter()
            .map(|a| a.uid.as_str())
            .collect::<Vec<_>>(),
        vec!["c1", "c2"],
        "cn 保持池内相对顺序"
    );
    assert_eq!(targets.len(), 4);
    assert!(!targets.is_empty());
}

#[test]
fn ordered_puts_intl_before_cn_regardless_of_pool_order() {
    let accounts = vec![cn("c1"), intl("i1"), cn("c2"), intl("i2")];
    let targets = select_targets(&accounts, "");
    assert_eq!(
        targets
            .ordered()
            .iter()
            .map(|a| a.uid.as_str())
            .collect::<Vec<_>>(),
        vec!["i1", "i2", "c1", "c2"]
    );
}

#[test]
fn select_targets_filters_by_uid_when_given() {
    let accounts = vec![cn("c1"), cn("c2"), intl("i1"), intl("i2")];
    let targets = select_targets(&accounts, "c2");
    assert!(targets.intl.is_empty());
    assert_eq!(targets.cn.len(), 1);
    assert_eq!(targets.cn[0].uid, "c2");
    assert!(
        check_guard(true, "c2", &targets).is_none(),
        "命中的 uid 不应被守卫拦下"
    );
}

#[test]
fn select_targets_treats_all_sentinel_as_everything() {
    let accounts = vec![cn("c1"), intl("i1")];
    assert_eq!(select_targets(&accounts, "").len(), 2);
    assert_eq!(select_targets(&accounts, UID_ALL).len(), 2);
}

// ---------------------------------------------------------------------------
// 5：intl 只跑 daily_chat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn intl_account_runs_only_daily_chat() {
    let accounts = vec![intl("i1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let deps = ClaimAllDeps::new(&exec, &clock, &sleeper);

    let out = route_claim_all(&ClaimAllRequest::new(None), &accounts, true, &deps).await;
    assert_eq!(exec.steps(), vec!["daily_chat"], "intl 不跑 cn 五步");

    let logs = out["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 2, "子日志 + 完成行");
    assert_eq!(logs[0], "  [i1] 每日活跃 对话成功");
    assert_eq!(
        logs[1],
        "====== claim-all 完成，成长任务累计积分 +0，共 1 个账号 ======"
    );
    assert_eq!(out["credit_added"], 0);
}

#[tokio::test]
async fn intl_daily_chat_skipped_when_already_done_today() {
    let mut acc = intl("i1");
    acc.last_daily_chat = Some(TODAY.to_string());
    let accounts = vec![acc];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let deps = ClaimAllDeps::new(&exec, &clock, &sleeper);

    let out = route_claim_all(&ClaimAllRequest::new(None), &accounts, true, &deps).await;
    assert!(exec.steps().is_empty(), "不可做时不应调用下游");
    assert_eq!(sleeper.calls(), Vec::<f64>::new(), "intl 分支全程不睡");
    assert_eq!(out["logs"][0], "  [i1] 每日活跃跳过（今日已完成）");

    // step 表里仍有一条 `daily_chat`，`ok:False`、固定文案。
    let full = out_steps(&deps, &accounts).await;
    let steps = full["accounts"][0]["steps"].as_array().unwrap();
    assert_eq!(steps[0][0], "daily_chat");
    assert_eq!(steps[0][1], false);
    assert_eq!(steps[0][2], "今日已做或不可做");
}

#[tokio::test]
async fn intl_daily_chat_granted_flag_also_blocks() {
    let mut acc = intl("i1");
    acc.daily_chat_granted_today = true;
    let accounts = vec![acc];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let deps = ClaimAllDeps::new(&exec, &clock, &sleeper);

    let _ = route_claim_all(&ClaimAllRequest::new(None), &accounts, true, &deps).await;
    assert!(exec.steps().is_empty());
}

// ---------------------------------------------------------------------------
// 6-9：cn 五步顺序、跳过分支、cat_travel 恒 True、growth 积分
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cn_account_runs_five_steps_in_python_order() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(120));

    let out = out_steps(&deps, &accounts).await;
    let steps = out["accounts"][0]["steps"].as_array().unwrap();
    let names = steps
        .iter()
        .map(|s| s[0].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "checkin",
            "report_activity",
            "growth_tasks",
            "streak_bonus",
            "cat_travel"
        ]
    );
    // growth 的 detail 是 `+earned`
    assert_eq!(steps[2][2], "+120");
    assert_eq!(out["credit_added"], 120);
    // 下游调用顺序（不含 growth，它走接缝）
    assert_eq!(
        exec.steps(),
        vec!["checkin", "report_activity", "streak_bonus", "cat_travel"]
    );
}

#[tokio::test]
async fn cn_skip_branches_do_not_call_downstream_nor_sleep() {
    // 今天已签到 + 今天已上报 ⇒ 两步都跳过，也就没有 0.8 / 0.5 的睡。
    let mut acc = cn_checked_in("c1");
    acc.last_activity_report = Some(TODAY.to_string());
    let accounts = vec![acc];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let out = out_steps(&deps, &accounts).await;
    let steps = out["accounts"][0]["steps"].as_array().unwrap();
    assert_eq!(steps[0][0], "checkin");
    assert_eq!(steps[0][1], false);
    assert_eq!(steps[0][2], "今日已签到");
    // 上报被跳过 ⇒ 没有 report_activity step（Python 只在可做时追加）
    let names = steps
        .iter()
        .map(|s| s[0].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["checkin", "growth_tasks", "streak_bonus", "cat_travel"]
    );
    assert_eq!(
        exec.steps(),
        vec!["streak_bonus", "cat_travel"],
        "只有签到/上报可做时才调下游"
    );
    // 跳过签到/上报 ⇒ 只剩 成长 1.0 + 连登 0.8 + 猫猫 1.0（单账号，无账号间 1.2）
    assert_eq!(sleeper.calls(), vec![1.0, 0.8, 1.0]);
}

#[tokio::test]
async fn cat_travel_step_is_always_ok_even_when_upstream_fails() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone()).script(
        "cat_travel",
        "c1",
        json!({"ok": false, "error": "猫猫罢工"}),
    );
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let out = out_steps(&deps, &accounts).await;
    let steps = out["accounts"][0]["steps"].as_array().unwrap();
    let cat = steps.iter().find(|s| s[0] == "cat_travel").unwrap();
    assert_eq!(cat[1], true, "Python 里 cat_travel 的 ok 恒 True");
    assert_eq!(cat[2], "猫猫罢工", "detail 仍透传 msg or error");
}

#[tokio::test]
async fn streak_and_cat_logs_write_none_when_msg_missing() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone())
        .script("streak_bonus", "c1", json!({"ok": true}))
        .script("cat_travel", "c1", json!({"ok": true}));
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let targets = select_targets(&accounts, "");
    let out = wb_tasks::claim_all::run_claim_all(&targets, &deps).await;
    let logs = &out.logs;
    assert!(
        logs.iter().any(|l| l == "  [c1] 连登管家 None"),
        "Python `%s % None` ⇒ None，实际：{logs:?}"
    );
    assert!(
        logs.iter().any(|l| l == "  [c1] 猫猫日常 None"),
        "实际：{logs:?}"
    );
}

#[tokio::test]
async fn growth_logs_get_two_space_prefix() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(30));

    let targets = select_targets(&accounts, "");
    let out = wb_tasks::claim_all::run_claim_all(&targets, &deps).await;
    assert!(
        out.logs.iter().any(|l| l == "  领了 3 个任务"),
        "成长子日志前置两空格，实际：{:?}",
        out.logs
    );
}

// ---------------------------------------------------------------------------
// 10：单账号出错不中断
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_account_failure_does_not_stop_the_others() {
    let accounts = vec![cn("c1"), cn("c2"), cn("c3")];
    let exec = FakeExecutor::new(accounts.clone())
        // c1 的签到失败（返回 ok:false）。这不是 panic，而是 4.9 对 Python
        // `except Exception` 的有意等价物：记一条 ok:false 的 step 后继续。
        .script(
            "checkin",
            "c1",
            json!({"ok": false, "error": "签到接口 500"}),
        )
        .script(
            "streak_bonus",
            "c1",
            json!({"ok": false, "error": "连登接口超时"}),
        );
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let out = out_steps(&deps, &accounts).await;
    assert_eq!(out["accounts_count"], 3);
    assert_eq!(out["accounts"].as_array().unwrap().len(), 3);

    // 三个账号都被执行了（c1 失败不影响 c2/c3）
    let uids = exec.first_seen_uids();
    assert_eq!(uids, vec!["c1", "c2", "c3"]);

    // c1 的签到 step 记录了失败与原因
    let c1_steps = out["accounts"][0]["steps"].as_array().unwrap();
    let checkin = &c1_steps[0];
    assert_eq!(checkin[0], "checkin");
    assert_eq!(checkin[1], false);
    assert_eq!(checkin[2], "签到接口 500");

    // c2/c3 全部成功
    for i in 1..3 {
        for s in out["accounts"][i]["steps"].as_array().unwrap() {
            assert_eq!(s[1], true, "账号 {} 的每一步都应成功", i + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// 11：间隔序列
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sleep_sequence_matches_python_with_no_sleep_after_last_account() {
    // 两个 cn 账号、全部可做 ⇒ 每账号 0.8 + 0.5 + 1.0 + 0.8 + 1.0 = 4.1；
    // 账号之间 1.2，但最后一个账号之后不睡。
    let accounts = vec![cn("c1"), cn("c2")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let targets = select_targets(&accounts, "");
    let _ = wb_tasks::claim_all::run_claim_all(&targets, &deps).await;

    let per_account = [0.8, 0.5, 1.0, 0.8, 1.0];
    let mut expected = Vec::new();
    expected.extend_from_slice(&per_account);
    expected.push(1.2);
    expected.extend_from_slice(&per_account);
    assert_eq!(
        sleeper.calls(),
        expected,
        "间隔必须与 Python 17353/17361/17368/17372/17376/17386 一致"
    );
}

#[tokio::test]
async fn no_between_account_sleep_for_single_account() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let targets = select_targets(&accounts, "");
    let _ = wb_tasks::claim_all::run_claim_all(&targets, &deps).await;
    assert_eq!(sleeper.calls(), vec![0.8, 0.5, 1.0, 0.8, 1.0]);
    assert!(!sleeper.calls().contains(&1.2));
}

// ---------------------------------------------------------------------------
// 12-13：返回体四键、完成行、面板日志
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_has_exactly_four_keys_in_python_order() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(15));

    let out = route_claim_all(&ClaimAllRequest::new(None), &accounts, true, &deps).await;
    let keys = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(keys, vec!["ok", "credit_added", "accounts_count", "logs"]);
    assert_eq!(out["ok"], true);
    assert_eq!(out["credit_added"], 15);
    assert_eq!(out["accounts_count"], 1);
    assert!(out["logs"].is_array());
}

#[tokio::test]
async fn done_line_puts_credit_before_count_while_panel_log_is_reversed() {
    let accounts = vec![cn("c1"), cn("c2")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(50));

    let out = route_claim_all(&ClaimAllRequest::new(None), &accounts, true, &deps).await;
    let logs = out["logs"].as_array().unwrap();
    let last = logs.last().unwrap().as_str().unwrap();
    assert_eq!(
        last,
        "====== claim-all 完成，成长任务累计积分 +100，共 2 个账号 ======"
    );

    // 面板日志：先账号数、后积分
    let targets = select_targets(&accounts, "");
    let outcome = wb_tasks::claim_all::run_claim_all(&targets, &deps).await;
    let (msg, tag) = outcome.panel_log();
    assert_eq!(
        msg,
        "[claim-all] 全量积分领取完成，共 2 账号，成长积分 +100"
    );
    assert_eq!(tag, "tasks");
}

#[tokio::test]
async fn guard_hits_return_two_key_envelope_and_never_run_steps() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let deps = ClaimAllDeps::new(&exec, &clock, &sleeper);

    // 池不可用
    let out = route_claim_all(&ClaimAllRequest::new(None), &accounts, false, &deps).await;
    assert_eq!(out, json!({"ok": false, "msg": "账号池不可用"}));

    // 未知 uid
    let out = route_claim_all(&ClaimAllRequest::new(Some("nope")), &accounts, true, &deps).await;
    assert_eq!(out, json!({"ok": false, "msg": "未找到指定账号"}));

    // 空池
    let out = route_claim_all(&ClaimAllRequest::new(None), &[], true, &deps).await;
    assert_eq!(out, json!({"ok": false, "msg": "未找到已启用的账号"}));

    assert!(exec.steps().is_empty(), "守卫挡下时不得执行任何步骤");
    assert!(sleeper.calls().is_empty(), "守卫挡下时不得睡眠");
}

// ---------------------------------------------------------------------------
// 14：lines() 形状
// ---------------------------------------------------------------------------

#[tokio::test]
async fn account_lines_hold_header_and_steps_line() {
    let accounts = vec![cn("c1")];
    let exec = FakeExecutor::new(accounts.clone());
    let clock = FixedClock;
    let sleeper = NoopSleeper::default();
    let mut deps = ClaimAllDeps::new(&exec, &clock, &sleeper);
    deps.growth_tasks = Some(growth_returning(0));

    let targets = select_targets(&accounts, "");
    let out = wb_tasks::claim_all::run_claim_all(&targets, &deps).await;
    let lines = out.accounts[0].lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "====== [claim-all] c1 (c1) realm=cn ======");
    assert!(
        lines[1].starts_with("  steps="),
        "steps 行以两空格 + `steps=` 开头，实际：{}",
        lines[1]
    );
    // steps 行是 JSON 数组文本，形状等价于 Python repr(list[tuple])
    let parsed: Value = serde_json::from_str(lines[1].trim_start_matches("  steps=")).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 5);
    assert_eq!(parsed[0][0], "checkin");
}

// ---------------------------------------------------------------------------
// 语义小工具
// ---------------------------------------------------------------------------

#[test]
fn truthy_matches_python_bool() {
    assert!(!truthy(None));
    assert!(!truthy(Some(&Value::Null)));
    assert!(!truthy(Some(&json!(false))));
    assert!(truthy(Some(&json!(true))));
    assert!(!truthy(Some(&json!(0))));
    assert!(truthy(Some(&json!(1))));
    assert!(!truthy(Some(&json!(""))));
    assert!(truthy(Some(&json!("x"))));
    assert!(!truthy(Some(&json!([]))));
    assert!(truthy(Some(&json!([0]))));
    assert!(!truthy(Some(&json!({}))));
    assert!(truthy(Some(&json!({"a": 0}))));
}

#[test]
fn ok_or_true_defaults_missing_key_to_true() {
    assert!(ok_or_true(&json!({})));
    assert!(ok_or_true(&json!({"ok": true})));
    assert!(!ok_or_true(&json!({"ok": false})));
    assert!(!ok_or_true(&json!({"ok": null})));
    assert!(!ok_or_true(&json!({"ok": ""})));
}

#[test]
fn msg_or_error_prefers_truthy_msg_then_error() {
    assert_eq!(
        msg_or_error(&json!({"msg": "a", "error": "b"})),
        Some("a".to_string())
    );
    assert_eq!(
        msg_or_error(&json!({"msg": "", "error": "b"})),
        Some("b".to_string())
    );
    assert_eq!(
        msg_or_error(&json!({"msg": null, "error": "b"})),
        Some("b".to_string())
    );
    assert_eq!(msg_or_error(&json!({})), None);
    assert_eq!(msg_or_error(&json!({"msg": "", "error": ""})), None);
}

#[test]
fn uid8_and_display_name_fall_back_to_uid_prefix() {
    assert_eq!(uid8(""), "?");
    assert_eq!(uid8("short"), "short");
    assert_eq!(uid8("0123456789abcdef"), "01234567");

    let acc = cn("0123456789abcdef");
    let empty = HashMap::new();
    assert_eq!(display_name(&acc, &empty), "01234567");
    assert_eq!(
        header_line(&acc, &empty),
        "====== [claim-all] 01234567 (01234567) realm=cn ======"
    );

    let mut named = HashMap::new();
    named.insert("0123456789abcdef".to_string(), "老王".to_string());
    assert_eq!(display_name(&acc, &named), "老王");

    // 空串等价「无昵称」（Python `nickname or ...` 只看真值）
    let mut blank = HashMap::new();
    blank.insert("0123456789abcdef".to_string(), String::new());
    assert_eq!(display_name(&acc, &blank), "01234567");
}

#[test]
fn growth_step_outcome_from_value_uses_python_defaults() {
    let o = GrowthStepOutcome::from_value(&json!({"ok": true, "earned_credit": 42, "logs": ["a"]}));
    assert!(o.ok);
    assert_eq!(o.earned, 42);
    assert_eq!(o.logs, vec!["a".to_string()]);

    // 缺 ok ⇒ True；缺 earned_credit / logs ⇒ 0 / 空
    let o = GrowthStepOutcome::from_value(&json!({}));
    assert!(o.ok);
    assert_eq!(o.earned, 0);
    assert!(o.logs.is_empty());

    // 字符串数字也吃得下
    let o = GrowthStepOutcome::from_value(&json!({"earned_credit": "7"}));
    assert_eq!(o.earned, 7);
}

#[test]
fn claim_step_serializes_as_three_element_tuple() {
    assert_eq!(
        ClaimStep::new("checkin", true, Some("ok".into())).to_json(),
        json!(["checkin", true, "ok"])
    );
    assert_eq!(
        ClaimStep::new("cat_travel", true, None).to_json(),
        json!(["cat_travel", true, null])
    );
}
