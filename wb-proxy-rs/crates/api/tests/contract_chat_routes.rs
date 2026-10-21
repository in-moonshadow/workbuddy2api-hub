//! 关卡 5.4 契约：10 条 chat 路由 + OPTIONS + `/realm` + 404 + 401 信封 + 上游拒绝透传。
//!
//! **全部走 wiremock 假上游**（经 `Config.upstream_base_url` 注入），绝不向真实
//! 上游发请求。假上游返回 **SSE** 而非 JSON —— 因为非流式路径靠
//! `aggregate_stream` 把上游 SSE 拼成 chat JSON（对齐 Python
//! `_chat_nonstream_response` :18584）。
//!
//! 断言聚焦「路由存在 + 信封形状 + 状态码」，不断言上游取值细节
//! （那些由 gateway 层自身单测覆盖）。

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

/// OpenAI 6 条 + Anthropic 4 条（与 routes.rs 的常量一一对应）。
const OPENAI_PATHS: [&str; 6] = [
    "/v1/chat/completions",
    "/chat/completions",
    "/v1/completions",
    "/completions",
    "/v1/responses",
    "/responses",
];
const ANTHROPIC_MESSAGE_PATHS: [&str; 2] = ["/v1/messages", "/messages"];
const ANTHROPIC_COUNT_PATHS: [&str; 2] = ["/v1/messages/count_tokens", "/messages/count_tokens"];

/// 假上游要回的 SSE：一个 content 增量 + 一个带 usage 的 stop，末尾 [DONE]。
/// Python 侧上游回的就是带 `data: ` 前缀的 SSE，故假上游按真实形状给。
fn sse_body() -> String {
    [
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"hy4-preview","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
}

fn account_doc(realm: &str) -> String {
    // expires_at 刻意不设：走 AccountState::ready 的「未设即视为可用」分支
    // （设了则须满足 exp - now > 120）。
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

/// 建 fixture 目录（含 1 个 ready 账号）+ 起假上游，返回 (TempDir, MockServer)。
/// `mock_status` 控制假上游状态码：200 走 SSE 成功路径，其余走 Rejected 透传。
async fn fixture(realm: &str, mock_status: u16) -> (tempfile::TempDir, MockServer) {
    let server = MockServer::start().await;
    let template = if mock_status == 200 {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse_body())
    } else {
        ResponseTemplate::new(mock_status).set_body_string(
            serde_json::json!({
                "error": {"message": "upstream says no", "type": "invalid_request_error", "code": mock_status}
            })
            .to_string(),
        )
    };
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(template)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let acc_dir = accounts_dir(dir.path());
    fs::create_dir_all(&acc_dir).unwrap();
    fs::write(
        acc_dir.join("active_realm.json"),
        format!(r#"{{"realm":"{realm}"}}"#),
    )
    .unwrap();
    // auth_disabled=true：让成功路径用例不被默认鉴权拦住（401 用例单独设 launcher_key）。
    fs::write(
        acc_dir.join("settings.json"),
        r#"{"ui_language":"zh","auth_disabled":true}"#,
    )
    .unwrap();
    fs::write(acc_dir.join("acc-1.json"), account_doc(realm)).unwrap();
    (dir, server)
}

/// 需要鉴权的 fixture（给 401 用例）：settings 带 launcher_key 且未关鉴权。
fn auth_fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let acc_dir = accounts_dir(dir.path());
    fs::create_dir_all(&acc_dir).unwrap();
    fs::write(acc_dir.join("active_realm.json"), r#"{"realm":"intl"}"#).unwrap();
    fs::write(
        acc_dir.join("settings.json"),
        r#"{"ui_language":"zh","launcher_key":"wb-test-key"}"#,
    )
    .unwrap();
    fs::write(acc_dir.join("acc-1.json"), account_doc("intl")).unwrap();
    dir
}

/// 账号目录：放在 tempdir **里面**的 `accounts/`。
///
/// ⚠️ 用量目录是从 `accounts_dir` 的**同级**目录推导的（`usage_dir_from`：
/// `usage/` 与 `accounts/` 同级，对齐 Python `wb_proxy.py:402-403`）。若把
/// accounts_dir 直接设成 tempdir 根，同级目录就变成 `/tmp` ⇒ 测试的用量行
/// 会写到 `/tmp/usage/usage.jsonl`（tempdir 外面，不 hermetic 且每次跑都在长）。
fn accounts_dir(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("accounts");
    // 建目录是这个 helper 的副作用：测试里的账号/realm/settings 都往这里写。
    let _ = std::fs::create_dir_all(&path);
    path
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

async fn post_json(
    cfg: Arc<Config>,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

fn openai_body() -> serde_json::Value {
    serde_json::json!({"model": "hy4-preview", "messages": [{"role": "user", "content": "hi"}]})
}

fn anthropic_body() -> serde_json::Value {
    serde_json::json!({
        "model": "hy4-preview", "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

// ---- 1. OpenAI 6 条路由：全部到达上游并被聚合 ----

#[tokio::test]
async fn openai_paths_all_reach_upstream_and_aggregate() {
    for uri in OPENAI_PATHS {
        let (dir, server) = fixture("intl", 200).await;
        let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
        let (status, value) = post_json(cfg, uri, openai_body()).await;
        assert_eq!(status, StatusCode::OK, "{uri} should be 200");
        if uri.ends_with("responses") {
            // responses 路由经 wrap_response_response 二次转形：输出必须是
            // responses 形状（object=response + output[] + output_text），
            // 而不是 chat 形状 —— 这正是本路由与其余 4 条的差别所在。
            assert_eq!(
                value["object"], "response",
                "{uri} must be a response object"
            );
            assert!(
                value.get("output").is_some(),
                "{uri} must carry output[], got {value}"
            );
            assert_eq!(value["output_text"], "hi", "{uri} must flatten output_text");
            assert!(
                value.get("choices").is_none(),
                "{uri} must NOT leak chat choices, got {value}"
            );
        } else {
            // 其余 OpenAI 路由保持 chat 形状：必有 choices
            assert!(
                value.get("choices").is_some(),
                "{uri} aggregated body must carry choices, got {value}"
            );
        }
    }
}

// ---- 2. Anthropic 2 条 messages 路由：产出 messages 形状 ----

#[tokio::test]
async fn anthropic_message_paths_return_messages_shape() {
    for uri in ANTHROPIC_MESSAGE_PATHS {
        let (dir, server) = fixture("intl", 200).await;
        let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
        let (status, value) = post_json(cfg, uri, anthropic_body()).await;
        assert_eq!(status, StatusCode::OK, "{uri} should be 200");
        // chat_to_messages 产出 anthropic 形状：type=message + content 数组
        assert_eq!(value["type"], "message", "{uri} must be a message object");
        assert!(value.get("content").is_some(), "{uri} must carry content");
    }
}

// ---- 3. Anthropic 2 条 count_tokens 路由（不占槽、不触上游）----

#[tokio::test]
async fn anthropic_count_tokens_paths_return_input_tokens() {
    for uri in ANTHROPIC_COUNT_PATHS {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            accounts_dir(dir.path()).join("active_realm.json"),
            r#"{"realm":"intl"}"#,
        )
        .unwrap();
        fs::write(
            accounts_dir(dir.path()).join("settings.json"),
            r#"{"ui_language":"zh","auth_disabled":true}"#,
        )
        .unwrap();
        // 刻意**不挂 mock、不放账号**：count_tokens 不得触上游。
        let cfg = Arc::new(config(dir.path(), Some("http://127.0.0.1:1"), "intl"));
        let (status, value) = post_json(cfg, uri, anthropic_body()).await;
        assert_eq!(status, StatusCode::OK, "{uri} should be 200");
        assert!(
            value.get("input_tokens").and_then(|v| v.as_i64()).is_some(),
            "{uri} must return input_tokens, got {value}"
        );
    }
}

// ---- 4. OPTIONS 预检 ----

#[tokio::test]
async fn options_preflight_on_chat_paths_is_204_with_star_headers() {
    let dir = tempfile::tempdir().unwrap();
    for uri in OPENAI_PATHS
        .iter()
        .chain(ANTHROPIC_MESSAGE_PATHS.iter())
        .chain(ANTHROPIC_COUNT_PATHS.iter())
    {
        let cfg = Arc::new(config(dir.path(), None, "intl"));
        let response = app(cfg)
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri(*uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "OPTIONS {uri}");
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "*"
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-headers")
                .unwrap(),
            "*"
        );
    }
}

// ---- 5. /realm GET ----

#[tokio::test]
async fn realm_get_reports_process_realm_and_options() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        accounts_dir(dir.path()).join("active_realm.json"),
        r#"{"realm":"cn"}"#,
    )
    .unwrap();
    let cfg = Arc::new(config(dir.path(), None, "cn"));
    let response = app(cfg)
        .oneshot(
            Request::builder()
                .uri("/realm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["realm"], "cn");
    assert_eq!(value["persisted"], "cn");
    assert_eq!(value["options"], serde_json::json!(["cn", "intl"]));
}

// ---- 6. /realm POST ----

#[tokio::test]
async fn realm_post_persists_valid_and_rejects_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(dir.path(), None, "intl"));

    let (status, value) =
        post_json(cfg.clone(), "/realm", serde_json::json!({"realm": "cn"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["ok"], true);
    assert_eq!(value["persisted"], true);
    // 落盘确认
    let written = fs::read_to_string(accounts_dir(dir.path()).join("active_realm.json")).unwrap();
    assert!(
        written.contains("cn"),
        "realm must be persisted, got {written}"
    );

    // 非法值：save_realm 返回 Ok(false) → persisted=false，进程 realm 不变
    let (status, value) = post_json(cfg, "/realm", serde_json::json!({"realm": "mars"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["persisted"], false);
    assert_eq!(value["realm"], "intl");

    // 缺 realm 字段 → 400
    let cfg2 = Arc::new(config(dir.path(), None, "intl"));
    let (status, _) = post_json(cfg2, "/realm", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---- 7. 404 兜底 ----

#[tokio::test]
async fn unknown_path_returns_404_error_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(dir.path(), None, "intl"));
    let response = app(cfg)
        .oneshot(
            Request::builder()
                .uri("/definitely/not/a/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], 404);
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert_eq!(value["error"]["message"], "not found");
}

// ---- 8. 401 信封（OpenAI 与 Anthropic 形状不同）----

#[tokio::test]
async fn unauthorized_returns_protocol_specific_envelopes() {
    // OpenAI 路径：带 code 字段的 OpenAI 信封
    let dir = auth_fixture_dir();
    let cfg = Arc::new(config(dir.path(), None, "intl"));
    let (status, value) = post_json(cfg, "/v1/chat/completions", openai_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(value["error"]["code"], 401);
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("invalid api key -"));

    // Anthropic 路径：无 code 字段的 anthropic 信封
    let dir = auth_fixture_dir();
    let cfg = Arc::new(config(dir.path(), None, "intl"));
    let (status, value) = post_json(cfg, "/v1/messages", anthropic_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "authentication_error");
    assert!(
        value["error"].get("code").is_none(),
        "anthropic envelope must not carry code"
    );
}

// ---- 9. 上游拒绝透传 ----

#[tokio::test]
async fn upstream_rejection_is_passed_through_openai_envelope() {
    let (dir, server) = fixture("intl", 400).await;
    let cfg = Arc::new(config(dir.path(), Some(&server.uri()), "intl"));
    let (status, value) = post_json(cfg, "/v1/chat/completions", openai_body()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // 上游拒绝被包成 "upstream <status>: <原 body>"，并保持 OpenAI 信封形状。
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("upstream")
            && message.contains("400")
            && message.contains("upstream says no"),
        "rejection must carry upstream status + body, got {message}"
    );
    assert_eq!(value["error"]["type"], "server_error");
    assert_eq!(value["error"]["code"], 400);
}

// ---- 10. 池空不得挂起，须给出错误信封 ----
//
// ConcurrencyGate 的 503 需要「池 ready 数 > 0 且并发槽耗尽」或 acquire 超时；
// 单个 axum 实例无法在一轮请求内自我占满 32 槽。并发耗尽语义由
// gateway::pool 的 ConcurrencyGate 单测覆盖；此处只钉「池空 → Rejected →
// 有错误信封」这条集成路径。

#[tokio::test]
async fn empty_pool_does_not_hang_and_returns_error_envelope() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        accounts_dir(dir.path()).join("active_realm.json"),
        r#"{"realm":"intl"}"#,
    )
    .unwrap();
    fs::write(
        accounts_dir(dir.path()).join("settings.json"),
        r#"{"ui_language":"zh","auth_disabled":true}"#,
    )
    .unwrap();
    // 不写任何账号文件 → 池空 → pick_for_session 返回 None → Rejected
    let cfg = Arc::new(config(dir.path(), Some("http://127.0.0.1:1"), "intl"));
    let (status, value) = post_json(cfg, "/v1/chat/completions", openai_body()).await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "empty pool must yield an error status, got {status}"
    );
    assert!(
        value.get("error").is_some(),
        "empty pool must yield an error envelope, got {value}"
    );
}
