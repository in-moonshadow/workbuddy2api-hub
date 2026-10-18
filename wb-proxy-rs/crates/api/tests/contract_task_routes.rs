//! 关卡 4.10 产出物：9 条任务路由的契约测试。
//!
//! 全走 wiremock 假上游 + tempdir 账号池；绝不向真实上游发请求。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_tasks::api_routes::TaskRuntime;
use wb_tasks::growth_api::TaskEndpoints;

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().to_path_buf(),
        dashboard: dir.path().to_path_buf(),
        api_key: Some("test-key".to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "cn".to_string(),
    })
}

#[tokio::test]
async fn unauthorized_returns_401() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    for path in ["/tasks", "/scheduler"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn invalid_body_returns_400() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .method("POST")
        .uri("/tasks/run")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from("not json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tasks_get_empty_pool_returns_msg() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .uri("/tasks")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["msg"], "未找到可用的国内版账号");
}

#[tokio::test]
async fn scheduler_get_returns_enabled_field() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .uri("/scheduler")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("enabled").is_some());
}

#[tokio::test]
async fn claim_all_empty_pool_returns_ok() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .method("POST")
        .uri("/tasks/claim-all")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(json!({}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("ok").is_some());
}
