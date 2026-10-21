//! 关卡 4.19（线 C）产出物：用量路由的契约测试。
//!
//! 全走 tempdir 账号/用量目录；**不发任何网络请求**——三条路由只读本地
//! `usage/usage.jsonl`，不出站。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_tasks::api_routes::{usage_dir_from, TaskRuntime};
use wb_tasks::growth_api::TaskEndpoints;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "test-key";

/// 造一份临时配置 + 运行时。
///
/// `accounts_dir` 放在 tempdir 下的 `accounts/` 里：用量目录由
/// `usage_dir_from` 从 `accounts_dir` 的**同级**推导 ⇒ 落在 tempdir 内
/// （`tmp/xxx/usage`），不会写进共享的 `/tmp/usage`。
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

fn make_app(cfg: &Arc<Config>) -> Router {
    app_with(
        cfg.clone(),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        make_task(cfg),
    )
}

/// 发一次 GET，返回 (状态码, 解析后的 JSON)。
async fn get(app: &Router, path: &str, with_key: bool) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(path);
    if with_key {
        builder = builder.header("authorization", format!("Bearer {API_KEY}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// 一条成功用量行。
fn ok_row(model: &str, account: &str, total_tokens: i64, iso: &str) -> Value {
    json!({
        "at": 1_700_000_000.0,
        "iso": iso,
        "model": model,
        "stream": false,
        "outcome": "completed",
        "elapsed_ms": 120,
        "prompt_tokens": total_tokens / 2,
        "completion_tokens": total_tokens - total_tokens / 2,
        "total_tokens": total_tokens,
        "account": account,
        "realm": "cn",
        "key": "",
    })
}

/// 用量目录（tempdir 内）。
fn usage_dir(cfg: &Config) -> PathBuf {
    usage_dir_from(&cfg.accounts_dir)
}

#[tokio::test]
async fn usage_routes_require_auth() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    for path in ["/usage", "/usage/by-account", "/usage/analytics"] {
        let (status, body) = get(&app, path, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} 未鉴权应回 401");
        // 401 走 OpenAI 信封（对齐 `/scheduler` 的鉴权模式）。
        assert_eq!(body["error"]["code"], 401);
    }
}

#[tokio::test]
async fn usage_get_returns_empty_aggregate_when_log_missing() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    // 用量目录压根不存在。
    assert!(!usage_dir(&cfg).exists());
    let (status, body) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_requests"], 0);
    assert_eq!(body["requests"], 0);
    assert_eq!(body["errors"], 0);
    assert_eq!(body["total_tokens"], 0);
    assert!(body["by_model"].as_object().unwrap().is_empty());
}

#[tokio::test]
async fn usage_get_returns_empty_aggregate_when_log_file_missing() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    // 目录在、日志不在。
    std::fs::create_dir_all(usage_dir(&cfg)).unwrap();
    let (status, body) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_requests"], 0);
    assert!(body["by_model"].as_object().unwrap().is_empty());

    let (status, body) = get(&app, "/usage/by-account", true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["accounts"].as_array().unwrap().is_empty());

    let (status, body) = get(&app, "/usage/analytics", true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["by_date"].as_object().unwrap().is_empty());
}

#[tokio::test]
async fn append_usage_row_appends_instead_of_overwriting() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);

    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:00:00"))
        .unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 20, "2026-10-21 10:01:00"))
        .unwrap();

    let rows = wb_tasks::usage::load_usage_rows(&usage).unwrap();
    assert_eq!(rows.len(), 2, "两行都该在（append 不是覆盖）");
    assert_eq!(rows[0]["total_tokens"], 10);
    assert_eq!(rows[1]["total_tokens"], 20);
}

#[tokio::test]
async fn usage_get_counts_appended_rows() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:00:00"))
        .unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m2", "a2", 30, "2026-10-21 10:01:00"))
        .unwrap();

    let (status, body) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_requests"], 2);
    assert_eq!(body["requests"], 2);
    assert_eq!(body["total_tokens"], 40);
    assert_eq!(body["by_model"]["m1"]["requests"], 1);
    assert_eq!(body["by_model"]["m1"]["total_tokens"], 10);
    assert_eq!(body["by_model"]["m2"]["requests"], 1);
    assert_eq!(body["by_model"]["m2"]["total_tokens"], 30);
}

#[tokio::test]
async fn usage_by_account_groups_by_account() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:00:00"))
        .unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 5, "2026-10-21 10:01:00"))
        .unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m2", "a2", 100, "2026-10-21 10:02:00"))
        .unwrap();

    let (status, body) = get(&app, "/usage/by-account", true).await;
    assert_eq!(status, StatusCode::OK);
    let accounts = body["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    // 按 total_tokens 降序。
    assert_eq!(accounts[0]["account"], "a2");
    assert_eq!(accounts[0]["requests"], 1);
    assert_eq!(accounts[0]["total_tokens"], 100);
    assert_eq!(accounts[1]["account"], "a1");
    assert_eq!(accounts[1]["requests"], 2);
    assert_eq!(accounts[1]["total_tokens"], 15);
    assert_eq!(accounts[1]["models"][0][0], "m1");
    assert_eq!(accounts[1]["models"][0][1], 2);
    // 同一个桶也能按键直查。
    assert_eq!(body["by_account"]["a1"]["total_tokens"], 15);
}

#[tokio::test]
async fn usage_analytics_groups_by_date() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:00:00"))
        .unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m2", "a1", 20, "2026-10-21 23:00:00"))
        .unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 30, "2026-10-22 01:00:00"))
        .unwrap();

    let (status, body) = get(&app, "/usage/analytics", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["by_date"]["2026-10-21"]["requests"], 2);
    assert_eq!(body["by_date"]["2026-10-21"]["total_tokens"], 30);
    assert_eq!(body["by_date"]["2026-10-21"]["date"], "2026-10-21");
    assert_eq!(body["by_date"]["2026-10-22"]["requests"], 1);
    assert_eq!(body["by_date"]["2026-10-22"]["total_tokens"], 30);
    assert_eq!(body["summary"]["requests"], 3);
    assert_eq!(body["summary"]["total_tokens"], 60);
}

#[tokio::test]
async fn failed_rows_are_counted_as_errors_not_requests() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    let mut failed = ok_row("m1", "a1", 10, "2026-10-21 10:00:00");
    failed["outcome"] = json!("failed");
    failed["error"] = json!(true);
    failed["status"] = json!(502);
    wb_tasks::usage::append_usage_row(&usage, &failed).unwrap();
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:01:00"))
        .unwrap();

    let (status, body) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["requests"], 1);
    assert_eq!(body["errors"], 1);
    assert_eq!(body["total_requests"], 2);
    assert_eq!(body["total_tokens"], 10);
}

// ---------------------------------------------------------------------------
// chat → 用量落盘（关卡 4.19 线 C）
//
// 假上游走 wiremock，**不发任何真实上游请求**。
// ---------------------------------------------------------------------------

/// 假上游要回的 SSE：一个 content 增量 + 一个带 usage 的 stop，末尾 [DONE]。
///
/// 形状对齐 `contract_chat_routes.rs` 的 fixture：非流式路径靠
/// `aggregate_stream` 把上游 SSE 拼成 chat JSON。
fn sse_body() -> String {
    [
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
}

/// 一个 ready 的账号档案（形状对齐 `contract_chat_routes.rs` 的 `account_doc`）。
fn account_doc(realm: &str) -> String {
    json!({
        "uid": "acc-1",
        "realm": realm,
        "enabled": true,
        "accessToken": "h.p.s",
        "product": "workbuddy",
        "cooldownUntil": 0.0,
        "lastError": ""
    })
    .to_string()
}

/// 起假上游 + 建带账号的 fixture 目录。返回 (TempDir, MockServer)。
async fn chat_fixture(dir: &TempDir, realm: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;
    let accounts = dir.path().join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("acc-1.json"), account_doc(realm)).unwrap();
    server
}

/// POST 一个 JSON body，返回 (状态码, 解析后的 JSON)。
async fn post_json(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {API_KEY}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// 造一份指向假上游的配置（realm intl，账号 acc-1）。
fn chat_cfg(dir: &TempDir, base_url: &str) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().join("accounts"),
        dashboard: dir.path().join("dashboard.html"),
        api_key: Some(API_KEY.to_string()),
        upstream_base_url: Some(base_url.to_string()),
        lan: false,
        realm: "intl".to_string(),
    })
}

#[tokio::test]
async fn openai_chat_appends_one_usage_row() {
    let dir = TempDir::new().unwrap();
    let server = chat_fixture(&dir, "intl").await;
    let cfg = chat_cfg(&dir, &server.uri());
    let app = make_app(&cfg);

    let (status, _) = post_json(
        &app,
        "/v1/chat/completions",
        &json!({"model": "hy4-preview", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let rows = wb_tasks::usage::load_usage_rows(&usage_dir(&cfg)).unwrap();
    assert_eq!(rows.len(), 1, "一次 chat 应当只写一行用量");
    let row = &rows[0];
    assert_eq!(row["model"], "hy4-preview");
    assert_eq!(row["outcome"], "completed");
    assert_eq!(row["stream"], false);
    assert_eq!(row["total_tokens"], 4);
    assert_eq!(row["prompt_tokens"], 3);
    assert_eq!(row["completion_tokens"], 1);
    assert_eq!(row["account"], "acc-1");
    assert_eq!(row["realm"], "intl");
    assert_eq!(row["key"], "");
    assert!(row["at"].as_f64().unwrap() > 0.0);
    assert!(row["elapsed_ms"].as_i64().unwrap() >= 0);
    // usage 读到了 ⇒ 不该打 usage_missing。
    assert!(row.get("usage_missing").is_none());
}

#[tokio::test]
async fn anthropic_chat_appends_one_usage_row() {
    let dir = TempDir::new().unwrap();
    let server = chat_fixture(&dir, "intl").await;
    let cfg = chat_cfg(&dir, &server.uri());
    let app = make_app(&cfg);

    let (status, _) = post_json(
        &app,
        "/v1/messages",
        &json!({
            "model": "hy4-preview",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let rows = wb_tasks::usage::load_usage_rows(&usage_dir(&cfg)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["model"], "hy4-preview");
    assert_eq!(rows[0]["outcome"], "completed");
    assert_eq!(rows[0]["total_tokens"], 4);
    assert_eq!(rows[0]["account"], "acc-1");
}

#[tokio::test]
async fn failed_chat_still_appends_an_error_row() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string(
                json!({
                    "error": {"message": "upstream says no", "type": "invalid_request_error", "code": 429}
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;
    let accounts = dir.path().join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("acc-1.json"), account_doc("intl")).unwrap();
    let cfg = chat_cfg(&dir, &server.uri());
    let app = make_app(&cfg);

    let (status, _) = post_json(
        &app,
        "/v1/chat/completions",
        &json!({"model": "hy4-preview", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "上游 429 应透传状态码");

    let rows = wb_tasks::usage::load_usage_rows(&usage_dir(&cfg)).unwrap();
    assert_eq!(rows.len(), 1, "失败的请求也要记账");
    let row = &rows[0];
    assert_eq!(row["outcome"], "failed");
    assert_eq!(row["error"], true);
    assert_eq!(row["status"], 429);
    assert_eq!(row["model"], "hy4-preview");
}

#[tokio::test]
async fn usage_get_filters_by_realm_like_python() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    // cn 行 + intl 行（当前 realm 是 cn）。
    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:00:00"))
        .unwrap();
    let mut intl = ok_row("m2", "a2", 100, "2026-10-21 10:01:00");
    intl["realm"] = json!("intl");
    wb_tasks::usage::append_usage_row(&usage, &intl).unwrap();

    // 缺省 = 当前 realm（cn）⇒ 只算 cn 行（Python `_get_v1_usage` 同款）。
    let (status, body) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_requests"], 1);
    assert_eq!(body["total_tokens"], 10);

    // `?realm=all` ⇒ 不过滤。
    let (status, body) = get(&app, "/usage?realm=all", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_requests"], 2);
    assert_eq!(body["total_tokens"], 110);

    // `?realm=intl` ⇒ 只算 intl 行。
    let (status, body) = get(&app, "/usage?realm=intl", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_requests"], 1);
    assert_eq!(body["total_tokens"], 100);
}

#[tokio::test]
async fn usage_analytics_reports_all_realms_by_default() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    wb_tasks::usage::append_usage_row(&usage, &ok_row("m1", "a1", 10, "2026-10-21 10:00:00"))
        .unwrap();
    let mut intl = ok_row("m2", "a2", 100, "2026-10-21 10:01:00");
    intl["realm"] = json!("intl");
    wb_tasks::usage::append_usage_row(&usage, &intl).unwrap();

    // analytics 缺省是「全 realm 合并」（Python `_get_usage_analytics` 的
    // `req_realm = ... or None`）。
    let (status, body) = get(&app, "/usage/analytics", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["summary"]["requests"], 2);
    assert_eq!(body["summary"]["total_tokens"], 110);

    // by-account 也从不过滤 realm。
    let (status, body) = get(&app, "/usage/by-account", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["accounts"].as_array().unwrap().len(), 2);
}
