//! 关卡 4.10：`ProductionExecutor::night_growth` 契约测试（**执行器层**）。
//!
//! 任务层（`run_night_growth` 的八个分支）由 `contract_night_growth_task.rs`
//! 覆盖——那里能注入固定时钟与不睡的 sleeper；本文件只锁定**执行器接线**：
//! 账号查找、`realm` / 企业标记的传递、端点确实指向 mock、以及返回体的键集。
//!
//! ⚠️ 夜猫子只在 **23:00-08:00** 的窗口内执行，窗口判定读的是
//! `TaskRuntime` 的时钟 ⇒ 本文件用 `TaskRuntime::test_with_ready_and_clock`
//! 注入固定时钟，断言与真实时间无关（否则白天跑必挂、夜里跑又慢）。
//!
//! 全程 wiremock + tempdir，**不向真实上游发任何请求**。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_tasks::api_routes::{PooledTaskAccount, ReadyAccount, TaskRuntime};
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::growth_runner::Clock;
use wb_tasks::scheduler::{AccountView, CycleExecutor, PanelSink};

struct NullPanel;

impl PanelSink for NullPanel {
    fn add_log_entry(&self, _msg: &str, _tag: &str) {}
}

/// 固定时钟：夜猫子窗口判定只看 `local_hour`。
struct FixedClock(u32);

impl Clock for FixedClock {
    fn local_hour(&self) -> u32 {
        self.0
    }
}

/// 夜猫子窗口内的固定小时（23 点）。
const NIGHT_HOUR: u32 = 23;
/// 夜猫子窗口外的固定小时（12 点）。
const DAY_HOUR: u32 = 12;

/// 造一个账号（`realm` / 企业标记可控）。
fn account(uid: &str, realm: &str, enterprise: bool) -> ReadyAccount {
    let mut state = wb_gateway::pool::AccountState::new(uid, realm);
    state.access_token = format!("{uid}-tok");
    state.enterprise_id = if enterprise {
        format!("ent-{uid}")
    } else {
        String::new()
    };
    let view = AccountView::from_state(&state).with_last_records(None, None, None, false);
    let mut chat_headers = HashMap::new();
    chat_headers.insert("Authorization".into(), format!("Bearer {uid}-tok"));
    let creds = PooledTaskAccount {
        uid: uid.into(),
        token: format!("{uid}-tok"),
        enterprise_id: state.enterprise_id.clone(),
        realm: realm.into(),
        chat_headers,
    };
    ReadyAccount {
        state,
        view,
        creds,
        nickname: None,
        device_token: None,
    }
}

/// 构建指向 wiremock 的执行器，注入固定时钟。
fn build_exec(
    server: &MockServer,
    uid: &str,
    realm: &str,
    enterprise: bool,
    hour: u32,
) -> Arc<dyn CycleExecutor> {
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    let endpoints = TaskEndpoints {
        chat_base: base,
        web_base: base,
    };
    let ready = vec![account(uid, realm, enterprise)];
    TaskRuntime::test_with_ready_and_clock(
        ready,
        endpoints,
        Arc::new(NullPanel),
        Arc::new(FixedClock(hour)),
    )
}

/// 该账号的 `AccountView`（执行器按 `uid` 找回 ready 里的账号）。
fn acc_view(acc: &ReadyAccount) -> AccountView {
    acc.view.clone()
}

// ---------------------------------------------------------------------------
// fixture：night task black_cat
// ---------------------------------------------------------------------------

/// 任务清单：`black_cat` 已达标（`current >= target`）⇒ 走「直接领奖」分支。
///
/// 该分支**不轮询**，因此不触发 `sleep(4)` 的轮询等待。
fn night_task_at_target() -> Value {
    json!({
        "code": 0,
        "data": {"tasks": [{
            "task_code": "black_cat",
            "accept_status": "completed",
            "task_type": "single",
            "progress": {"current": 3, "target": 3}
        }]}
    })
}

/// 挂「拉清单」mock。
async fn mount_tasks(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// 挂「领奖」mock。
async fn mount_claim(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/activity/growth/tasks/black_cat/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// 该 mock 上**不应**被碰到的端点。
async fn mount_forbidden(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v2/activity/growth/tasks/accept"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/tasks/black_cat/claim"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// 1：已达标 ⇒ 直接领奖（返回体键集：`ok` / `earned_credit` / `logs`，无 `msg`）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_claims_when_already_at_target() {
    let server = MockServer::start().await;
    mount_tasks(&server, night_task_at_target()).await;
    mount_claim(
        &server,
        json!({"code": 0, "data": {"credit": 100, "energy": 0}}),
    )
    .await;

    let exec = build_exec(&server, "u-night", "cn", false, NIGHT_HOUR);
    let acc = acc_view(&account("u-night", "cn", false));
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    // Python 的键名是 `earned_credit`（不是旧实现的 `credit`）。
    assert_eq!(res.get("earned_credit").and_then(Value::as_i64), Some(100));
    assert!(res.get("credit").is_none(), "旧实现的键名不该再出现：{res}");
    // 非国际版 / 非企业版两档**没有** `msg` 键（Python 字面量如此）。
    assert!(res.get("msg").is_none(), "{res}");
    let logs = res.get("logs").and_then(Value::as_array).unwrap();
    assert!(
        logs.iter()
            .any(|v| v.as_str().unwrap_or_default().contains("领奖 ✓ +100 积分")),
        "logs = {logs:?}"
    );

    // 端点确实指向 mock（没有打真实上游），且只碰了「拉清单 + 领奖」两步。
    let reqs = server.received_requests().await.expect("requests");
    let paths: Vec<String> = reqs.iter().map(|r| r.url.path().to_string()).collect();
    assert!(
        paths.contains(&"/v2/activity/growth/tasks".to_string()),
        "paths = {paths:?}"
    );
    assert!(
        paths.contains(&"/activity/growth/tasks/black_cat/claim".to_string()),
        "paths = {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// 2：领奖失败 ⇒ ok=false、积分 0，日志写明原因（不中断）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_claim_failure_reports_reason() {
    let server = MockServer::start().await;
    mount_tasks(&server, night_task_at_target()).await;
    mount_claim(&server, json!({"code": 1, "msg": "已领取"})).await;

    let exec = build_exec(&server, "u-night", "cn", false, NIGHT_HOUR);
    let acc = acc_view(&account("u-night", "cn", false));
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(res.get("earned_credit").and_then(Value::as_i64), Some(0));
    let logs = res.get("logs").and_then(Value::as_array).unwrap();
    assert!(
        logs.iter()
            .any(|v| v.as_str().unwrap_or_default().contains("领奖 ! 失败")),
        "logs = {logs:?}"
    );
}

// ---------------------------------------------------------------------------
// 3：不在夜间窗口 ⇒ 诚实跳过，且**不发任何上游请求**
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_outside_night_window_skips_without_request() {
    let server = MockServer::start().await;
    mount_forbidden(&server).await;

    let exec = build_exec(&server, "u-night", "cn", false, DAY_HOUR);
    let acc = acc_view(&account("u-night", "cn", false));
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(res.get("earned_credit").and_then(Value::as_i64), Some(0));
    let logs = res.get("logs").and_then(Value::as_array).unwrap();
    assert!(
        logs.iter()
            .any(|v| v.as_str().unwrap_or_default().contains("夜间窗口")),
        "logs = {logs:?}"
    );

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 0, "窗口外不得发任何上游请求");
}

// ---------------------------------------------------------------------------
// 4：国际版 ⇒ 短路（`ok:false` + `msg`），不发任何上游请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_intl_account_short_circuits() {
    let server = MockServer::start().await;
    mount_forbidden(&server).await;

    let exec = build_exec(&server, "u-intl", "intl", false, NIGHT_HOUR);
    let acc = acc_view(&account("u-intl", "intl", false));
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("msg").and_then(Value::as_str),
        Some("国际版不适用国内成长任务中心")
    );
    assert_eq!(
        res.get("logs").and_then(Value::as_array),
        Some(&Vec::<Value>::new())
    );
    assert_eq!(res.get("earned_credit").and_then(Value::as_i64), Some(0));

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 0, "国际版不得发任何上游请求");
}

// ---------------------------------------------------------------------------
// 5：企业版 ⇒ 短路（`ok:true` + `msg`），不发任何上游请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_enterprise_account_short_circuits() {
    let server = MockServer::start().await;
    mount_forbidden(&server).await;

    let exec = build_exec(&server, "u-ent", "cn", true, NIGHT_HOUR);
    let acc = acc_view(&account("u-ent", "cn", true));
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        res.get("msg").and_then(Value::as_str),
        Some("企业版账号无成长体系，跳过")
    );
    assert_eq!(
        res.get("logs").and_then(Value::as_array),
        Some(&Vec::<Value>::new())
    );
    assert_eq!(res.get("earned_credit").and_then(Value::as_i64), Some(0));

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 0, "企业版不得发任何上游请求");
}

// ---------------------------------------------------------------------------
// 6：账号不找到（执行器特有的分支）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_account_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-night", "cn", false, NIGHT_HOUR);
    let acc = acc_view(&account("no-such-uid", "cn", false));
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("account not found")
    );
    assert_eq!(
        res.get("logs").and_then(Value::as_array),
        Some(&Vec::<Value>::new())
    );
}
