//! 关卡 4.10：`ProductionExecutor::daily_chat` 契约测试。
//!
//! 端点：`POST {chat_base}/v2/activity/daily-chat`
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
// 1：成功路径（code=0 带 msg）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_chat_success_returns_ok_and_msg() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/daily-chat"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "msg": "每日活跃打卡成功"
        })))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-chat", "intl");
    let acc = acc_view("u-chat", "intl");
    let res = exec.daily_chat(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        res.get("msg").and_then(Value::as_str),
        Some("每日活跃打卡成功")
    );
}

// ---------------------------------------------------------------------------
// 2：成功路径，上游不给 msg
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_chat_success_no_msg() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/daily-chat"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-chat", "intl");
    let acc = acc_view("u-chat", "intl");
    let res = exec.daily_chat(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(res.get("msg").and_then(Value::as_str), Some(""));
}

// ---------------------------------------------------------------------------
// 3：上游拒绝（code!=0）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_chat_upstream_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/daily-chat"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 1,
            "msg": "今日已打卡"
        })))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-chat", "intl");
    let acc = acc_view("u-chat", "intl");
    let res = exec.daily_chat(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(res.get("error").and_then(Value::as_str), Some("今日已打卡"));
}

// ---------------------------------------------------------------------------
// 4：HTTP 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_chat_http_400_is_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/daily-chat"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-chat", "intl");
    let acc = acc_view("u-chat", "intl");
    let res = exec.daily_chat(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("HTTP 400: bad request")
    );
}

// ---------------------------------------------------------------------------
// 5：账号不找到（不发上游请求）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_chat_account_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/daily-chat"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-chat", "intl");
    let acc = acc_view("no-such-uid", "intl");
    let res = exec.daily_chat(&acc).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("account not found")
    );
    let reqs = server.received_requests().await.unwrap_or_default();
    assert!(reqs.is_empty(), "账号不找到时不得发上游请求");
}

// ---------------------------------------------------------------------------
// 6：请求体与头
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_chat_sends_empty_body_and_chat_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/activity/daily-chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;

    let exec = build_exec(&server, "u-chat", "intl");
    let acc = acc_view("u-chat", "intl");
    let _ = exec.daily_chat(&acc).await;

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
        "Bearer u-chat-tok"
    );
}
