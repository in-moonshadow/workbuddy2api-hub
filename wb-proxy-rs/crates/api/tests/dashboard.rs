use std::fs;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};

fn fixture(language: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("settings.json"),
        format!(r#"{{"ui_language":"{language}"}}"#),
    )
    .unwrap();
    fs::write(
        dir.path().join("dashboard.html"),
        r#"<html data-ui-language="__WB_UI_LANGUAGE__"><body>ok</body></html>"#,
    )
    .unwrap();
    dir
}

fn config(dir: &std::path::Path) -> Config {
    Config {
        accounts_dir: dir.to_path_buf(),
        dashboard: dir.join("dashboard.html"),
        api_key: None,
        lan: false,
    }
}

#[tokio::test]
async fn dashboard_injects_language_and_serves_etag() {
    let dir = fixture("en");
    let router = app(Arc::new(config(dir.path())));
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    let etag = response
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains(r#"data-ui-language="en""#));

    let cached = router
        .oneshot(
            Request::builder()
                .uri("/")
                .header("If-None-Match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    let body = cached.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}
