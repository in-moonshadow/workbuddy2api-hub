//! 关卡 4.18：`night_growth::run_night_growth` 的契约测试。
//!
//! 权威源：Python `wb_tasks.run_night_growth`（`wb_tasks.py:712-770`）。
//!
//! ⚠️ **文件名说明**：本 crate 已有 `contract_night_growth.rs`——那是对
//! `api_routes::ProductionExecutor::night_growth`（**路由层占位实现**）的
//! 关卡 4.10 契约测试，路由接线由并行实现的另一条线负责，不在本模块范围内，
//! 故不覆盖它。本文件测的是**任务层**的 `run_night_growth`。
//!
//! 全程 wiremock mock 上游，**绝不向真实上游发任何请求**。

use std::collections::HashMap;

use serde_json::{json, Value};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_gateway::outbound::UpstreamClient;
use wb_tasks::growth_api::{TaskAccount, TaskEndpoints};
use wb_tasks::growth_runner::{Clock, NoopSleeper, RunnerAccount, RunnerCtx};
use wb_tasks::night_growth::{run_night_growth, NightGrowthOutcome};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/// 固定时钟（夜间窗口判定用）。
struct FixedClock(u32);
impl Clock for FixedClock {
    fn local_hour(&self) -> u32 {
        self.0
    }
}

/// 账号身份（uid / token / 头 / realm / 昵称 / 企业标记）。
struct Acc {
    uid: String,
    token: String,
    enterprise_id: String,
    chat_headers: HashMap<String, String>,
    realm: String,
    nickname: Option<String>,
    is_enterprise: bool,
}

impl Acc {
    fn cn() -> Self {
        let mut h = HashMap::new();
        h.insert("X-Domain".into(), "copilot.tencent.com".into());
        Acc {
            uid: "u-night".into(),
            token: "tok".into(),
            enterprise_id: String::new(),
            chat_headers: h,
            realm: "cn".into(),
            nickname: Some("小猫".into()),
            is_enterprise: false,
        }
    }

    fn intl() -> Self {
        let mut a = Acc::cn();
        a.realm = "intl".into();
        a
    }

    fn enterprise() -> Self {
        let mut a = Acc::cn();
        a.enterprise_id = "ent-1".into();
        a.is_enterprise = true;
        a
    }

    fn runner(&self) -> RunnerAccount<'_> {
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

/// 把 chat/web base 都指到同一个 wiremock（泄漏一次字符串换 `'static`）。
/// **绝不向真实上游发请求。**
fn ep(server: &MockServer) -> TaskEndpoints<'static> {
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    TaskEndpoints {
        chat_base: base,
        web_base: base,
    }
}

fn ctx<'a>(sleeper: &'a NoopSleeper, clock: &'a FixedClock) -> RunnerCtx<'a> {
    RunnerCtx { sleeper, clock }
}

/// 任务清单 mock：`black_cat` 的 `accept_status` / `current` / `target` 可配。
fn tasks_body(accept_status: &str, current: i64, target: i64) -> Value {
    json!({
        "code": 0,
        "data": {"tasks": [{
            "task_code": "black_cat",
            "accept_status": accept_status,
            "task_type": "single",
            "progress": {"current": current, "target": target}
        }]}
    })
}

async fn mount_tasks(server: &MockServer, body: Value, times: Option<u64>) {
    let mock = Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body));
    let mock = match times {
        Some(n) => mock.up_to_n_times(n),
        None => mock,
    };
    mock.mount(server).await;
}

async fn mount_accept_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v2/activity/growth/tasks/accept"))
        .and(body_json(json!({"task_codes": ["black_cat"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"results": [{"task_code": "black_cat", "status": "accepted"}]}
        })))
        .mount(server)
        .await;
}

async fn mount_accept_rejected(server: &MockServer, msg: &str) {
    Mock::given(method("POST"))
        .and(path("/v2/activity/growth/tasks/accept"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 1, "msg": msg})))
        .mount(server)
        .await;
}

async fn mount_report(server: &MockServer, code: i64) {
    Mock::given(method("POST"))
        .and(path("/v2/report"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": code})))
        .mount(server)
        .await;
}

async fn mount_claim(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/activity/growth/tasks/black_cat/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// 跑一次夜猫子任务，返回 `(outcome, 刷新积分次数, 实际发出的路径)`。
async fn run(
    server: &MockServer,
    acc: &Acc,
    hour: u32,
) -> (NightGrowthOutcome, usize, Vec<String>) {
    let sleeper = NoopSleeper::default();
    let clock = FixedClock(hour);
    let c = ctx(&sleeper, &clock);
    let mut refresh_count = 0usize;
    let mut refresh = || {
        refresh_count += 1;
        Some(json!({"remain": 42}))
    };
    let out = run_night_growth(
        &UpstreamClient::new(),
        &acc.runner(),
        &ep(server),
        &c,
        &mut refresh,
    )
    .await;
    let paths = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    (out, refresh_count, paths)
}

// ---------------------------------------------------------------------------
// 1：国际版短路（不发任何请求）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn intl_realm_short_circuits_without_any_request() {
    let server = MockServer::start().await;
    let (out, refresh, paths) = run(&server, &Acc::intl(), 2).await;
    assert!(!out.ok);
    assert_eq!(out.msg.as_deref(), Some("国际版不适用国内成长任务中心"));
    assert!(out.logs.is_empty());
    assert_eq!(out.earned_credit, 0);
    assert_eq!(refresh, 0);
    assert!(paths.is_empty(), "国际版不应发出任何请求：{paths:?}");
    // `msg` 键存在，且键序为 ok, msg, logs, earned_credit。
    let j = out.to_json();
    assert_eq!(
        j.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["ok", "msg", "logs", "earned_credit"]
    );
}

// ---------------------------------------------------------------------------
// 2：企业版短路
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enterprise_short_circuits_without_any_request() {
    let server = MockServer::start().await;
    let (out, refresh, paths) = run(&server, &Acc::enterprise(), 2).await;
    assert!(out.ok);
    assert_eq!(out.msg.as_deref(), Some("企业版账号无成长体系，跳过"));
    assert!(out.logs.is_empty());
    assert_eq!(refresh, 0);
    assert!(paths.is_empty(), "企业版不应发出任何请求：{paths:?}");
}

// ---------------------------------------------------------------------------
// 3：夜间窗口外
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outside_night_window_skips_and_reports_the_reason() {
    let server = MockServer::start().await;
    let (out, refresh, paths) = run(&server, &Acc::cn(), 10).await;
    assert!(out.ok);
    assert_eq!(out.msg, None, "窗口外分支不带 msg 键");
    assert_eq!(out.earned_credit, 0);
    assert_eq!(
        out.logs,
        vec!["[小猫] 当前不在 23:00-08:00 夜间窗口, 夜猫子任务跳过 (每日 01:00 自动执行)"]
    );
    assert_eq!(refresh, 0);
    assert!(paths.is_empty(), "窗口外不应拉清单：{paths:?}");
    // 不带 msg 的键序：ok, earned_credit, logs。
    assert_eq!(
        out.to_json()
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["ok", "earned_credit", "logs"]
    );
}

/// Python `in_night_window()`：`h >= 23 or h < 8`。
#[tokio::test]
async fn night_window_boundaries_match_python() {
    for (hour, in_window) in [(0u32, true), (7, true), (8, false), (22, false), (23, true)] {
        let server = MockServer::start().await;
        mount_tasks(&server, tasks_body("claimed", 0, 3), None).await;
        let (out, _, _) = run(&server, &Acc::cn(), hour).await;
        // 窗口内会走到「已领奖」分支（logs 非空且文案不同）；窗口外走「跳过」分支。
        let skipped = out
            .logs
            .first()
            .map(|l| l.contains("当前不在 23:00-08:00 夜间窗口"))
            .unwrap_or(false);
        assert_eq!(skipped, !in_window, "hour={hour} 窗口判定与 Python 不符");
    }
}

// ---------------------------------------------------------------------------
// 4：拉不到夜猫子任务
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_black_cat_task_reports_failure() {
    let server = MockServer::start().await;
    mount_tasks(&server, json!({"code": 0, "data": {"tasks": []}}), None).await;
    let (out, refresh, _) = run(&server, &Acc::cn(), 2).await;
    assert!(!out.ok);
    assert_eq!(out.msg, None);
    assert_eq!(out.earned_credit, 0);
    assert_eq!(out.logs, vec!["[小猫] 未获取到夜猫子任务清单"]);
    assert_eq!(refresh, 0);
}

// ---------------------------------------------------------------------------
// 5：已领奖
// ---------------------------------------------------------------------------

#[tokio::test]
async fn already_claimed_task_short_circuits() {
    let server = MockServer::start().await;
    mount_tasks(&server, tasks_body("claimed", 3, 3), None).await;
    let (out, refresh, paths) = run(&server, &Acc::cn(), 2).await;
    assert!(out.ok);
    assert_eq!(out.earned_credit, 0);
    assert_eq!(out.logs, vec!["[小猫] 夜猫子任务已领奖"]);
    assert_eq!(refresh, 0);
    assert_eq!(paths, vec!["/v2/activity/growth/tasks"]);
}

// ---------------------------------------------------------------------------
// 6：接取失败
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accept_failure_reports_the_reason() {
    let server = MockServer::start().await;
    mount_tasks(&server, tasks_body("not_accepted", 0, 3), None).await;
    mount_accept_rejected(&server, "任务已满").await;
    let (out, refresh, _) = run(&server, &Acc::cn(), 2).await;
    assert!(!out.ok);
    assert_eq!(out.msg, None);
    assert_eq!(out.earned_credit, 0);
    assert_eq!(
        out.logs,
        vec!["[小猫] 夜猫子任务未接取, 今晚跳过 (先解决接取失败: 任务已满)"]
    );
    assert_eq!(refresh, 0);
}

// ---------------------------------------------------------------------------
// 7：上报事件 → 轮询到进度 +1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reports_event_and_polls_until_progress_appears() {
    let server = MockServer::start().await;
    // 首次拉取：current=0；之后轮询拿到 current=1。
    mount_tasks(&server, tasks_body("not_accepted", 0, 3), Some(1)).await;
    mount_tasks(&server, tasks_body("accepted", 1, 3), None).await;
    mount_accept_ok(&server).await;
    mount_report(&server, 0).await;
    let (out, refresh, paths) = run(&server, &Acc::cn(), 2).await;
    assert!(out.ok);
    assert_eq!(out.msg, None);
    assert_eq!(out.earned_credit, 0);
    assert_eq!(
        out.logs,
        vec![
            "[小猫] 夜猫子任务已接取",
            "[小猫] 上报夜间 GLM-5.2 对话事件: 成功 (进度 0/3)",
            "[小猫] 今晚 +1 (1/3), 明晚继续, 累计 3 天可领奖",
        ]
    );
    assert_eq!(refresh, 1, "尾部要刷新一次积分余额");
    assert!(paths.contains(&"/v2/report".to_string()), "{paths:?}");
    assert!(paths.contains(&"/v2/activity/growth/tasks/accept".to_string()));
}

/// 上报失败必须**可见**（Python：日志里写「失败」）。
#[tokio::test]
async fn event_report_failure_is_visible_in_logs() {
    let server = MockServer::start().await;
    mount_tasks(&server, tasks_body("accepted", 0, 3), None).await;
    mount_report(&server, 1).await;
    let (out, _, _) = run(&server, &Acc::cn(), 2).await;
    assert!(
        out.logs
            .iter()
            .any(|l| l.contains("上报夜间 GLM-5.2 对话事件: 失败 (进度 0/3)")),
        "logs = {:?}",
        out.logs
    );
    assert!(
        out.logs
            .iter()
            .any(|l| l.contains("已上报但进度暂未变化 (0/3)")),
        "logs = {:?}",
        out.logs
    );
}

// ---------------------------------------------------------------------------
// 8：轮询到达标 → 领奖
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reaching_target_during_polling_claims_the_reward() {
    let server = MockServer::start().await;
    mount_tasks(&server, tasks_body("accepted", 0, 3), Some(1)).await;
    mount_tasks(&server, tasks_body("accepted", 3, 3), None).await;
    mount_report(&server, 0).await;
    mount_claim(
        &server,
        json!({"code": 0, "data": {"credit": 50, "energy": 0}}),
    )
    .await;
    let (out, refresh, paths) = run(&server, &Acc::cn(), 2).await;
    assert!(out.ok);
    // earned_credit 由日志里的 `+50 积分` 反推。
    assert_eq!(out.earned_credit, 50);
    assert_eq!(
        out.logs.last().unwrap(),
        "[小猫] 夜猫子任务完成 3/3, 领奖 ✓ +50 积分"
    );
    assert_eq!(refresh, 1);
    assert!(paths.contains(&"/activity/growth/tasks/black_cat/claim".to_string()));
}

// ---------------------------------------------------------------------------
// 9：进入时已达标 → 直接领奖（不等轮询）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn already_at_target_claims_immediately() {
    let server = MockServer::start().await;
    mount_tasks(&server, tasks_body("accepted", 3, 3), Some(1)).await;
    mount_claim(
        &server,
        json!({"code": 0, "data": {"credit": 30, "energy": 0}}),
    )
    .await;
    let (out, refresh, paths) = run(&server, &Acc::cn(), 2).await;
    assert!(out.ok);
    assert_eq!(out.earned_credit, 30);
    assert_eq!(out.logs, vec!["[小猫] 夜猫子任务达标, 领奖 ✓ +30 积分"]);
    assert_eq!(refresh, 1);
    assert!(
        !paths.contains(&"/v2/report".to_string()),
        "已达标时不应上报事件：{paths:?}"
    );
}

// ---------------------------------------------------------------------------
// 10：领奖失败 → ok=false、积分 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_failure_keeps_zero_credit_and_reports_the_reason() {
    let server = MockServer::start().await;
    mount_tasks(&server, tasks_body("accepted", 3, 3), Some(1)).await;
    mount_claim(&server, json!({"code": 1, "msg": "进度未落账"})).await;
    let (out, _, _) = run(&server, &Acc::cn(), 2).await;
    assert!(!out.ok);
    assert_eq!(out.earned_credit, 0);
    assert_eq!(
        out.logs,
        vec!["[小猫] 夜猫子任务达标, 领奖 ! 失败: 进度未落账"]
    );
}

// ---------------------------------------------------------------------------
// 11：轮询次数固定 4 轮（每轮 sleep 4 秒）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn polling_runs_at_most_four_rounds_with_four_second_gaps() {
    let server = MockServer::start().await;
    // 进度**一直不变** → 走满 4 轮。
    mount_tasks(&server, tasks_body("accepted", 0, 3), None).await;
    mount_report(&server, 0).await;
    let sleeper = NoopSleeper::default();
    let clock = FixedClock(2);
    let c = ctx(&sleeper, &clock);
    let mut refresh = || None;
    let acc = Acc::cn();
    let out = run_night_growth(
        &UpstreamClient::new(),
        &acc.runner(),
        &ep(&server),
        &c,
        &mut refresh,
    )
    .await;
    assert!(out.ok);
    assert_eq!(sleeper.calls(), vec![4.0, 4.0, 4.0, 4.0]);
    assert!(
        out.logs
            .iter()
            .any(|l| l.contains("已上报但进度暂未变化 (0/3)")),
        "logs = {:?}",
        out.logs
    );
}

// ---------------------------------------------------------------------------
// 12：R1 —— 旅行状态路径必须逐字含 `/buddy/`
// ---------------------------------------------------------------------------

/// R1：`_travel_status` 走 `/activity/growth/buddy/travel/status`（**无** `/v2`）。
///
/// 该路径此前在 Rust 侧漏了 `/buddy/`，且旧单测把错误路径当期望值 —— 这里用
/// wiremock **实际收到的路径**做断言，防止再漂。
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
    let acc = Acc::cn();
    let sleeper = NoopSleeper::default();
    let clock = FixedClock(2);
    let c = ctx(&sleeper, &clock);
    let mut logs = Vec::new();
    let res = wb_tasks::growth_runner::build_cat_travel(
        &UpstreamClient::new(),
        &acc.runner(),
        &ep(&server),
        &mut logs,
        &c,
    )
    .await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["action"], "traveling");
    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(
        paths,
        vec!["/activity/growth/buddy/travel/status".to_string()],
        "旅行状态路径必须逐字等于 Python 的 /activity/growth/buddy/travel/status"
    );
}
