use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("active_realm.json"), r#"{"realm":"cn"}"#).unwrap();
    fs::write(dir.path().join("settings.json"), r#"{"ui_language":"zh"}"#).unwrap();
    let account = |uid: &str, enabled: bool| {
        serde_json::json!({
            "uid": uid, "realm": "cn", "enabled": enabled,
            "accessToken": "h.p.s", "cooldownUntil": 0.0, "lastError": ""
        })
    };
    fs::write(dir.path().join("a.json"), account("a", true).to_string()).unwrap();
    fs::write(dir.path().join("b.json"), account("b", false).to_string()).unwrap();
    dir
}

fn config(dir: &Path) -> Config {
    Config {
        accounts_dir: dir.to_path_buf(),
        dashboard: dir.join("dashboard.html"),
        api_key: None,
        lan: false,
    }
}

#[tokio::test]
async fn health_reports_pool_shape() {
    let dir = fixture();
    let response = app(Arc::new(config(dir.path())))
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "ok": true, "realm": "cn", "accounts": 2,
            "accounts_ready": 1, "api_key_required": false
        })
    );
}
