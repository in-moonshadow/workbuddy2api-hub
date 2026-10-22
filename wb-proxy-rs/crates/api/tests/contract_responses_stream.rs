//! 关卡 4.21（线 B）契约：`/v1/responses` 的 `stream: true` 流式事件、
//! `previous_response_id` 的 400、以及回显元数据。
//!
//! **全部走 wiremock 假上游**（经 `Config.upstream_base_url` 注入），绝不向真实
//! 上游发请求。假上游返回 **SSE**（上游本来就是 SSE，非流式路径靠
//! `aggregate_stream` 把它拼成 chat JSON）。
//!
//! 断言聚焦「客户端看到什么」：Content-Type、SSE 帧的事件名与顺序、delta 拼接
//! 后的文本、错误信封形状。

use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `previous_response_id` 的 400 文案（Python :17888-17895 逐字）。
const PREVIOUS_RESPONSE_ID_MESSAGE: &str = concat!(
    "previous_response_id is not supported: this gateway does not store response state. ",
    "Send the full conversation in 'input' instead, or use a stateless client.",
);

/// 假上游回的三片文本，拼接后是 `Hello, world!`。
const UPSTREAM_TEXT: &str = "Hello, world!";

/// 假上游要回的 SSE：一个 content 增量 + 一个带 usage 的 stop，末尾 [DONE]。
fn upstream_sse() -> String {
    [
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello, "},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{"content":"world"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
}

fn account_doc(realm: &str) -> String {
    serde_json::json!({
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

/// 账号目录：放在 tempdir **里面**的 `accounts/`。
///
/// ⚠️ 用量目录是从 `accounts_dir` 的**同级**目录推导的（`usage/` 与 `accounts/`
/// 同级）。accounts_dir 若直接指 tempdir 根，同级目录就变成 `/tmp` ⇒ 测试的
/// 用量行会写到 tempdir 外面（不 hermetic 且每次跑都在长）。
fn accounts_dir(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("accounts");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// 建 fixture 目录（含 1 个 ready 账号）+ 起假上游，返回 (TempDir, MockServer)。
async fn fixture(realm: &str) -> (tempfile::TempDir, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse()),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let acc_dir = accounts_dir(dir.path());
    fs::write(
        acc_dir.join("active_realm.json"),
        format!(r#"{{"realm":"{realm}"}}"#),
    )
    .unwrap();
    // auth_disabled=true：让用例不被默认鉴权拦住。
    fs::write(
        acc_dir.join("settings.json"),
        r#"{"ui_language":"zh","auth_disabled":true}"#,
    )
    .unwrap();
    fs::write(acc_dir.join("acc-1.json"), account_doc(realm)).unwrap();
    (dir, server)
}

fn config(dir: &Path, base_url: Option<&str>, realm: &str) -> Config {
    Config {
        accounts_dir: accounts_dir(dir),
        dashboard: dir.join("dashboard.html"),
        api_key: None,
        lan: false,
        upstream_base_url: base_url.map(str::to_string),
        realm: realm.to_string(),
    }
}

/// 发一次请求，回 (状态码, content-type, 正文)。
async fn post_raw(
    cfg: Arc<Config>,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, String, String) {
    let response = app(cfg)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).to_string(),
    )
}

/// 把 SSE 正文拆成 (事件名, data JSON) 列表。
fn parse_sse(body: &str) -> Vec<(String, serde_json::Value)> {
    body.split("\n\n")
        .map(str::trim)
        .filter(|block| !block.is_empty())
        .map(|block| {
            let mut etype = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    etype = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.trim().to_string();
                }
            }
            let value = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
            (etype, value)
        })
        .collect()
}

fn responses_body(extra: serde_json::Value) -> serde_json::Value {
    let mut body = serde_json::json!({"model": "hy4-preview", "input": "hi"});
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            body[k] = v.clone();
        }
    }
    body
}

// ---- 1. stream: true ⇒ SSE ----

#[tokio::test]
async fn stream_true_returns_sse_with_responses_events() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, content_type, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({"stream": true})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        content_type.starts_with("text/event-stream"),
        "stream:true must answer with SSE, got content-type {content_type}"
    );

    let frames = parse_sse(&body);
    let types: Vec<&str> = frames.iter().map(|(t, _)| t.as_str()).collect();
    assert!(
        types.contains(&"response.created"),
        "SSE must carry response.created, got {types:?}"
    );
    assert!(
        types.contains(&"response.completed"),
        "SSE must carry response.completed, got {types:?}"
    );
    // 生命周期顺序：created 在 completed 之前
    let created = types.iter().position(|t| *t == "response.created").unwrap();
    let completed = types
        .iter()
        .position(|t| *t == "response.completed")
        .unwrap();
    assert!(
        created < completed,
        "created must precede completed, got {types:?}"
    );

    // delta 拼接 == 上游返回的文本
    let text: String = frames
        .iter()
        .filter(|(t, _)| t == "response.output_text.delta")
        .map(|(_, v)| v["delta"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        text, UPSTREAM_TEXT,
        "concatenated output_text.delta must equal upstream text"
    );

    // 收尾帧带完整的响应对象
    let final_response = frames
        .iter()
        .find(|(t, _)| t == "response.completed")
        .map(|(_, v)| v["response"].clone())
        .expect("response.completed frame");
    assert_eq!(final_response["object"], "response");
    assert_eq!(final_response["status"], "completed");
    assert_eq!(final_response["output_text"], UPSTREAM_TEXT);
}

/// `/responses`（无 /v1 前缀）同样支持流式。
#[tokio::test]
async fn stream_true_works_on_both_responses_paths() {
    for uri in ["/v1/responses", "/responses"] {
        let (dir, server) = fixture("intl").await;
        let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
        let (status, content_type, body) = post_raw(
            cfg,
            uri,
            responses_body(serde_json::json!({"stream": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri} body: {body}");
        assert!(
            content_type.starts_with("text/event-stream"),
            "{uri} must answer with SSE, got {content_type}"
        );
        let types: Vec<String> = parse_sse(&body).into_iter().map(|(t, _)| t).collect();
        assert!(
            types.contains(&"response.created".to_string())
                && types.contains(&"response.completed".to_string()),
            "{uri} must carry the lifecycle events, got {types:?}"
        );
    }
}

// ---- 2. stream 缺省 / false ⇒ 仍走非流式（既有行为不许回归）----

#[tokio::test]
async fn stream_absent_still_returns_json_response_object() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, content_type, body) =
        post_raw(cfg, "/v1/responses", responses_body(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !content_type.starts_with("text/event-stream"),
        "no stream flag must NOT answer with SSE, got {content_type}"
    );
    let value: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(value["object"], "response");
    assert_eq!(value["output_text"], UPSTREAM_TEXT);
}

#[tokio::test]
async fn stream_false_still_returns_json_response_object() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, content_type, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({"stream": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !content_type.starts_with("text/event-stream"),
        "stream:false must NOT answer with SSE, got {content_type}"
    );
    let value: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(value["object"], "response");
}

// ---- 3. previous_response_id ⇒ 400（文案与 Python 一致）----

#[tokio::test]
async fn previous_response_id_is_rejected_with_400() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, content_type, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({"previous_response_id": "resp_abc"})),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        !content_type.starts_with("text/event-stream"),
        "a rejected request must not open an SSE stream, got {content_type}"
    );
    let value: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert_eq!(value["error"]["code"], 400);
    assert_eq!(
        value["error"]["message"], PREVIOUS_RESPONSE_ID_MESSAGE,
        "error message must match the Python wording verbatim"
    );

    // 400 必须在**触上游之前**发生：网关无状态，追问不该白花上游的 token。
    let received = server.received_requests().await.unwrap();
    assert!(
        received.is_empty(),
        "previous_response_id must fail fast without touching upstream, got {} requests",
        received.len()
    );
}

/// 空串 / null 不算「带了 id」（Python 真值口径），不该拦。
#[tokio::test]
async fn empty_previous_response_id_is_not_rejected() {
    for value in [serde_json::json!(""), serde_json::json!(null)] {
        let (dir, server) = fixture("intl").await;
        let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
        let (status, _, body) = post_raw(
            cfg,
            "/v1/responses",
            responses_body(serde_json::json!({"previous_response_id": value})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "empty previous_response_id must not be rejected, body: {body}"
        );
    }
}

// ---- 4. 回显元数据（tools / tool_choice / parallel_tool_calls）----
#[tokio::test]
async fn response_echoes_request_capabilities() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, _, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({
            "tools": [{"type": "function", "name": "lookup", "description": "d"}],
            "tool_choice": "required",
            "parallel_tool_calls": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(
        value["tools"][0]["name"], "lookup",
        "tools must be echoed back, got {body}"
    );
    assert_eq!(value["tool_choice"], "required", "got {body}");
    assert_eq!(value["parallel_tool_calls"], false, "got {body}");
}

/// 流式路径同样回显这三个字段（response.completed 的 response 对象里）。
#[tokio::test]
async fn streamed_response_echoes_request_capabilities() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, _, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({
            "stream": true,
            "tools": [{"type": "function", "name": "lookup"}],
            "tool_choice": "required",
            "parallel_tool_calls": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let frames = parse_sse(&body);
    let final_response = frames
        .iter()
        .find(|(t, _)| t == "response.completed")
        .map(|(_, v)| v["response"].clone())
        .expect("response.completed frame");
    assert_eq!(final_response["tools"][0]["name"], "lookup");
    assert_eq!(final_response["tool_choice"], "required");
    assert_eq!(final_response["parallel_tool_calls"], false);
}

// ---- 5. 流式的记账 ----
//
// 流式的 usage 只有流结束才知道 ⇒ 记账在流任务收尾时做（Python 侧同样在
// `_responses_stream_response` 结束后才 `record_usage(stream=True)`）。
// `stream` 标志取自**原始**请求体：翻译后的 chat body 不带它，记账会把它记成
// 非流式。

/// 等用量行落盘（有界轮询：记账发生在流任务里，与客户端收包并发）。
async fn wait_for_usage_rows(dir: &Path) -> Vec<serde_json::Value> {
    let log = dir.join("usage").join("usage.jsonl");
    for _ in 0..200 {
        if let Ok(text) = fs::read_to_string(&log) {
            let rows: Vec<serde_json::Value> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("usage row must be json"))
                .collect();
            if !rows.is_empty() {
                return rows;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn streamed_request_records_usage_row_with_stream_flag() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, _, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({"stream": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let rows = wait_for_usage_rows(dir.path()).await;
    assert_eq!(rows.len(), 1, "streamed request must record one usage row");
    assert_eq!(rows[0]["stream"], true, "usage row must carry stream=true");
    assert_eq!(rows[0]["outcome"], "completed");
    assert_eq!(
        rows[0]["total_tokens"], 5,
        "usage row must carry the upstream usage, got {rows:?}"
    );
}

#[tokio::test]
async fn non_streamed_request_records_usage_row_without_stream_flag() {
    let (dir, server) = fixture("intl").await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, _, body) =
        post_raw(cfg, "/v1/responses", responses_body(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let rows = wait_for_usage_rows(dir.path()).await;
    assert_eq!(
        rows.len(),
        1,
        "non-streamed request must record one usage row"
    );
    assert_eq!(rows[0]["stream"], false);
    assert_eq!(rows[0]["outcome"], "completed");
}

// ---- 6. 流式路径的上游拒绝 ----

/// 假上游直接拒绝（非 200）。
async fn rejecting_fixture(realm: &str, status: u16) -> (tempfile::TempDir, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(
            ResponseTemplate::new(status).set_body_string(
                serde_json::json!({
                    "error": {"message": "upstream says no", "type": "invalid_request_error", "code": status}
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let acc_dir = accounts_dir(dir.path());
    fs::write(
        acc_dir.join("active_realm.json"),
        format!(r#"{{"realm":"{realm}"}}"#),
    )
    .unwrap();
    fs::write(
        acc_dir.join("settings.json"),
        r#"{"ui_language":"zh","auth_disabled":true}"#,
    )
    .unwrap();
    fs::write(acc_dir.join("acc-1.json"), account_doc(realm)).unwrap();
    (dir, server)
}

/// 上游拒绝：回错误信封（**不是** SSE），并记一行失败用量。
#[tokio::test]
async fn upstream_rejection_in_stream_mode_keeps_error_envelope() {
    let (dir, server) = rejecting_fixture("intl", 400).await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, content_type, body) = post_raw(
        cfg,
        "/v1/responses",
        responses_body(serde_json::json!({"stream": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        !content_type.starts_with("text/event-stream"),
        "a rejected request must not open an SSE stream, got {content_type}"
    );
    let value: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(value["error"]["code"], 400);

    // 拒绝也要记一行（outcome=failed），否则这次请求在账本上凭空消失。
    let rows = wait_for_usage_rows(dir.path()).await;
    assert_eq!(rows.len(), 1, "rejected request must record one usage row");
    assert_eq!(rows[0]["outcome"], "failed");
    assert_eq!(rows[0]["status"], 400);
}
