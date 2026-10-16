//! 授权契约：带正确 key 请求 /health 时，影子实例必须输出与 Python 相同的
//! 身份字段集（uid/domain/issuer/credential_file/expires_at，5 基础 + 5 身份）。
//!
//! fixture 是从真实 Python 实例（127.0.0.1:8788）带 Authorization 抓取的
//! **字段形状**，值已全部脱敏（str→"s"/"f.json"、int→0、bool→true、null→null）。
//! 契约只断言字段集与 JSON 类型一致，值允许不同（账号池因机器而异）。

use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};

fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("active_realm.json"), r#"{"realm":"intl"}"#).unwrap();
    fs::write(
        dir.path().join("settings.json"),
        r#"{"ui_language":"zh","launcher_key":"wb-test-key"}"#,
    )
    .unwrap();
    let account = serde_json::json!({
        "uid": "a", "realm": "intl", "enabled": true,
        "accessToken": "h.p.s", "cooldownUntil": 0.0, "lastError": ""
    });
    fs::write(dir.path().join("a.json"), account.to_string()).unwrap();
    dir
}

fn config(dir: &Path) -> Config {
    Config {
        accounts_dir: dir.to_path_buf(),
        dashboard: dir.join("dashboard.html"),
        // 复刻真实启动路径（main.rs）：launcher_key 经 effective_api_key 进入
        // cfg.api_key，key_ok 的 identify_key 才能把它当 extra key 识别
        // （面板 key 表为空时的第三条路径）。
        api_key: wb_storage::effective_api_key(dir, None),
        lan: false,
    }
}

/// 与 contract_health.rs 相同的形状断言：字段集 + JSON 类型一致。
fn assert_same_shape(actual: &serde_json::Value, expected: &serde_json::Value) {
    let a = actual.as_object().expect("actual must be an object");
    let e = expected.as_object().expect("expected must be an object");
    let mut a_keys: Vec<&String> = a.keys().collect();
    let mut e_keys: Vec<&String> = e.keys().collect();
    a_keys.sort();
    e_keys.sort();
    assert_eq!(a_keys, e_keys, "field sets differ");
    for key in e_keys {
        assert_eq!(
            std::mem::discriminant(&a[key]),
            std::mem::discriminant(&e[key]),
            "type of {key} differs"
        );
    }
}

async fn get_health(cfg: Arc<Config>, auth_header: Option<(&str, &str)>) -> serde_json::Value {
    let mut builder = Request::builder().uri("/health");
    if let Some((name, value)) = auth_header {
        builder = builder.header(name, value);
    }
    let response = app(cfg)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn authorized_request_matches_python_auth_shape() {
    // 授权响应形状 = Python 授权 fixture（10 字段：5 基础 + 5 身份）。
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/health_python_auth.json")).unwrap();
    let dir = fixture_dir();
    let actual = get_health(
        Arc::new(config(dir.path())),
        Some(("x-api-key", "wb-test-key")),
    )
    .await;
    assert_same_shape(&actual, &expected);
}

#[tokio::test]
async fn unauthorized_request_has_no_identity_fields() {
    // 未授权响应只含 5 基础字段，不得泄露身份字段。
    let dir = fixture_dir();
    let actual = get_health(Arc::new(config(dir.path())), None).await;
    let obj = actual.as_object().unwrap();
    assert_eq!(
        obj.len(),
        5,
        "unauthorized must expose exactly 5 base fields"
    );
    for key in ["uid", "domain", "issuer", "credential_file", "expires_at"] {
        assert!(!obj.contains_key(key), "unauthorized must not leak {key}");
    }
}

#[tokio::test]
async fn authorized_identity_values_come_from_representative() {
    // 授权后身份字段的值来自代表账号（uid=文件名、issuer=JWT iss）。
    let dir = fixture_dir();
    let actual = get_health(
        Arc::new(config(dir.path())),
        Some(("x-api-key", "wb-test-key")),
    )
    .await;
    assert_eq!(actual["uid"], "a");
    assert_eq!(actual["credential_file"], "a.json");
    // a.json 的 accessToken="h.p.s" 不是合法 JWT → issuer 为空字符串（宽容）
    assert_eq!(actual["issuer"], "");
}
