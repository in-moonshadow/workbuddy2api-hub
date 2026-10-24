//! 关卡 4.18（线 C）：后台积分刷新器的**接线层**契约测试。
//!
//! 覆盖三件事：
//! 1. `GET /accounts/credits-refresher` 的鉴权与返回体形状（六个键与 Python
//!    `CreditsRefresher.status()` 逐字一致）；
//! 2. TTL 真的来自 settings 的 `credits_refresh_hours`（默认 12h / 0 = 关闭）；
//! 3. D36-b：`credits_refresh` 的实现是**读账号文件里的本地快照**。
//!
//! 全程用 tempdir 账号池，**不发任何真实上游请求**（本文件根本不碰出站）。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_tasks::api_routes::{local_credits_snapshot, TaskRuntime};
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

fn write_json(dir: &TempDir, name: &str, doc: Value) {
    std::fs::write(
        dir.path().join(name),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();
}

/// 造一个只含一个账号的 tempdir 账号池。
fn pool_with_account(dir: &TempDir, credits: Option<Value>) {
    let mut account = json!({
        "uid": "u1",
        "realm": "cn",
        "enabled": true,
        "accessToken": "token-1",
    });
    if let Some(credits) = credits {
        account["credits"] = credits;
    }
    write_json(dir, "u1.json", account);
}

fn runtime(dir: &TempDir) -> Arc<TaskRuntime> {
    TaskRuntime::new(
        dir.path(),
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    )
}

// ---------------------------------------------------------------------------
// 路由：鉴权与形状
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credits_refresher_route_requires_auth() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let app = app_with(cfg, panels, runtime(&dir));
    let req = Request::builder()
        .uri("/accounts/credits-refresher")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn credits_refresher_route_returns_the_six_python_keys() {
    let dir = TempDir::new().unwrap();
    pool_with_account(&dir, None);
    let cfg = make_cfg(&dir);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(cfg, panels, runtime(&dir));
    let req = Request::builder()
        .uri("/accounts/credits-refresher")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token.as_str())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    for key in [
        "ttl_hours",
        "interval_seconds",
        "last_run",
        "last_uid",
        "last_error",
        "logs",
    ] {
        assert!(body.get(key).is_some(), "返回体缺键 `{key}`: {body}");
    }
    // 还没跑过 ⇒ last_run / last_uid / last_error 都是空
    assert!(body["last_run"].is_null());
    assert!(body["last_uid"].is_null());
    assert!(body["last_error"].is_null());
    assert_eq!(body["logs"], json!([]));
}

// ---------------------------------------------------------------------------
// TTL 来自 settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ttl_defaults_to_twelve_hours() {
    let dir = TempDir::new().unwrap();
    pool_with_account(&dir, None);
    let task = runtime(&dir);
    let status = wb_tasks::api_routes::route_credits_refresher_status(&task);
    assert_eq!(status["ttl_hours"], json!(12.0), "默认 TTL 应为 12 小时");
    assert_eq!(status["interval_seconds"], json!(1800.0));
}

#[tokio::test]
async fn ttl_follows_the_settings_key() {
    let dir = TempDir::new().unwrap();
    pool_with_account(&dir, None);
    write_json(&dir, "settings.json", json!({"credits_refresh_hours": 6.0}));
    let task = runtime(&dir);
    let status = wb_tasks::api_routes::route_credits_refresher_status(&task);
    assert_eq!(status["ttl_hours"], json!(6.0));
}

#[tokio::test]
async fn ttl_zero_means_disabled() {
    let dir = TempDir::new().unwrap();
    pool_with_account(&dir, None);
    write_json(&dir, "settings.json", json!({"credits_refresh_hours": 0}));
    let task = runtime(&dir);
    let status = wb_tasks::api_routes::route_credits_refresher_status(&task);
    assert_eq!(status["ttl_hours"], json!(0.0), "0 = 关闭刷新");
}

// ---------------------------------------------------------------------------
// D36-b：`credits_refresh` 读的是本地快照
// ---------------------------------------------------------------------------

#[test]
fn local_snapshot_is_read_from_the_account_file() {
    let dir = TempDir::new().unwrap();
    let credits = json!({
        "remain": 30.0,
        "used": 0.0,
        "size": 30.0,
        "updated_at": 1760000000.0,
        "updated_iso": "2026-10-21 08:00:00",
    });
    pool_with_account(&dir, Some(credits.clone()));
    let got = local_credits_snapshot(dir.path(), "u1.json").unwrap();
    assert_eq!(got["remain"], json!(30.0));
    assert_eq!(got["updated_at"], json!(1760000000.0));
}

#[test]
fn local_snapshot_is_none_when_the_account_has_none() {
    let dir = TempDir::new().unwrap();
    pool_with_account(&dir, None);
    assert!(local_credits_snapshot(dir.path(), "u1.json").is_none());
}

#[test]
fn local_snapshot_is_none_for_a_missing_account_file() {
    let dir = TempDir::new().unwrap();
    assert!(local_credits_snapshot(dir.path(), "nope.json").is_none());
}
