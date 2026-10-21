//! 关卡 4.19（线 A）：定价数据源的 **HTTP 那一半**（只取原文，不塑形）。
//!
//! 权威源：Python `wb_pricing.py` 的 `fetch_openrouter()`（`:1124-1136`）——
//! `GET https://openrouter.ai/api/v1/models`，timeout **45s**，UA
//! `workbuddy2api-hub-pricing/1.0`。
//!
//! ## 边界（与 Python 一致）
//!
//! - **只取回上游原文**：本模块返回 `serde_json::Value`（上游响应体原样解析），
//!   **不塑形**。塑形分两步，都在 `wb-tasks` 侧：
//!   1. `data[]` → `{id: pricing}` 表（Python `fetch_openrouter()` 的返回值语义）；
//!   2. 按 hub 模型名解析出价格条目（`wb_tasks::price::build_snapshot`）。
//! - **失败要抛给调用方**：Python 的 `fetch_openrouter()` 在网络失败时抛异常，
//!   「沿用上一版价格」这个决定由调用方（`wb_tasks::price_refresher`）做出。
//!
//! 契约测试一律走 wiremock mock 上游，**绝不向真实上游发请求**。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::outbound::{describe_error, UpstreamClient};

/// OpenRouter 的模型目录地址。Python `wb_pricing.py:988` 的 `OPENROUTER_URL`。
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// 请求的 User-Agent。Python `wb_pricing.py:1129` 的
/// `{"User-Agent": "workbuddy2api-hub-pricing/1.0"}`。
pub const PRICING_USER_AGENT: &str = "workbuddy2api-hub-pricing/1.0";

/// 抓取超时（秒）。Python `wb_pricing.py:1132` 的 `urlopen(req, timeout=45)`。
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 45.0;

/// 错误文案里最多保留多少字符的响应体。
///
/// Python 的 `urlopen` 在非 2xx 时抛 `HTTPError`，文案里只有状态码；这里多带
/// 一小段响应体便于运维定位，但**必须截断**——上游可能返回很大的错误页。
const MAX_ERROR_BODY_CHARS: usize = 200;

/// 抓一份 OpenRouter 模型列表（**上游原文**，不塑形）。
///
/// `timeout_secs` 是这次请求的总时限；非正数或非有限值按
/// [`DEFAULT_TIMEOUT_SECONDS`] 处理（`Duration::from_secs_f64` 对负数/NaN 会
/// panic，所以这里必须先夹）。
pub async fn fetch_openrouter_models(
    client: &UpstreamClient,
    timeout_secs: f64,
) -> Result<Value, String> {
    fetch_models_from(client, OPENROUTER_MODELS_URL, timeout_secs).await
}

/// 抓一份模型列表原文，**指定地址**（测试用 wiremock 上游时走这里）。
///
/// 与 [`fetch_openrouter_models`] 语义完全一致，只是把地址也做成参数。
pub async fn fetch_models_from(
    client: &UpstreamClient,
    url: &str,
    timeout_secs: f64,
) -> Result<Value, String> {
    let mut headers = HashMap::new();
    headers.insert("User-Agent".to_string(), PRICING_USER_AGENT.to_string());
    headers.insert("Accept".to_string(), "application/json".to_string());

    let timeout = Duration::from_secs_f64(sane_timeout(timeout_secs));
    let response = client
        .get_with_timeout(url, &headers, timeout)
        .await
        .map_err(|err| describe_error(&err))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("读取响应失败: {err}"))?;

    if !status.is_success() {
        // Python 的 `urlopen` 在非 2xx 时抛 HTTPError ⇒ 调用方按「抓取失败」
        // 处理（沿用上一版价格，不写盘）。
        return Err(format!(
            "OpenRouter 返回 {}: {}",
            status.as_u16(),
            truncate(&body, MAX_ERROR_BODY_CHARS)
        ));
    }

    serde_json::from_str::<Value>(&body).map_err(|err| format!("响应不是合法 JSON: {err}"))
}

/// 把超时夹成一个 `Duration::from_secs_f64` 能接受的值。
fn sane_timeout(timeout_secs: f64) -> f64 {
    if timeout_secs.is_finite() && timeout_secs > 0.0 {
        timeout_secs
    } else {
        DEFAULT_TIMEOUT_SECONDS
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// UA 与方法都对，且返回**上游原文**（不塑形）。
    #[tokio::test]
    async fn fetch_sends_user_agent_and_returns_raw_document() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .and(header("user-agent", PRICING_USER_AGENT))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"id": "openai/gpt-4o", "pricing": {}}]})),
            )
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let url = format!("{}/api/v1/models", server.uri());
        let value = fetch_models_from(&client, &url, 5.0)
            .await
            .expect("mock 上游必须成功");

        // 原文透传：data[] 还在，没有被折成 {id: pricing}
        assert_eq!(value["data"][0]["id"], "openai/gpt-4o");
        assert!(value.get("data").is_some());
    }

    /// 非 2xx ⇒ 报错（调用方据此沿用上一版价格）。
    #[tokio::test]
    async fn fetch_reports_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let url = format!("{}/api/v1/models", server.uri());
        let err = fetch_models_from(&client, &url, 5.0)
            .await
            .expect_err("503 必须报错");
        assert!(err.contains("503"), "错误文案要带状态码: {err}");
    }

    /// 坏 JSON ⇒ 报错，不是 panic。
    #[tokio::test]
    async fn fetch_reports_invalid_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let url = format!("{}/api/v1/models", server.uri());
        let err = fetch_models_from(&client, &url, 5.0)
            .await
            .expect_err("坏 JSON 必须报错");
        assert!(err.contains("JSON"), "错误文案要说清是 JSON 的问题: {err}");
    }

    /// 上游挂住 ⇒ 在时限内放弃（Python 是 45s，这里用 150ms 复现同一条路径）。
    #[tokio::test]
    async fn fetch_gives_up_on_slow_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(1500))
                    .set_body_string("{}"),
            )
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let url = format!("{}/api/v1/models", server.uri());
        let err = fetch_models_from(&client, &url, 0.15)
            .await
            .expect_err("慢上游必须超时");
        assert!(err.contains("超时"), "超时要有明确文案: {err}");
    }

    /// 非法超时（0 / 负数 / NaN）不得 panic —— 回落默认时限。
    #[tokio::test]
    async fn illegal_timeout_falls_back_to_default() {
        assert_eq!(sane_timeout(0.0), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(sane_timeout(-1.0), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(sane_timeout(f64::NAN), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(sane_timeout(12.5), 12.5);
    }

    /// 错误文案里的响应体要截断。
    #[test]
    fn error_body_is_truncated() {
        let long = "x".repeat(1000);
        let cut = truncate(&long, MAX_ERROR_BODY_CHARS);
        assert_eq!(cut.chars().count(), MAX_ERROR_BODY_CHARS + 1); // 加一个省略号
    }
}
