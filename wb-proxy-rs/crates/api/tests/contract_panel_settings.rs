//! 关卡 4.22（线 C）产出物：面板设置路由（`GET /settings` 与
//! `POST /settings/save`）的契约测试。
//!
//! 权威源：Python `wb_proxy.py:16112 _get_settings`、`:16401 _handle_settings_save`
//! （末行 `reply.update(runtime_settings_view())`）、`:18264-18267`
//! （`POST /settings/save` **只认面板会话**）。
//!
//! 视图与增量来自线 B（`wb_tasks::settings_view`）。
//!
//! 全走 tempdir 账号目录；**不发任何网络请求**。

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
const PANEL_PASSWORD: &str = "admin";

const PANEL_REQUIRED_BODY: &str =
    r#"{"error":{"message":"panel password required","type":"invalid_request_error","code":401}}"#;

/// `runtime_settings_view()` 的 33 个顶层键，顺序逐条对齐 Python
/// `wb_proxy.py:9661-9730`（也与线 B 的 `json!` 字面量顺序一致）。
const VIEW_KEYS: [&str; 33] = [
    "panel_password_is_default",
    "api_key_set",
    "api_key_set_by_panel",
    "api_key_masked",
    "auth_required",
    "api_keys",
    "deleted_api_keys",
    "limits",
    "reserve_credits",
    "daily_token_limit",
    "daily_credit_limit",
    "model_daily_token_limit",
    "pricing_refresh_minutes",
    "credits_refresh_hours",
    "ui_language",
    "pricing_variant_inherit",
    "pricing_enabled",
    "auto_switch_product",
    "daily_chat_web",
    "local_web_tools",
    "remaining_priority_enabled",
    "egress_backoff_enabled",
    "accounts_collapsed",
    "accounts_separate_tab",
    "key_before_hidden",
    "hidden_pages",
    "update_check_enabled",
    "upstream",
    "prompt",
    "accounts_dir",
    "usage_dir",
    "settings_file",
    "version",
];

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

/// 登录拿 token。
async fn login(app: &Router) -> String {
    let (status, body) = call(
        app,
        Method::POST,
        "/panel/login",
        Some(json!({"password": PANEL_PASSWORD})),
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

/// 响应体顶层键的顺序（`serde_json` 开了 `preserve_order`，顺序是字节的一部分）。
fn key_order(body: &str) -> Vec<String> {
    let value: Value = serde_json::from_str(body).unwrap();
    value.as_object().unwrap().keys().cloned().collect()
}

// ---------------------------------------------------------------------------
// GET /settings
// ---------------------------------------------------------------------------

/// 没有面板会话 ⇒ 401（Python：`/settings` 命中面板前缀表 ⇒ 外层门禁）。
#[tokio::test]
async fn settings_get_requires_panel_session() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = call(&app, Method::GET, "/settings", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);

    // 光有 API key 也不行（面板路由只认面板会话）。
    let (status, body) = call(&app, Method::GET, "/settings", None, None, Some(API_KEY)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);
}

/// 有面板会话 ⇒ 200，33 个顶层键齐全且顺序与 Python 一致。
#[tokio::test]
async fn settings_get_returns_the_full_view() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app).await;

    let (status, body) = call(&app, Method::GET, "/settings", None, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let keys = key_order(&body);
    let expected: Vec<String> = VIEW_KEYS.iter().map(|key| key.to_string()).collect();
    assert_eq!(keys, expected);
    assert_eq!(keys.len(), 33);
}

// ---------------------------------------------------------------------------
// POST /settings/save
// ---------------------------------------------------------------------------

/// 门禁只认面板会话：API key 不行（Python `:18264-18267`）。
#[tokio::test]
async fn settings_save_requires_panel_session() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);

    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"ui_language": "en"})),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);

    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"ui_language": "en"})),
        None,
        Some(API_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, PANEL_REQUIRED_BODY);
}

/// 保存成功：回包 = 视图合并增量。
///
/// Python `:16723-16725` 的 `reply.update(runtime_settings_view())` ⇒
/// **视图覆盖增量**；键序与 Python 的 `dict.update` 一致（增量里已有的键保持
/// 原位、视图独有的键追加到末尾）。
#[tokio::test]
async fn settings_save_returns_view_merged_with_delta() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app).await;

    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"ui_language": "en"})),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 键序：增量键 `ui_language` 在最前（Python 的 reply 先建、视图后 update），
    // 其余 32 个键按视图顺序追加。
    let mut expected: Vec<String> = vec!["ui_language".to_string()];
    expected.extend(
        VIEW_KEYS
            .iter()
            .filter(|key| **key != "ui_language")
            .map(|key| key.to_string()),
    );
    assert_eq!(key_order(&body), expected);

    // 值：新值生效（视图覆盖增量 ⇒ 取视图里的生效值）。
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["ui_language"], json!("en"));
    assert_eq!(value["panel_password_is_default"], json!(true));

    // 真的落盘了。
    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(cfg.accounts_dir.join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["ui_language"], json!("en"));
}

/// 400 文案逐字（文案由线 B 给）。
#[tokio::test]
async fn settings_save_rejects_bad_values_verbatim() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app).await;

    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"ui_language": "xx"})),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body,
        r#"{"error":{"message":"ui_language must be zh, zh-Hant or en","type":"invalid_request_error","code":400}}"#
    );

    // 非布尔 ⇒ 400（Python 的 `isinstance(raw, bool)` 检查）。
    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"pricing_enabled": "false"})),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body,
        r#"{"error":{"message":"pricing_enabled must be true or false","type":"invalid_request_error","code":400}}"#
    );
}

/// `pricing_variant_inherit` 变了 ⇒ 回包带 `pricing_refresh_started`
/// （Python `:16575-16583` 起一次取价线程的信号）。
///
/// ⚠️ 先把定价开关关掉：否则那次取价会真的去拉 openrouter.ai，违反
/// 「绝不向真实上游发任何请求」的约束（`run_once` 在总开关关着时直接返回）。
#[tokio::test]
async fn settings_save_reports_pricing_refresh_started() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(PanelSessions::new(7 * 24 * 3600));
    let app = make_app(&cfg, &panels);
    let token = login(&app).await;

    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"pricing_enabled": false})),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = call(
        &app,
        Method::POST,
        "/settings/save",
        Some(json!({"pricing_variant_inherit": false})),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["pricing_refresh_started"], json!(true));
    assert_eq!(value["pricing_variant_inherit"], json!(false));
    // 视图里没有 `pricing_refresh_started` ⇒ 它留在回包里；键序按 Python 的
    // `dict.update`：增量里已有的键保持原位（这里是前两位），视图的其余键追加。
    let keys = key_order(&body);
    assert_eq!(
        &keys[..2],
        &[
            "pricing_variant_inherit".to_string(),
            "pricing_refresh_started".to_string()
        ]
    );
    let mut expected: Vec<String> = vec![
        "pricing_variant_inherit".to_string(),
        "pricing_refresh_started".to_string(),
    ];
    expected.extend(
        VIEW_KEYS
            .iter()
            .filter(|key| **key != "pricing_variant_inherit")
            .map(|key| key.to_string()),
    );
    assert_eq!(keys, expected);
    assert_eq!(keys.len(), 34);
}
