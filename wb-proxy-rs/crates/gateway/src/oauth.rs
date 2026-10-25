//! 关卡 4.24（线 A）：OAuth 登录的上游 HTTP 那一半。
//!
//! 权威源：Python `wb_accounts.py`
//! - `AccountPool.start_login`（:3334-3354）：POST
//!   `<chat_upstream>/v2/plugin/auth/state?platform=<平台>`，body `{}`。
//! - `AccountPool.poll_login`（:3356-3418）：GET
//!   `<chat_upstream>/v2/plugin/auth/token?state=<state>`。
//! - `poll_login` 内嵌的 profile 跳（:3391-3400）：GET
//!   `<chat_upstream>/v2/plugin/login/account?state=<state>`，带
//!   `Authorization: Bearer <token>`。
//!
//! ## 边界
//!
//! 本模块只负责**把 OAuth 流程的三跳 HTTP 取回原文**（`serde_json::Value`）：
//! - `start_oauth_state` 校验 `data.state`/`data.authUrl` 都非空并组装
//!   [`OAuthLoginInfo`]（对齐 Python :3350-3351 的 RuntimeError）。
//! - `poll_oauth_token` / `fetch_login_account` **不塑形**，原样返回上游应答；
//!   状态判定（[`LOGIN_PENDING`] / code!=0 / token 缺失）与账号落地在调用方
//!   （`wb-tasks` 的 login 路径，Python `poll_login` 的 :3380-3418 那一半）。
//!
//! ## 与 Python 的差异（登记在案）
//!
//! 1. **重试**：Python `http_json` 默认 retries=3（start）/ 2（poll）。Rust
//!    侧走单次请求 + 总时限（与本 crate pricing/github/product_catalog 一致），
//!    重试编排由调用方（tasks 层）决定。
//! 2. **超时语义**：reqwest 的总时限（Python `urlopen(timeout=N)` 是逐次读）。
//!
//! ⛔ 契约测试一律走 wiremock mock 上游，**绝不向真实上游发请求**。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::outbound::{describe_error, UpstreamClient};

/// OAuth 登录第一步：换取 state + authUrl（Python wb_accounts.py:214）。
pub const AUTH_STATE_PATH: &str = "/v2/plugin/auth/state";
/// OAuth 登录第二步：用 state 轮询 accessToken（Python wb_accounts.py:215）。
pub const AUTH_TOKEN_PATH: &str = "/v2/plugin/auth/token";
/// OAuth 登录第三步：取账号资料（昵称等；Python wb_accounts.py:216）。
pub const LOGIN_ACCOUNT_PATH: &str = "/v2/plugin/login/account";
/// 上游仍在等待浏览器授权时的业务码（Python wb_accounts.py:226）。
pub const LOGIN_PENDING: i64 = 11217;

/// 第一步/第二步请求超时（秒）。Python `http_json(..., timeout=30)`。
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0;
/// 账号资料跳请求超时（秒）。Python `urlopen(req_acct, timeout=15)`。
pub const ACCOUNT_TIMEOUT_SECONDS: f64 = 15.0;

/// 错误文案里最多保留多少字符的响应体。
const MAX_ERROR_BODY_CHARS: usize = 200;

/// `start_oauth_state` 的返回：state 与浏览器授权地址。
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthLoginInfo {
    pub state: String,
    pub auth_url: String,
}

/// 三跳共用的请求头（Python start_login :3337-3344 / poll_login :3368-3375）：
/// 计费 UA + Origin/Referer + XHR 标记。
fn oauth_headers(billing_ua: &str, origin: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert(
        "Accept".to_string(),
        "application/json, text/plain, */*".to_string(),
    );
    headers.insert("X-Requested-With".to_string(), "XMLHttpRequest".to_string());
    headers.insert("User-Agent".to_string(), billing_ua.to_string());
    headers.insert("Origin".to_string(), origin.to_string());
    headers.insert("Referer".to_string(), format!("{origin}/"));
    headers
}

/// `urllib.parse.quote` 的等价（Python 默认 safe='/'）：ALPHA/DIGIT 与
/// `_.-~` 原样，`/` 保留，其余字节（含 UTF-8 多字节）按大写 %XX 编码。
fn quote(input: &str) -> String {
    let mut out = String::new();
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 把 JSON 标量转成 Python 的 `str(v or "")` 口径（假值 ⇒ 空串）。
fn text_or_empty(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// 截断一段文本（按字符，不切碎多字节字符）。
fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max_chars).collect();
    format!("{head}…")
}

fn sane_timeout(timeout_secs: f64) -> f64 {
    if timeout_secs.is_finite() && timeout_secs > 0.0 {
        timeout_secs
    } else {
        DEFAULT_TIMEOUT_SECONDS
    }
}

/// 第一步：POST `<chat_upstream>/v2/plugin/auth/state?platform=<平台>`，body `{}`。
///
/// 对齐 Python `start_login` :3336-3354：校验 `data.state` / `data.authUrl` 都
/// 非空，缺一抛错；成功返回 [`OAuthLoginInfo`]。
pub async fn start_oauth_state(
    client: &UpstreamClient,
    chat_upstream: &str,
    platform: &str,
    billing_ua: &str,
    origin: &str,
) -> Result<OAuthLoginInfo, String> {
    let url = format!(
        "{chat_upstream}{AUTH_STATE_PATH}?platform={}",
        quote(platform)
    );
    let headers = oauth_headers(billing_ua, origin);
    let response = client
        .post_with_timeout(
            &url,
            &headers,
            &serde_json::json!({}),
            Duration::from_secs_f64(sane_timeout(DEFAULT_TIMEOUT_SECONDS)),
        )
        .await
        .map_err(|err| describe_error(&err))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("读取响应失败: {err}"))?;

    if !status.is_success() {
        return Err(format!(
            "auth/state 返回 {}: {}",
            status.as_u16(),
            truncate(&body, MAX_ERROR_BODY_CHARS)
        ));
    }

    let payload: Value =
        serde_json::from_str(&body).map_err(|err| format!("响应不是合法 JSON: {err}"))?;
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let state = text_or_empty(data.get("state").unwrap_or(&Value::Null));
    let auth_url = text_or_empty(data.get("authUrl").unwrap_or(&Value::Null));

    if state.is_empty() || auth_url.is_empty() {
        return Err(format!("auth/state 返回无 state/authUrl: {payload}"));
    }

    Ok(OAuthLoginInfo { state, auth_url })
}

/// 第二步：GET `<chat_upstream>/v2/plugin/auth/token?state=<state>`。
///
/// **不塑形**：原样返回上游应答（`serde_json::Value`），状态判定
/// （[`LOGIN_PENDING`] / code!=0 / token 缺失）与账号落地在调用方。
pub async fn poll_oauth_token(
    client: &UpstreamClient,
    chat_upstream: &str,
    state: &str,
    billing_ua: &str,
    origin: &str,
) -> Result<Value, String> {
    let url = format!("{chat_upstream}{AUTH_TOKEN_PATH}?state={}", quote(state));
    let headers = oauth_headers(billing_ua, origin);
    let response = client
        .get_with_timeout(
            &url,
            &headers,
            Duration::from_secs_f64(sane_timeout(DEFAULT_TIMEOUT_SECONDS)),
        )
        .await
        .map_err(|err| describe_error(&err))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("读取响应失败: {err}"))?;

    if !status.is_success() {
        return Err(format!(
            "auth/token 返回 {}: {}",
            status.as_u16(),
            truncate(&body, MAX_ERROR_BODY_CHARS)
        ));
    }

    serde_json::from_str::<Value>(&body).map_err(|err| format!("响应不是合法 JSON: {err}"))
}

/// 第三步：GET `<chat_upstream>/v2/plugin/login/account?state=<state>`，
/// 带 `Authorization: Bearer <token>`。**不塑形**，原样返回上游应答。
pub async fn fetch_login_account(
    client: &UpstreamClient,
    chat_upstream: &str,
    state: &str,
    token: &str,
    billing_ua: &str,
    origin: &str,
) -> Result<Value, String> {
    let url = format!("{chat_upstream}{LOGIN_ACCOUNT_PATH}?state={}", quote(state));
    let mut headers = oauth_headers(billing_ua, origin);
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));

    let response = client
        .get_with_timeout(
            &url,
            &headers,
            Duration::from_secs_f64(sane_timeout(ACCOUNT_TIMEOUT_SECONDS)),
        )
        .await
        .map_err(|err| describe_error(&err))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("读取响应失败: {err}"))?;

    if !status.is_success() {
        return Err(format!(
            "login/account 返回 {}: {}",
            status.as_u16(),
            truncate(&body, MAX_ERROR_BODY_CHARS)
        ));
    }

    serde_json::from_str::<Value>(&body).map_err(|err| format!("响应不是合法 JSON: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const UA: &str = "WorkBuddy/5.5.2";
    const ORIGIN: &str = "https://www.workbuddy.ai";

    #[tokio::test]
    async fn start_returns_state_and_auth_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(AUTH_STATE_PATH))
            .and(query_param("platform", "CLI"))
            .and(header("user-agent", UA))
            .and(header("origin", ORIGIN))
            .and(header("x-requested-with", "XMLHttpRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"state": "st-abc", "authUrl": "https://login.example/abc"}
            })))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let info = start_oauth_state(&client, &server.uri(), "CLI", UA, ORIGIN)
            .await
            .expect("mock 上游必须成功");
        assert_eq!(info.state, "st-abc");
        assert_eq!(info.auth_url, "https://login.example/abc");
    }

    #[tokio::test]
    async fn start_errors_when_state_or_auth_url_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(AUTH_STATE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {}})))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = start_oauth_state(&client, &server.uri(), "CLI", UA, ORIGIN)
            .await
            .expect_err("缺 state/authUrl 必须报错");
        assert!(err.contains("state"), "文案要指到问题字段: {err}");
    }

    #[tokio::test]
    async fn start_errors_on_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(AUTH_STATE_PATH))
            .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = start_oauth_state(&client, &server.uri(), "CLI", UA, ORIGIN)
            .await
            .expect_err("非 2xx 必须报错");
        assert!(err.contains("502"), "文案要带状态码: {err}");
    }

    #[tokio::test]
    async fn poll_returns_the_raw_token_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(AUTH_TOKEN_PATH))
            .and(query_param("state", "st-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": LOGIN_PENDING, "msg": "waiting for browser login"
            })))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let value = poll_oauth_token(&client, &server.uri(), "st-abc", UA, ORIGIN)
            .await
            .expect("mock 上游必须成功");
        assert_eq!(value["code"], LOGIN_PENDING);
        assert_eq!(value["msg"], "waiting for browser login");
    }

    #[tokio::test]
    async fn poll_reports_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(AUTH_TOKEN_PATH))
            .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = poll_oauth_token(&client, &server.uri(), "st-abc", UA, ORIGIN)
            .await
            .expect_err("非 2xx 必须报错");
        assert!(err.contains("502"), "文案要带状态码: {err}");
    }

    #[tokio::test]
    async fn fetch_account_sends_bearer_and_returns_profile() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(LOGIN_ACCOUNT_PATH))
            .and(query_param("state", "st-abc"))
            .and(header("Authorization", "Bearer tok-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"nickname": "nick-1"}
            })))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let value = fetch_login_account(&client, &server.uri(), "st-abc", "tok-1", UA, ORIGIN)
            .await
            .expect("mock 上游必须成功");
        assert_eq!(value["data"]["nickname"], "nick-1");
    }

    #[tokio::test]
    async fn fetch_account_reports_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(LOGIN_ACCOUNT_PATH))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = fetch_login_account(&client, &server.uri(), "st-abc", "tok-1", UA, ORIGIN)
            .await
            .expect_err("非 2xx 必须报错");
        assert!(err.contains("401"), "文案要带状态码: {err}");
    }

    #[test]
    fn quote_matches_python_urlencode_style() {
        assert_eq!(quote("CLI"), "CLI");
        assert_eq!(quote("a b"), "a%20b");
        assert_eq!(quote("a?b=c"), "a%3Fb%3Dc");
        assert_eq!(quote("a/b"), "a/b");
        assert_eq!(quote("中文"), "%E4%B8%AD%E6%96%87");
    }
}
