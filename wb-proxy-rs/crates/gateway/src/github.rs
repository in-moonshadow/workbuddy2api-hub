//! 关卡 4.19（线 B）：更新发现的 **HTTP 那一半**（只取原文，不判版本）。
//!
//! 权威源：Python `wb_updates.py`
//!
//! - `fetch_releases()`（`:133-148`）：主路径是 API 那一跳；**API 失败自动退回**
//!   网页端点；回退也失败时抛的是 **API** 的原因（`:147`）。
//! - `_fetch_api()`（`:150-161`）：`GET {RELEASES_URL}`，timeout **5s**，
//!   `User-Agent` + `Accept: application/vnd.github+json`；带 `GITHUB_TOKEN`
//!   时加 `Authorization: Bearer <token>`（`:156-159`）。
//! - `fetch_latest_release()`（`:163-183`）：`GET {LATEST_RELEASE_URL}`，跟完
//!   302 之后从**终态 URL** 里取 tag（`:174-177`）；发布日期这一跳给不出来，
//!   留空（`:181`）。
//!
//! 本模块**只取回上游原文**：判版本（草稿/预发布/版本号大小）在
//! `wb_tasks::update`，本模块一概不碰。
//!
//! ## 与 Python 的逐条差异（登记在案，不粉饰）
//!
//! 1. **超时语义**：Python 的 `urlopen(req, timeout=5)` 是**逐次读**的 socket
//!    超时；这里用 reqwest 的**总时限**（`RequestBuilder::timeout`）。对「几 KB
//!    的 JSON 答复」两者没有实质差别，总时限更严格（绝不会无限挂住）。
//!    （`crates/gateway/src/outbound.rs` 的 `post_with_timeout` 已有同样的说明。）
//! 2. **响应体上限**：Python `response.read(MAX_BODY)` 是**截断**（多出来的
//!    字节直接丢掉）；这里同样按 `MAX_BODY` 截断（边读边截，不把整个响应体
//!    装进内存）。⇒ 一份超过 2MB 的坏文档在两边都是「截断后 JSON 解析失败」。
//! 3. **无效 JSON 不触发回退**：Python 的 `fetch_releases` 只在**抓取本身**
//!    抛异常时回退（`:145`），而 `json.loads` 发生在**回退之外**的
//!    `_run_check`（`:313`）⇒ 「200 + 坏 JSON」在 Python 里是 `invalid JSON`
//!    失败、**不**回退网页端点。本模块照抄这个次序（见 [`ApiOutcome`]）。
//! 4. **网络错误文案**：Python 保留异常类名（`network error: URLError`，
//!    `wb_updates.py:129`）；Rust 侧没有可对应的异常类名，只给
//!    `network error`（与 `wb_tasks::update::FailureKind::Network` 一致）。
//!
//! ## 依赖方向上的一个已知重复
//!
//! `RELEASES_URL` / `LATEST_RELEASE_URL` / `TOKEN_ENV` / `REQUEST_TIMEOUT`
//! 这几枚常量**同时**存在于 `wb_tasks::update` 与本模块。原因是依赖方向是
//! `wb-tasks → wb-gateway`，gateway **不能**反过来依赖 tasks。两份字面量由
//! `crates/tasks/tests/contract_update_checker.rs` 里的一条断言盯着，不许漂。
//! 同理，`tag_from_redirect`（本模块私有）与 `wb_tasks::update` 的同名函数
//! 是两份实现——tag 抽取规则本身只有几行，但跨 crate 无法共用。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::outbound::UpstreamClient;

/// 官方仓库的 releases 接口——主路径。
///
/// 与 `wb_tasks::update::RELEASES_URL` 同值（见模块文档的「已知重复」）。
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/ardeyouxipianyi/workbuddy2api-hub/releases";

/// 回退路径：网页端点的 302 Location 末尾就是最新稳定版 tag。
///
/// 与 `wb_tasks::update::LATEST_RELEASE_URL` 同值（见模块文档的「已知重复」）。
pub const LATEST_RELEASE_URL: &str =
    "https://github.com/ardeyouxipianyi/workbuddy2api-hub/releases/latest";

/// 可选凭据的环境变量名（只从环境变量读，绝不落盘）。
///
/// 与 `wb_tasks::update::TOKEN_ENV` 同值（见模块文档的「已知重复」）。
pub const TOKEN_ENV: &str = "GITHUB_TOKEN";

/// 请求的 User-Agent（Python `wb_updates.py:41`）。
pub const USER_AGENT: &str = "wb2api-hub-update-check";

/// 请求的 `Accept` 头（Python `wb_updates.py:154`）。
pub const ACCEPT: &str = "application/vnd.github+json";

/// 请求超时（秒）。Python `wb_updates.py:42`。
pub const REQUEST_TIMEOUT: f64 = 5.0;

/// 响应体上限（字节）。Python `wb_updates.py:44`。
pub const MAX_BODY: usize = 2 * 1024 * 1024;

/// 网络层失败的短原因（与 `wb_tasks::update::FailureKind::Network` 对齐）。
pub const NETWORK_ERROR: &str = "network error";

/// 响应不是 JSON 时的短原因（与 `wb_tasks::update::FailureKind::InvalidJson` 对齐）。
pub const INVALID_JSON: &str = "invalid JSON";

// ---------------------------------------------------------------------------
// 公开入口
// ---------------------------------------------------------------------------

/// 抓一份 releases 文档（**原文**）。
///
/// 主路径：`GET {RELEASES_URL}`，5s 超时；`token` 非空时加
/// `Authorization: Bearer <token>`。API 那一跳失败（传输错误 / 非 2xx）⇒
/// **退回** `GET {LATEST_RELEASE_URL}`，跟完 302 后从终态 URL 取 tag。
///
/// 返回值是一份**未经判定的 JSON 值**（正常是数组；也可能是别的形状）——
/// 「是不是 releases 数组」「哪个版本更新」由 `wb_tasks::update` 判定。
///
/// 失败时返回一行短原因（`HTTP 403` / `invalid JSON` / `network error`），
/// 不带 URL、请求头与响应体（Python `describe_failure`，`:115-130`）。
pub async fn fetch_github_releases(
    client: &UpstreamClient,
    token: Option<&str>,
) -> Result<Value, String> {
    fetch_releases_at(client, RELEASES_URL, LATEST_RELEASE_URL, token).await
}

/// 同 [`fetch_github_releases`]，但**端点可注入**。
///
/// 这是给契约测试留的接缝：测试把两个端点指到 wiremock 的本地 mock server，
/// 于是整条链路（API → 回退 → 判版本）都能在**不碰真实上游**的前提下跑通。
/// 生产路径只用 [`fetch_github_releases`]。
pub async fn fetch_releases_at(
    client: &UpstreamClient,
    api_url: &str,
    latest_url: &str,
    token: Option<&str>,
) -> Result<Value, String> {
    match fetch_api(client, api_url, token).await {
        ApiOutcome::Document(document) => Ok(document),
        // 「200 + 坏 JSON」在 Python 里**不**回退（模块文档的差异 3）。
        ApiOutcome::Unparsable => Err(INVALID_JSON.to_string()),
        ApiOutcome::Failed(reason) => {
            // API 那一跳失败 ⇒ 退回网页端点（Python `fetch_releases`，`:144-148`）。
            match fetch_latest_release(client, latest_url).await {
                Ok(document) => Ok(document),
                // 回退也失败 ⇒ 抛的是 **API** 的原因（Python `:147`）。
                Err(_) => Err(reason),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 内部实现
// ---------------------------------------------------------------------------

/// API 那一跳的三种结局。
///
/// 拆成三种是为了照抄 Python 的次序：**只有抓取本身失败才回退**
/// （`wb_updates.py:144-148`）；「200 + 坏 JSON」不回退（`:313` 的
/// `json.loads` 在回退之外）。
enum ApiOutcome {
    /// 2xx 且响应体是合法 JSON。
    Document(Value),
    /// 2xx 但响应体不是合法 JSON（Python：解析失败，不回退）。
    Unparsable,
    /// 传输错误或非 2xx（Python：抛异常 ⇒ 回退）。
    Failed(String),
}

/// API 那一跳：`GET {api_url}`，5s 超时，可选 Bearer token。
async fn fetch_api(client: &UpstreamClient, api_url: &str, token: Option<&str>) -> ApiOutcome {
    let mut headers = HashMap::new();
    headers.insert("User-Agent".to_string(), USER_AGENT.to_string());
    headers.insert("Accept".to_string(), ACCEPT.to_string());
    // Python：`token = str(os.environ.get(TOKEN_ENV) or "").strip()`，
    // 非空才加头（`wb_updates.py:156-159`）。
    let token = token.map(str::trim).filter(|value| !value.is_empty());
    if let Some(token) = token {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }

    let response = match client
        .get_with_timeout(api_url, &headers, Duration::from_secs_f64(REQUEST_TIMEOUT))
        .await
    {
        Ok(response) => response,
        Err(_) => return ApiOutcome::Failed(NETWORK_ERROR.to_string()),
    };

    let status = response.status();
    if !status.is_success() {
        return ApiOutcome::Failed(format!("HTTP {}", status.as_u16()));
    }

    match read_body_capped(response).await {
        Ok(body) => match serde_json::from_slice::<Value>(&body) {
            Ok(document) => ApiOutcome::Document(document),
            Err(_) => ApiOutcome::Unparsable,
        },
        Err(reason) => ApiOutcome::Failed(reason),
    }
}

/// 读响应体，超过 `MAX_BODY` 就**截断**（Python `response.read(MAX_BODY)` 的语义）。
///
/// 边读边截 ⇒ 坏响应不能把内存拉满（Python 模块头第 3 条）。
async fn read_body_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() < MAX_BODY {
                    let take = MAX_BODY - body.len();
                    let len = chunk.len().min(take);
                    body.extend_from_slice(&chunk[..len]);
                }
                if body.len() >= MAX_BODY {
                    // 与 Python 的 `read(MAX_BODY)` 一致：到上限就停，不再多读。
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => return Err(NETWORK_ERROR.to_string()),
        }
    }
    Ok(body)
}

/// 回退路径：`GET {latest_url}`，跟完 302，从终态 URL 取 tag。
///
/// Python `fetch_latest_release`（`:163-183`）：`urlopen` 跟完重定向后
/// `geturl()` 就是那条最终地址，tag 直接从 URL 里取——**不解析 HTML**，
/// 页面改版也不会让检查失效。
async fn fetch_latest_release(client: &UpstreamClient, latest_url: &str) -> Result<Value, String> {
    let mut headers = HashMap::new();
    headers.insert("User-Agent".to_string(), USER_AGENT.to_string());

    let response = client
        .get_with_timeout(
            latest_url,
            &headers,
            Duration::from_secs_f64(REQUEST_TIMEOUT),
        )
        .await
        .map_err(|_| NETWORK_ERROR.to_string())?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }

    // reqwest 默认跟随重定向（最多 10 跳）⇒ `url()` 就是终态地址。
    let final_url = response.url().to_string();
    let tag = tag_from_redirect(&final_url)
        .ok_or_else(|| "no release tag in the redirect target".to_string())?;

    Ok(json!([{
        "tag_name": tag,
        "draft": false,
        "prerelease": false,
        "html_url": final_url,
        "published_at": "",
    }]))
}

/// 从终态地址里取出 tag。
///
/// Python `TAG_RE = re.compile(r"/releases/tag/([^/?#]+)")`（`wb_updates.py:37`）
/// —— **搜索**（不是从头匹配），tag 是 `/` `?` `#` 之前的部分。
///
/// 与 `wb_tasks::update::tag_from_redirect` 是两份实现（跨 crate 无法共用，
/// 见模块文档的「已知重复」）。
fn tag_from_redirect(final_url: &str) -> Option<String> {
    const MARKER: &str = "/releases/tag/";
    let index = final_url.find(MARKER)?;
    let rest = &final_url[index + MARKER.len()..];
    let tag: String = rest
        .chars()
        .take_while(|c| !matches!(c, '/' | '?' | '#'))
        .collect();
    if tag.is_empty() {
        None
    } else {
        Some(tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn release(tag: &str) -> Value {
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "html_url": format!("https://github.com/a/b/releases/tag/{tag}"),
            "published_at": "2026-10-08T00:00:00Z",
        })
    }

    /// 主路径：API 那一跳成功 ⇒ 原样返回 releases 文档。
    #[tokio::test]
    async fn api_path_returns_the_document_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([release("v1.6.18"), release("v1.6.17")])),
            )
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let document = fetch_releases_at(
            &client,
            &format!("{}/releases", server.uri()),
            &format!("{}/latest", server.uri()),
            None,
        )
        .await
        .expect("api path must succeed");
        let entries = document.as_array().expect("document is an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["tag_name"], "v1.6.18");
    }

    /// 匿名配额被同出口的邻居耗光（403）⇒ 自动退回网页端点。
    #[tokio::test]
    async fn quota_error_falls_back_to_the_web_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/releases/tag/v1.6.20"),
            )
            .mount(&server)
            .await;
        // 302 的落点（真实世界里是一个 HTML 页面；这里只要 200 即可）。
        Mock::given(method("GET"))
            .and(path("/releases/tag/v1.6.20"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html/>"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let document = fetch_releases_at(
            &client,
            &format!("{}/releases", server.uri()),
            &format!("{}/latest", server.uri()),
            None,
        )
        .await
        .expect("the fallback must answer");
        let entries = document.as_array().expect("document is an array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["tag_name"], "v1.6.20");
        assert_eq!(entries[0]["draft"], false);
        assert_eq!(entries[0]["prerelease"], false);
        assert_eq!(entries[0]["published_at"], "");
    }

    /// 回退也失败 ⇒ 抛的是 **API** 的原因（Python `wb_updates.py:147`）。
    #[tokio::test]
    async fn a_failing_fallback_still_reports_the_api_cause() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let error = fetch_releases_at(
            &client,
            &format!("{}/releases", server.uri()),
            &format!("{}/latest", server.uri()),
            None,
        )
        .await
        .expect_err("both hops failed");
        assert_eq!(error, "HTTP 403");
    }

    /// 可选 token 走 `Authorization: Bearer <token>`（Python `:156-159`）。
    #[tokio::test]
    async fn the_optional_token_is_sent_as_a_bearer_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases"))
            .and(header("Authorization", "Bearer ghp-not-a-real-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let document = fetch_releases_at(
            &client,
            &format!("{}/releases", server.uri()),
            &format!("{}/latest", server.uri()),
            Some("ghp-not-a-real-token"),
        )
        .await
        .expect("api path must succeed");
        assert!(document.as_array().is_some());
    }

    /// 空 token 不加 Authorization 头（mock 上没有该头匹配器 ⇒ 加了就会 404）。
    #[tokio::test]
    async fn an_empty_token_adds_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let document = fetch_releases_at(
            &client,
            &format!("{}/releases", server.uri()),
            &format!("{}/latest", server.uri()),
            Some("   "),
        )
        .await
        .expect("api path must succeed");
        assert!(document.as_array().is_some());
    }

    /// 「200 + 坏 JSON」⇒ `invalid JSON`，且**不**回退（Python 口径）。
    #[tokio::test]
    async fn an_unparsable_body_is_reported_without_falling_back() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{ not json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/releases/tag/v1.6.20"),
            )
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let error = fetch_releases_at(
            &client,
            &format!("{}/releases", server.uri()),
            &format!("{}/latest", server.uri()),
            None,
        )
        .await
        .expect_err("the body is not json");
        assert_eq!(error, INVALID_JSON);
    }

    /// 网络层失败 ⇒ `network error`（不带 URL 与响应体）。
    #[tokio::test]
    async fn a_transport_error_is_reported_as_network_error() {
        let client = UpstreamClient::new();
        let error = fetch_releases_at(
            &client,
            "http://127.0.0.1:1/releases",
            "http://127.0.0.1:1/latest",
            None,
        )
        .await
        .expect_err("nothing is listening");
        assert_eq!(error, NETWORK_ERROR);
    }

    /// tag 从终态 URL 里取（Python `TAG_RE` 口径）。
    #[test]
    fn the_tag_comes_from_the_redirect_target() {
        assert_eq!(
            tag_from_redirect("https://github.com/a/b/releases/tag/v1.6.20"),
            Some("v1.6.20".to_string())
        );
        assert_eq!(tag_from_redirect("https://github.com/a/b"), None);
        assert_eq!(
            tag_from_redirect("https://github.com/a/b/releases/tag/"),
            None
        );
        assert_eq!(
            tag_from_redirect("https://github.com/a/b/releases/tag/v1.6.20?x=1"),
            Some("v1.6.20".to_string())
        );
    }
}
