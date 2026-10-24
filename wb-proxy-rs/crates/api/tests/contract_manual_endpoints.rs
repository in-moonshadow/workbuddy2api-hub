//! 关卡 4.20（D 线）：两个**手动**每日活跃端点的契约测试。
//!
//! Python 真源：
//! - `wb_proxy.py:17549 _route_accounts_daily_chat`（完整打卡：桌面腿 + 网页腿）
//! - `wb_proxy.py:17568 _route_accounts_daily_chat_web`（**只**网页腿）
//!
//! 必测语义：
//! 1. 网页通道手动打卡**不写** `lastDailyChat`——那是「今天已经打过卡」的闸门，
//!    手动补一次不该让定时巡检跳过当天的正常流程（Python docstring 的原话）。
//! 2. 完整手动打卡（两腿都成）**会写** `lastDailyChat`——两条端点的差别就在这里。
//! 3. 目标选择：给了 `uid` 就只打那一个；没给就只打 `realm == "intl" && enabled`。
//! 4. `uid` 指向国内版账号 ⇒ 与 Python 同样的错误串（realm 闸门在业务层）。
//! 5. 未知 `uid` ⇒ 空结果，且**不发任何上游请求**。
//!
//! 全程 wiremock 假上游 + tempdir 账号池；绝不向真实上游发请求。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_gateway::pool::AccountState;
use wb_tasks::api_routes::{PooledTaskAccount, ReadyAccount, TaskRuntime};
use wb_tasks::daily_chat::DailyChatConfig;
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::scheduler::AccountView;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCOUNT_NAME: &str = "acct.json";
const CONV: &str = "conv-1";

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        // 账号目录放在 tempdir **里面**的 `accounts/`：活动历史与用量目录都
        // 是从账号目录的**同级**目录推导的（`usage/` 与 `accounts/` 同级），
        // 直接用 tempdir 根目录会推出 `/tmp/usage` ⇒ 测试往 tempdir 外写。
        accounts_dir: accounts_dir(dir.path()),
        dashboard: dir.path().to_path_buf(),
        api_key: Some("test-key".to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "cn".to_string(),
    })
}

/// 建账号目录：一个账号文件（`{}`）+ 可选的 `settings.json`。
fn account_dir(daily_chat_web: Option<bool>) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(accounts_dir(dir.path()).join(ACCOUNT_NAME), "{}").expect("seed account file");
    if let Some(flag) = daily_chat_web {
        std::fs::write(
            accounts_dir(dir.path()).join("settings.json"),
            json!({"daily_chat_web": flag}).to_string(),
        )
        .expect("write settings");
    }
    dir
}

/// 读 `lastDailyChat`（缺失 → None）。
fn read_last_daily_chat(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(ACCOUNT_NAME)).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    doc.get("lastDailyChat")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 造一个账号（`credential_file` 指向 `acct.json`，幂等字段回写才有落点）。
fn account(uid: &str, realm: &str, enabled: bool) -> ReadyAccount {
    let mut state = AccountState::new(uid, realm);
    state.access_token = format!("{uid}-tok");
    state.enabled = enabled;
    state.credential_file = ACCOUNT_NAME.into();
    let view = AccountView::from_state(&state).with_last_records(None, None, None, false);
    let mut chat_headers = HashMap::new();
    chat_headers.insert("Authorization".into(), format!("Bearer {uid}-tok"));
    let creds = PooledTaskAccount {
        uid: uid.into(),
        token: format!("{uid}-tok"),
        enterprise_id: "".into(),
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

/// 构建指向 wiremock base 的运行时并一次性注入 `ready` 账号。
///
/// 同函数内 `TaskRuntime` 仅由返回的 `Arc` 唯一强持有（`ProductionExecutor` 内
/// 仅持 `Weak<TaskRuntime>`），故用裸指针独占改写 `pool_present`/`ready`/
/// `daily_chat_config`，避开 `Arc::get_mut` 在本 toolchain 上的反常行为。
fn manual_runtime(dir: &TempDir, base: &'static str, ready: Vec<ReadyAccount>) -> Arc<TaskRuntime> {
    let endpoints = TaskEndpoints {
        chat_base: base,
        web_base: base,
    };
    let task = TaskRuntime::new(
        &accounts_dir(dir.path()),
        Arc::new(ApiPanelSink),
        endpoints,
        "http://127.0.0.1:1".to_string(),
        None,
    );
    // SAFETY: strong_count == 1，同一线程内独占访问，无并发数据竞争。
    let inner = unsafe { &mut *(Arc::as_ptr(&task) as *mut TaskRuntime) };
    inner.pool_present = true;
    inner.ready = ready;
    // 两条腿都指向 mock 上游（硬约束：不得对真实上游发任何请求）。
    inner.daily_chat_config = DailyChatConfig {
        chat_base: Some(base.to_string()),
        web_base: Some(base.to_string()),
        web_turn_timeout: 5,
        web_poll_interval: 0,
    };
    task
}

fn app_for(dir: &TempDir, task: Arc<TaskRuntime>) -> (axum::Router, String) {
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    (app_with(make_cfg(dir), panels, task), token)
}

fn post_req(uri: &str, body: Value, panel_token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", panel_token)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn post_json(
    app: axum::Router,
    uri: &str,
    body: Value,
    panel_token: &str,
) -> (StatusCode, Value) {
    let resp = app.oneshot(post_req(uri, body, panel_token)).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// 挂桌面腿的 mock（`POST {base}/v2/chat/completions`）。
async fn mount_desktop(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
        .mount(server)
        .await;
}

/// 挂一条**完整可用**的网页腿：建会话 → 取沙箱 → ACP → 状态 completed。
async fn mount_web_ok(server: &MockServer, base: &str) {
    Mock::given(method("POST"))
        .and(path("/console/as/conversations/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"id": CONV}})),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/console/as/conversations/{CONV}/session")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "link": format!("{base}/acp"),
                "token": "sandbox-tok",
                "sessionId": "sess-1",
                "cwd": "/workspace"
            }
        })))
        .mount(server)
        .await;

    // ACP 的 SSE 通道：必须回 `Acp-Connection-Id`，否则客户端起不来。
    Mock::given(method("GET"))
        .and(path("/acp"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Acp-Connection-Id", "conn-1")
                .set_body_string(
                    "data: {\"method\":\"session/update\",\"params\":{\"update\":\
{\"sessionUpdate\":\"agent_message_chunk\"}}}\n\n",
                ),
        )
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/acp"))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/console/as/conversations/{CONV}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"status": "completed"}})),
        )
        .mount(server)
        .await;
}

/// 网页腿「建会话就失败」的 mock。
async fn mount_web_broken(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/console/as/conversations/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// 1：网页通道手动打卡**不写** lastDailyChat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_web_chat_does_not_write_last_daily_chat() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![account("u-web", "intl", true)],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(app, "/accounts/daily-chat-web", json!({}), &token).await;
    assert_eq!(status, StatusCode::OK);
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["uid"], "u-web");
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["conversation"], CONV);
    assert!(v["accounts"].is_array(), "信封要带 accounts");

    assert!(
        read_last_daily_chat(&accounts_dir(dir.path())).is_none(),
        "网页通道手动打卡刻意不写 lastDailyChat（否则定时巡检会跳过当天流程）"
    );
}

// ---------------------------------------------------------------------------
// 2：完整手动打卡（两腿都成）**会写** lastDailyChat
//    —— 这正是两条端点的唯一差别
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_full_chat_writes_last_daily_chat_when_web_ok() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_desktop(&server, 200, "{}").await;
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![account("u-full", "intl", true)],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(app, "/accounts/daily-chat", json!({}), &token).await;
    assert_eq!(status, StatusCode::OK);
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], true);

    assert!(
        read_last_daily_chat(&accounts_dir(dir.path())).is_some(),
        "完整手动打卡在网页腿成功时必须写 lastDailyChat（与 Python 一致）"
    );
}

// ---------------------------------------------------------------------------
// 3：目标选择 —— 没给 uid 时只打 intl 且 enabled 的账号
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_web_chat_without_uid_targets_only_intl_enabled() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![
            account("u-cn", "cn", true),
            account("u-intl", "intl", true),
            account("u-intl-off", "intl", false),
        ],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(app, "/accounts/daily-chat-web", json!({}), &token).await;
    assert_eq!(status, StatusCode::OK);
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1, "只应打 intl 且 enabled 的账号");
    assert_eq!(results[0]["uid"], "u-intl");
}

// ---------------------------------------------------------------------------
// 4：给了 uid 就只打那一个（uid 是停用账号也照样打，与 Python 的
//    `POOL.get(uid)` 一致：不按 realm/enabled 过滤）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_web_chat_with_uid_targets_only_that_account() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![
            account("u-intl", "intl", true),
            account("u-intl-off", "intl", false),
        ],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(
        app,
        "/accounts/daily-chat-web",
        json!({"uid": "u-intl-off"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["uid"], "u-intl-off");
}

// ---------------------------------------------------------------------------
// 5：uid 指向国内版账号 ⇒ 与 Python 同样的 realm 错误串（且不发上游请求）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_web_chat_on_cn_account_returns_realm_error() {
    let server = MockServer::start().await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![account("u-cn", "cn", true)],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(
        app,
        "/accounts/daily-chat-web",
        json!({"uid": "u-cn"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], false);
    assert_eq!(
        results[0]["error"],
        "web daily chat is only for international accounts"
    );

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 0, "realm 不对时不得发任何上游请求");
}

// ---------------------------------------------------------------------------
// 6：未知 uid ⇒ 空结果，且不发任何上游请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_web_chat_unknown_uid_returns_empty_results() {
    let server = MockServer::start().await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![account("u-intl", "intl", true)],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(
        app,
        "/accounts/daily-chat-web",
        json!({"uid": "ghost"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["results"].as_array().unwrap().len(), 0);

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 0, "未知 uid 不得发任何上游请求");
}

// ---------------------------------------------------------------------------
// 7：网页腿失败 ⇒ 结果里带 error（面板据此显示失败原因）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_web_chat_failure_is_reported_in_result() {
    let server = MockServer::start().await;
    mount_web_broken(&server).await;

    let dir = account_dir(None);
    let task = manual_runtime(
        &dir,
        Box::leak(server.uri().into_boxed_str()),
        vec![account("u-web", "intl", true)],
    );
    let (app, token) = app_for(&dir, task);

    let (status, v) = post_json(app, "/accounts/daily-chat-web", json!({}), &token).await;
    assert_eq!(status, StatusCode::OK);
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], false);
    assert!(
        results[0]["error"].is_string(),
        "失败原因要出现在 error 字段：{results:?}"
    );
    assert!(
        read_last_daily_chat(&accounts_dir(dir.path())).is_none(),
        "失败时更不能写 lastDailyChat"
    );
}

// ---------------------------------------------------------------------------
// 9：/panel/status 不用会话也能问（Python 的 `_is_panel_route` 不含 `/panel`）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn panel_status_answers_without_session() {
    let dir = TempDir::new().unwrap();
    let task = manual_runtime(&dir, "http://127.0.0.1:1", vec![]);
    let (app, _token) = app_for(&dir, task);

    let req = Request::builder()
        .uri("/panel/status")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["panel_password_required"], true);
    // 没设过密码 ⇒ 还是默认值
    assert_eq!(v["panel_password_is_default"], true);
    // 没有面板会话 ⇒ 未认证
    assert_eq!(v["authenticated"], false);
    assert_eq!(v["api_key_set"], true);
}

// ---------------------------------------------------------------------------
// 10：鉴权 —— 无 api key 一律 401（面板路由口径）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_endpoints_require_api_key() {
    let dir = TempDir::new().unwrap();
    let task = manual_runtime(
        &dir,
        "http://127.0.0.1:1",
        vec![account("u-intl", "intl", true)],
    );
    let (app, _token) = app_for(&dir, task);

    for uri in ["/accounts/daily-chat", "/accounts/daily-chat-web"] {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri} 应当 401");
    }
}

/// 账号目录：tempdir 内的 `accounts/` 子目录（并建出来）。
fn accounts_dir(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("accounts");
    let _ = std::fs::create_dir_all(&path);
    path
}
