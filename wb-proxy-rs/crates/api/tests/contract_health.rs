//! 契约检查：影子实例的 /health 必须与 Python 输出同字段、同类型。
//!
//! fixture 是从真实 Python 实例（127.0.0.1:8788）抓取的响应，重新生成方式见
//! wb-proxy-rs/README.md 的「契约 fixture」一节。

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
    // launcher_key 使实例「要求 key」（对齐 Python 真实实例启动后 API_KEY 非空），
    // 未授权请求因此走未授权路径，匹配 Python fixture 的 5 字段形状。
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
        api_key: None,
        lan: false,
        upstream_base_url: None,
        realm: "intl".into(),
    }
}

/// 检查同字段、同 JSON 类型；值本身允许不同（账号池与开关因机器而异）。
///
/// 局限：`serde_json::Value` 的 `Number` 同时覆盖整数与浮点，故 `5` 与 `5.0`
/// 视为同型。`bool` 与 `Number` 的区分仍有效，足以拦住字段改名/字段丢失/
/// 布尔退化为数字这类 M1 关切的破坏性变更。
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

#[tokio::test]
async fn health_shape_matches_python_fixture() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/health_python.json")).unwrap();
    let dir = fixture_dir();
    let response = app(Arc::new(config(dir.path())))
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let actual: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_same_shape(&actual, &expected);
}
