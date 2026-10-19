//! 关卡 4.10：`ProductionExecutor::night_growth` 契约测试。
//!
//! 端点：`POST {chat_base}/v2/activity/growth/tasks/accept`  +
//! `POST {chat_base}/activity/growth/tasks/{code}/claim`
//! 全程 wiremock + tempdir，**不向真实上游发任何请求**。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_tasks::api_routes::{PooledTaskAccount, ReadyAccount, TaskRuntime};
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::scheduler::{AccountView, CycleExecutor, PanelSink};

struct NullPanel;

impl PanelSink for NullPanel {
    fn add_log_entry(&self, _msg: &str, _tag: &str) {}
}

/// 构建指向 wiremock 的 TaskRuntime 并取出其 `exec`（ProductionExecutor）。
/// `TaskRuntime::new` 已把 `ProductionExecutor` 设入 `exec` OnceLock，持有同一
/// `Arc<TaskRuntime>`；测试只需把测试账号写入 `ready`，executor 即可见。
fn build_exec(server: &MockServer, uid: &str, realm: &str) -> Arc<dyn CycleExecutor> {
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    let endpoints = TaskEndpoints {
        chat_base: base,
        web_base: base,
    };

    let mut chat_headers = HashMap::new();
    chat_headers.insert("Authorization".into(), format!("Bearer {uid}-tok"));

    let mut state = wb_gateway::pool::AccountState::new(uid, realm);
    state.access_token = format!("{uid}-tok");
    let view = AccountView::from_state(&state).with_last_records(None, None, None, false);
    let creds = PooledTaskAccount {
        uid: uid.into(),
        token: format!("{uid}-tok"),
        enterprise_id: "".into(),
        chat_headers,
    };
    let ready = vec![ReadyAccount {
        state,
        view,
        creds,
        nickname: None,
        device_token: None,
    }];

    TaskRuntime::test_with_ready(ready, endpoints, Arc::new(NullPanel))
}

fn acc_view(uid: &str, realm: &str) -> AccountView {
    let mut s = wb_gateway::pool::AccountState::new(uid, realm);
    s.access_token = "tok".into();
    AccountView::from_state(&s).with_last_records(None, None, None, false)
}

// ---------------------------------------------------------------------------
// fixture：night task black_cat
// ---------------------------------------------------------------------------

fn night_task_not_accepted() -> Value {
    json!({
        "code": 0,
        "data": {"tasks": [{
            "task_code": "black_cat",
            "accept_status": "not_accepted",
            "task_type": "single",
            "progress": {"current": 0, "target": 3}
        }]}
    })
}

fn night_task_completed() -> Value {
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

fn no_night_tasks() -> Value {
    json!({"code": 0, "data": {"tasks": []}})
}

// ---------------------------------------------------------------------------
// 1：成功路径（未接取 → 接取 → 重取已完成 → 领奖）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_accept_then_claim_happy_path() {
    let server = MockServer::start().await;
    // 第一次拉取：black_cat 尚未接取
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(night_task_not_accepted()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // 接取
    Mock::given(method("POST"))
        .and(path("/v2/activity/growth/tasks/accept"))
        .and(body_json(json!({"task_codes": ["black_cat"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"results": [{"task_code": "black_cat", "status": "accepted"}]}
        })))
        .mount(&server)
        .await;
    // 第二次拉取：black_cat 已完成
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(night_task_completed()))
        .mount(&server)
        .await;
    // 领奖
    Mock::given(method("POST"))
        .and(path("/activity/growth/tasks/black_cat/claim"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"credit": 100}})),
        )
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-night", "cn");
    let acc = acc_view("u-night", "cn");
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    let logs = res.get("logs").and_then(Value::as_array).unwrap();
    assert!(
        logs.iter()
            .any(|v| v.as_str().unwrap_or_default().contains("领奖成功")),
        "logs = {logs:?}"
    );
    assert_eq!(res.get("credit").and_then(Value::as_i64), Some(100));
}

// ---------------------------------------------------------------------------
// 2：无 night 任务
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_no_night_task() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(no_night_tasks()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/growth/tasks/accept"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/tasks/black_cat/claim"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-night", "cn");
    let acc = acc_view("u-night", "cn");
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        res.get("logs").and_then(Value::as_array),
        Some(&Vec::<Value>::new())
    );
}

// ---------------------------------------------------------------------------
// 3：账号不找到
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

    let exec = build_exec(&server, "u-night", "cn");
    let acc = acc_view("no-such-uid", "cn");
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

// ---------------------------------------------------------------------------
// 4：领奖失败（不中断，ok:true 且带失败日志）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn night_growth_claim_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(night_task_not_accepted()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/growth/tasks/accept"))
        .and(body_json(json!({"task_codes": ["black_cat"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"results": [{"task_code": "black_cat", "status": "accepted"}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/activity/growth/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(night_task_completed()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/tasks/black_cat/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 1, "msg": "已领取"})))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-night", "cn");
    let acc = acc_view("u-night", "cn");
    let res = exec.night_growth(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    let logs = res.get("logs").and_then(Value::as_array).unwrap();
    assert!(
        logs.iter()
            .any(|v| v.as_str().unwrap_or_default().contains("领奖失败")),
        "logs = {logs:?}"
    );
}
