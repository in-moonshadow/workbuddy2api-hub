//! 关卡 4.22（线 C）产出物：面板认证路由 + 面板鉴权强度的契约测试。
//!
//! 权威源：Python `wb_proxy.py:17032 _handle_panel`（登录/登出/改口令）、
//! `:15661 _is_panel_route`（面板前缀表）、`:15709`/`:18318`（双层门禁）。
//!
//! 全走 tempdir 账号目录；**不发任何网络请求**——这三条路由只读本地
//! settings.json，不出站。
//!
//! 断言用**原始响应体字符串**（不是解析后的 JSON）：`serde_json` 开了
//! `preserve_order`，响应键序是字节的一部分，逐字比对才能钉住它。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::auth::PanelSessions;
use wb_api::config::PanelConfig;
use wb_api::routes::ApiPanelSink;
use wb_api::Config;
use wb_tasks::api_routes::TaskRuntime;
use wb_tasks::growth_api::TaskEndpoints;

const API_KEY: &str = "test-key";
/// Python `wb_settings.DEFAULT_PANEL_PASSWORD`。
const PANEL_PASSWORD: &str = "admin";

/// 面板路由缺会话时的 401 响应体（**逐字节**）。
///
/// Python：`_error(401, "panel password required", "invalid_request_error")`
/// ⇒ `{"error": {"message", "type", "code"}}`。
const PANEL_REQUIRED_BODY: &str =
    r#"{"error":{"message":"panel password required","type":"invalid_request_error","code":401}}"#;

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().join("accounts"),
        dashboard: dir.path().join("dashboard.html"),
        api_key: Some(API_KEY.to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "cn".to_string(),
    })
}

fn make_task(cfg: &Arc<Config>) -> Arc<TaskRuntime> {
    TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    )
}

/// 造一个 app：面板退避时长设成 0（否则每个登录失败用例都要白等 0.5 秒）。
fn make_app(cfg: &Arc<Config>, panels: &Arc<PanelSessions>) -> Router {
    wb_api::routes::app_with_config(
        cfg.clone(),
        panels.clone(),
        make_task(cfg),
        PanelConfig {
            login_backoff: Duration::ZERO,
            trusted_proxies: Vec::new(),
        },
    )
}

/// 发一次请求，返回 (状态码, 原始响应体)。
async fn call(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
    api_key: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("x-panel-token", token);
    }
    if let Some(key) = api_key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let req = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn get(
    app: &Router,
    path: &str,
    token: Option<&str>,
    api_key: Option<&str>,
) -> (StatusCode, String) {
    call(app, Method::GET, path, None, token, api_key).await
}

async fn post(
    app: &Router,
    path: &str,
    body: Value,
    token: Option<&str>,
    api_key: Option<&str>,
) -> (StatusCode, String) {
    call(app, Method::POST, path, Some(body), token, api_key).await
}

/// 登录并拿到 token。
async fn login(app: &Router, password: &str) -> String {
    let (status, body) = post(
        app,
        "/panel/login",
        json!({"password": password}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "登录失败：{body}");
    serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// 双层门禁：面板路由必须有面板会话，API key 不够
// ---------------------------------------------------------------------------

/// Python `:15709` / `:18318`：命中面板前缀且没有面板会话 ⇒ 401。
#[tokio::test]
async fn panel_route_without_session_is_401() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = get(&app, "/usage", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);
}

/// **对齐 Python 的关键行为**：API key 单独**进不去**面板路由。
///
/// Rust 改造前所有面板路由都是 `unauthorized(...)`（API key 或面板会话即可），
/// 比 Python 宽；这条用例把收紧后的强度钉住。
#[tokio::test]
async fn panel_route_with_api_key_is_still_401() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = get(&app, "/usage", None, Some(API_KEY)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);
}

/// Python 前缀表里的每条路由都要面板会话。
///
/// 前缀表逐条照抄自 `wb_proxy._is_panel_route`（`:15661-15689`）。
#[tokio::test]
async fn every_panel_prefix_requires_a_panel_session() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let cases = [
        (Method::GET, "/usage"),
        (Method::GET, "/v1/usage"),
        (Method::GET, "/usage/recent"),
        (Method::GET, "/usage/analytics"),
        (Method::GET, "/activity/history"),
        (Method::GET, "/tasks"),
        (Method::GET, "/scheduler"),
        (Method::GET, "/pricing"),
        (Method::GET, "/updates"),
        (Method::GET, "/settings"),
        (Method::POST, "/accounts/checkin"),
        (Method::POST, "/tasks/run"),
        (Method::POST, "/settings/save"),
    ];
    for (method, path) in cases {
        let (status, body) = call(&app, method, path, Some(json!({})), None, Some(API_KEY)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} 应当要面板会话，实际 {status}：{body}"
        );
        assert_eq!(body, PANEL_REQUIRED_BODY, "{path} 的 401 文案不对");
    }
}

/// 面板会话能过外层门禁（内层 `_authorized()` 也过，因为面板会话本身就在
/// `key_ok` 的第一条路径里）。
#[tokio::test]
async fn panel_session_passes_the_gate() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = panels.create();

    let (status, body) = get(&app, "/usage", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "有面板会话却没过门禁：{body}");
}

/// 模型接口的 401 文案**不能**被换成面板文案（两套构造函数，别混）。
#[tokio::test]
async fn model_route_keeps_its_own_401_wording() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = post(
        &app,
        "/v1/chat/completions",
        json!({"model": "hy4-preview", "messages": []}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body.contains("invalid api key"),
        "模型接口 401 文案被改动了：{body}"
    );
    assert!(
        !body.contains("panel password required"),
        "模型接口误用了面板 401 文案：{body}"
    );
}

/// `/panel/status` **不**在 Python 的面板前缀表里 ⇒ 不需要会话也能问。
#[tokio::test]
async fn panel_status_stays_open() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = get(&app, "/panel/status", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// POST /panel/login（Python `:17036-17064`）
// ---------------------------------------------------------------------------

/// 登录成功：回 `{"ok": true, "token": ..., "using_default_password": true}`。
#[tokio::test]
async fn login_success_returns_token() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = post(
        &app,
        "/panel/login",
        json!({"password": PANEL_PASSWORD}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    // 键序是字节的一部分：ok → token → using_default_password。
    let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["ok", "token", "using_default_password"]);
    assert_eq!(value["ok"], json!(true));
    assert!(!value["token"].as_str().unwrap().is_empty());
    assert_eq!(value["using_default_password"], json!(true));

    // 发回来的 token 真的能用。
    let (status, body) = get(&app, "/usage", value["token"].as_str(), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// 口令错 ⇒ 401 `invalid panel password`。
#[tokio::test]
async fn login_with_wrong_password_is_401() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = post(
        &app,
        "/panel/login",
        json!({"password": "not-the-password"}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        r#"{"error":{"message":"invalid panel password","type":"invalid_request_error","code":401}}"#
    );
}

/// 限流：60 秒窗口内同一 IP 失败 5 次后，第 6 次直接 429。
///
/// Python `:17042-17047`：429 的 `type` 是 **`rate_limit_error`**（不是
/// `invalid_request_error`）。
#[tokio::test]
async fn login_is_rate_limited_after_five_failures() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    for _ in 0..5 {
        let (status, body) = post(
            &app,
            "/panel/login",
            json!({"password": "wrong"}),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }
    let (status, body) = post(
        &app,
        "/panel/login",
        json!({"password": "wrong"}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["type"], json!("rate_limit_error"));
    assert_eq!(value["error"]["code"], json!(429));
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("too many login attempts, please wait "),
        "限流文案不对：{message}"
    );
    assert!(message.ends_with('s'), "限流文案不对：{message}");
}

/// 坏 JSON 体 ⇒ 400 `invalid JSON body`（Python `:16331-16333` 逐字）。
#[tokio::test]
async fn login_with_broken_json_is_400() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let req = Request::builder()
        .method(Method::POST)
        .uri("/panel/login")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        r#"{"error":{"message":"invalid JSON body","type":"invalid_request_error","code":400}}"#
    );
}

// ---------------------------------------------------------------------------
// POST /panel/logout（Python `:17063-17065`）
// ---------------------------------------------------------------------------

/// 登出回 `{"ok": true}`，并把带来的 token 撤掉。
#[tokio::test]
async fn logout_revokes_the_token() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app, PANEL_PASSWORD).await;

    let (status, body) = post(&app, "/panel/logout", json!({}), Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);

    // 撤掉之后这个 token 不再能过门禁。
    let (status, body) = get(&app, "/usage", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);
}

/// 登出**不需要**已有会话（没带 token 也回 200）。
#[tokio::test]
async fn logout_without_token_is_ok() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = post(&app, "/panel/logout", json!({}), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);
}

// ---------------------------------------------------------------------------
// POST /panel/password（Python `:17066-17085`）
// ---------------------------------------------------------------------------

/// 改口令必须先有面板会话。
#[tokio::test]
async fn password_change_requires_session() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = post(
        &app,
        "/panel/password",
        json!({"current": PANEL_PASSWORD, "new": "new-password"}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);
}

/// 当前口令不对 ⇒ 401 `current password is wrong`。
#[tokio::test]
async fn password_change_with_wrong_current_is_401() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app, PANEL_PASSWORD).await;

    let (status, body) = post(
        &app,
        "/panel/password",
        json!({"current": "wrong", "new": "new-password"}),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        r#"{"error":{"message":"current password is wrong","type":"invalid_request_error","code":401}}"#
    );
}

/// 新口令太短 ⇒ 400 `new password must be at least 4 characters`。
#[tokio::test]
async fn password_change_with_short_new_password_is_400() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app, PANEL_PASSWORD).await;

    let (status, body) = post(
        &app,
        "/panel/password",
        json!({"current": PANEL_PASSWORD, "new": "abc"}),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        r#"{"error":{"message":"new password must be at least 4 characters","type":"invalid_request_error","code":400}}"#
    );
}

/// 改口令成功：回 `{"ok": true, "token": ...}`；旧会话被撤销，新 token 可用。
#[tokio::test]
async fn password_change_succeeds_and_rotates_sessions() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let old_token = login(&app, PANEL_PASSWORD).await;

    let (status, body) = post(
        &app,
        "/panel/password",
        json!({"current": PANEL_PASSWORD, "new": "new-password"}),
        Some(&old_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["ok", "token"]);
    assert_eq!(value["ok"], json!(true));
    let new_token = value["token"].as_str().unwrap().to_string();
    assert!(!new_token.is_empty());

    // 新口令不是默认值 ⇒ 旧会话全部撤销（Python `:17078-17081`）。
    let (status, body) = get(&app, "/usage", Some(&old_token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "旧会话没被撤销：{body}");

    // 新 token 能用；旧口令也不能再登录了。
    let (status, body) = get(&app, "/usage", Some(&new_token), None).await;
    assert_eq!(status, StatusCode::OK, "新 token 不能用：{body}");
    let (status, body) = post(
        &app,
        "/panel/login",
        json!({"password": PANEL_PASSWORD}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "旧口令还能登录：{body}");

    // 用新口令能登录上。
    let token = login(&app, "new-password").await;
    let (status, body) = get(&app, "/usage", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
