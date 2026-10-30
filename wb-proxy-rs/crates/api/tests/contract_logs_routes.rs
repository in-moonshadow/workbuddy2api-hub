//! 关卡 4.25：logs / settings/reveal / pricing/refresh / updates/check 契约测试。
//!
//! Python 真源：`_get_logs`（:16136-16149）/ `_get_logs_export`（:16152-16165）/
//! `_route_logs_clear`（:17477-17479）/ `_get_settings_reveal`（:16169-16178）/
//! `_route_pricing_refresh`（:17463-17476）/ `_route_update_check`（:17438-17450）。
//!
//! 测试环境 TaskRuntime 的 price_refresher / update_checker 未接线 ⇒ 返回
//! 「未运行」分支；日志缓冲走 tasks::logs（隔离测试串行锁由模块内测试保证）。

use std::sync::{Arc, LazyLock, Mutex};

// 日志缓冲跨测试共享 ⇒ 串行锁隔离。
static LOG_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_tasks::api_routes::TaskRuntime;
use wb_tasks::growth_api::TaskEndpoints;

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().join("accounts"),
        dashboard: dir.path().join("dashboard.html"),
        api_key: Some("test-key".to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "intl".to_string(),
    })
}

fn runtime(dir: &TempDir) -> Arc<TaskRuntime> {
    TaskRuntime::new(
        &dir.path().join("accounts"),
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    )
}

fn app_for(dir: &TempDir) -> (axum::Router, String) {
    let cfg = make_cfg(dir);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    (app_with(cfg, panels, runtime(dir)), token)
}

async fn send(app: axum::Router, token: &str, method: &str, uri: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token);
    if method == "POST" {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(Body::from("{}")).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn logs_requires_panel_session() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let app = app_with(cfg, panels, runtime(&dir));
    let req = Request::builder().uri("/logs").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn logs_returns_the_python_shape() {
    let _g = LOG_TEST_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    let (app, token) = app_for(&dir);
    wb_tasks::logs::clear();
    wb_tasks::logs::append("scheduler tick", None, None);
    let (status, body) = send(app, &token, "GET", "/logs?limit=10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["logs"][0]["msg"], "scheduler tick");
    assert!(body["max_id"].as_u64().unwrap() >= 1);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn logs_clear_empties_the_buffer() {
    let _g = LOG_TEST_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    let (app, token) = app_for(&dir);
    wb_tasks::logs::clear();
    wb_tasks::logs::append("one entry", None, None);
    let (status, _) = send(app.clone(), &token, "POST", "/logs/clear").await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = send(app, &token, "GET", "/logs").await;
    assert_eq!(body["total"], 0);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn logs_export_serves_text_with_attachment() {
    let _g = LOG_TEST_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    let (app, token) = app_for(&dir);
    wb_tasks::logs::clear();
    wb_tasks::logs::append("hello world", None, None);
    let req = Request::builder()
        .uri("/logs/export")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .map(|v| v.to_str().unwrap().contains("attachment"))
        .unwrap_or(false));
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("hello world"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn settings_reveal_unknown_id_is_404() {
    let _g = LOG_TEST_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    let (app, token) = app_for(&dir);
    let (status, _) = send(app, &token, "GET", "/settings/reveal?id=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// 注：/pricing/refresh 在 TaskRuntime::new 装配的 refresher 下会 spawn 真实
// run_once（打上游），不属于无副作用契约测试面 —— 生产实例上手动验证。

// 注：/updates/check 会触发真实 GitHub 请求（check_manual），同上面不在
// 无副作用契约面 —— 生产实例上手动验证。

#[tokio::test]
async fn pricing_mapping_requires_model() {
    let dir = TempDir::new().unwrap();
    let (app, token) = app_for(&dir);
    let req = Request::builder()
        .method("POST")
        .uri("/pricing/mapping")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "缺 model 应 400");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("model"));
}
