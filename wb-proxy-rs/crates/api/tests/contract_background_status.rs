//! 关卡 4.19：后台定价/更新两条状态端点的契约测试 + **跨语言版本号 parity**。
//!
//! 1. `GET /pricing` / `GET /updates` 的鉴权与返回体形状（对齐 Python
//!    `PRICING.status()` / `UPDATES.status()`）；
//! 2. `wb_tasks::update_checker::HUB_VERSION` 必须等于 `wb_proxy.py` 里
//!    `server_version = "wb-proxy/<ver>"` 的版本段——否则更新检查的
//!    「有新版」判定会拿 crate 版本（`0.1.0`）当基准，恒为真。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_tasks::api_routes::TaskRuntime;
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::update_checker::HUB_VERSION;

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().join("accounts"),
        dashboard: dir.path().join("dashboard.html"),
        api_key: Some("test-key".to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "cn".to_string(),
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

async fn get_json(app: axum::Router, uri: &str, with_key: bool) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri);
    if with_key {
        builder = builder.header("authorization", "Bearer test-key");
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

// ---------------------------------------------------------------------------
// 路由：鉴权与形状
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pricing_and_updates_require_auth() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        runtime(&dir),
    );
    for uri in ["/pricing", "/updates"] {
        let (status, _) = get_json(app.clone(), uri, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} 未鉴权应 401");
    }
}

#[tokio::test]
async fn pricing_status_has_the_python_keys() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        runtime(&dir),
    );
    let (status, body) = get_json(app, "/pricing", true).await;
    assert_eq!(status, StatusCode::OK);
    for key in [
        "interval_minutes",
        "enabled",
        "master_enabled",
        "last_run",
        "last_error",
        "logs",
    ] {
        assert!(body.get(key).is_some(), "/pricing 缺键 `{key}`: {body}");
    }
}

#[tokio::test]
async fn updates_status_has_the_ten_python_keys() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        runtime(&dir),
    );
    let (status, body) = get_json(app, "/updates", true).await;
    assert_eq!(status, StatusCode::OK);
    for key in [
        "current_version",
        "latest_version",
        "update_available",
        "enabled",
        "checking",
        "last_attempt",
        "last_success",
        "last_error",
        "release_url",
        "published_at",
    ] {
        assert!(body.get(key).is_some(), "/updates 缺键 `{key}`: {body}");
    }
    assert_eq!(
        body["current_version"],
        Value::String(HUB_VERSION.to_string()),
        "current_version 必须是 hub 的发布版本号，不是 crate 版本"
    );
}

// ---------------------------------------------------------------------------
// 跨语言 parity：Rust 的 HUB_VERSION 必须与 Python 源里的字面量一致
// ---------------------------------------------------------------------------

#[test]
fn hub_version_matches_the_python_source_literal() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../wb_proxy.py");
    let src = std::fs::read_to_string(path).expect("读不到仓库里的 wb_proxy.py");

    // `server_version = "wb-proxy/1.7.1"`
    let server = src
        .split("server_version = \"wb-proxy/")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("wb_proxy.py 里找不到 server_version 字面量");
    // `"version": "1.7.1",`（面板 overview 用）
    let overview = src
        .split("\"version\": \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("wb_proxy.py 里找不到 overview 的 version 字面量");

    assert_eq!(
        server, overview,
        "Python 侧两个版本字面量必须相等（tests/_test_release_engineering.py 盯着这条）"
    );
    assert_eq!(
        server, HUB_VERSION,
        "Rust 的 HUB_VERSION 必须与 Python 源里的版本一致，否则更新检查会恒判「有更新」"
    );
}
