//! 关卡 4.10：`ProductionExecutor::streak_bonus` 契约测试。
//!
//! 端点：`POST {chat_base}/activity/growth/streak/claim`
//! 全程 wiremock + tempdir，**不向真实上游发任何请求**。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
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
// 1：成功路径
// ---------------------------------------------------------------------------

/// 上游返回 code=0、带 data.logs → `ok:true`，透传 logs。
#[tokio::test]
async fn streak_bonus_success_returns_ok_and_logs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"logs": ["连登 +1", "当前连续 5 天"]},
        })))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-streak", "cn");
    let acc = acc_view("u-streak", "cn");
    let res = exec.streak_bonus(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        res.get("logs").and_then(Value::as_array),
        Some(&vec![json!("连登 +1"), json!("当前连续 5 天")])
    );
}

/// 上游返回 code=0 但无 data → `ok:true`，logs 为空。
#[tokio::test]
async fn streak_bonus_success_no_logs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-streak", "cn");
    let acc = acc_view("u-streak", "cn");
    let res = exec.streak_bonus(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        res.get("logs").and_then(Value::as_array),
        Some(&Vec::<Value>::new())
    );
}

// ---------------------------------------------------------------------------
// 2：上游拒绝
// ---------------------------------------------------------------------------

/// 上游返回 code!=0 → `ok:false`，带 error。
#[tokio::test]
async fn streak_bonus_upstream_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 1,
            "msg": "连续登录异常",
        })))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-streak", "cn");
    let acc = acc_view("u-streak", "cn");
    let res = exec.streak_bonus(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("连续登录异常")
    );
}

/// HTTP 400 → `ok:false`，带 HTTP 错误文案。
#[tokio::test]
async fn streak_bonus_http_400_is_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-streak", "cn");
    let acc = acc_view("u-streak", "cn");
    let res = exec.streak_bonus(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("HTTP 400: bad request")
    );
}

// ---------------------------------------------------------------------------
// 3：账号不找到
// ---------------------------------------------------------------------------

/// ready 里不存在该 uid → `ok:false`，error="account not found"，不发上游请求。
#[tokio::test]
async fn streak_bonus_account_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-streak", "cn");
    let acc = acc_view("no-such-uid", "cn");
    let res = exec.streak_bonus(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("account not found")
    );
    let reqs = server.received_requests().await.unwrap_or_default();
    assert!(reqs.is_empty(), "账号不找到时不得发上游请求");
}

// ---------------------------------------------------------------------------
// 4：请求体与头
// ---------------------------------------------------------------------------

/// 请求体为 `{}`，携带 chat 侧 Authorization 头。
#[tokio::test]
async fn streak_bonus_sends_empty_body_and_chat_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-streak", "cn");
    let acc = acc_view("u-streak", "cn");
    let _ = exec.streak_bonus(&acc).await;

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body, b"{}");
    assert_eq!(
        reqs[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer u-streak-tok"
    );
}
