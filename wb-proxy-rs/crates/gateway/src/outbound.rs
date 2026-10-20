//! 出站 HTTP 客户端（T4b）：向真实上游发送 build_upstream_body 产物 + 出站头。
//! 封装 reqwest（连接池 + rustls），暴露 `post`；SSE 读流、响应分类与重试
//! 由 T4c/T4d 基于 `reqwest::Response` 组装。
//!
//! 契约测试一律走 wiremock mock 上游（M3 约束），本模块不接触真实上游。

use std::time::Duration;

/// 出站客户端：默认连接池（复用 reqwest 默认配置）。
#[derive(Clone)]
pub struct UpstreamClient {
    client: reqwest::Client,
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamClient {
    /// 构造出站客户端。连接池与 TLS 走 reqwest 默认（rustls-tls）。
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("reqwest client build must succeed"),
        }
    }

    /// POST JSON body，携带指定头；返回上游原始响应（由调用方决定
    /// 流式读或聚合）。非 2xx 不抛错——错误分类由调用方按状态码处理。
    pub async fn post(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self.client.post(url).json(body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.send().await
    }

    /// GET，携带指定头；无请求体。语义与 [`UpstreamClient::post`] 一致：
    /// 非 2xx 不抛错，错误分类由调用方按状态码处理。
    ///
    /// 成长任务侧的三个拉取接口与 `_travel_status` 都是 GET，故补此入口；
    /// `post` 的签名与行为未改动。
    pub async fn get(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.send().await
    }

    /// POST JSON body，**带总时限**（对齐 Python `urlopen(req, timeout=N)`）。
    ///
    /// 语义说明：reqwest 的 `RequestBuilder::timeout` 是**总时限**（从开始连接
    /// 到响应体读完），Python 的 `urlopen(timeout=N)` 是**逐次读**的 socket
    /// 超时。对「几 KB 的 JSON 答复」两者没有实质差别；总时限更严格（绝不会
    /// 无限挂住），这正是巡检需要的行为。
    ///
    /// 既有 [`UpstreamClient::post`] **刻意不加**时限：M3 的流式聊天要把响应
    /// 体一路转给用户，总时限会把长回答腰斩。
    pub async fn post_with_timeout(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self.client.post(url).json(body).timeout(timeout);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.send().await
    }

    /// GET，**带总时限**（语义同 [`UpstreamClient::post_with_timeout`]）。
    pub async fn get_with_timeout(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
        timeout: Duration,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self.client.get(url).timeout(timeout);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.send().await
    }
}

/// 把 reqwest 错误翻成可读文本。
///
/// 超时要单独点名：默认文案是「error sending request for url (...)」，
/// 运维看不出是超时还是连接被拒。
pub fn describe_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "请求超时".to_string()
    } else if err.is_connect() {
        format!("连接失败: {err}")
    } else {
        err.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn client_can_be_constructed() {
        let c = UpstreamClient::new();
        let _ = &c.client; // 只验证构造不 panic
    }

    /// T4b 产出物：headers/body 接线 + 请求发送闭环（wiremock mock 上游）。
    #[tokio::test]
    async fn post_sends_headers_and_json_body_to_configured_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("X-Domain", "open.workbuddy.ai"))
            .and(body_json(json!({"model": "m", "stream": true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let mut headers = HashMap::new();
        headers.insert("X-Domain".into(), "open.workbuddy.ai".into());
        let resp = c
            .post(
                &format!("{}/v1/chat/completions", server.uri()),
                &headers,
                &json!({"model": "m", "stream": true}),
            )
            .await
            .expect("reqwest send must succeed");
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.expect("response is json");
        assert_eq!(body["ok"], true);
    }

    /// GET 入口：方法为 GET、头透传、无请求体。
    #[tokio::test]
    async fn get_sends_headers_without_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .and(header("X-Domain", "copilot.tencent.com"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "idle"}})),
            )
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let mut headers = HashMap::new();
        headers.insert("X-Domain".into(), "copilot.tencent.com".into());
        let resp = c
            .get(
                &format!("{}/activity/growth/buddy/travel/status", server.uri()),
                &headers,
            )
            .await
            .expect("reqwest send must succeed");
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.expect("response is json");
        assert_eq!(body["data"]["state"], "idle");
    }

    /// GET 的非 2xx 同样不抛错。
    #[tokio::test]
    async fn get_surfaces_upstream_error_status_without_raising() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let resp = c
            .get(&server.uri(), &HashMap::new())
            .await
            .expect("503 must not raise on the wire");
        assert_eq!(resp.status().as_u16(), 503);
    }

    /// 非 2xx 不抛错——分类归调用方。
    #[tokio::test]
    async fn post_surfaces_upstream_error_status_without_raising() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("upstream throttled"))
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let resp = c
            .post(&server.uri(), &HashMap::new(), &json!({"x": 1}))
            .await
            .expect("429 must not raise on the wire");
        assert_eq!(resp.status().as_u16(), 429);
    }

    /// 带时限的 POST：上游迟迟不回 ⇒ 超时（而不是永远挂着）。
    ///
    /// 这是 M1 的核心产出物：出站调用必须有上限，否则一次上游挂起就把
    /// 整个巡检循环卡死。
    #[tokio::test]
    async fn post_with_timeout_gives_up_on_slow_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(1500))
                    .set_body_string("{}"),
            )
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let err = c
            .post_with_timeout(
                &server.uri(),
                &HashMap::new(),
                &json!({"x": 1}),
                std::time::Duration::from_millis(150),
            )
            .await
            .expect_err("slow upstream must time out");
        assert!(err.is_timeout(), "expected a timeout, got {err:?}");
        assert_eq!(describe_error(&err), "请求超时");
    }

    /// 带时限的 GET：同样要在上限内放弃。
    #[tokio::test]
    async fn get_with_timeout_gives_up_on_slow_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(1500))
                    .set_body_string("{}"),
            )
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let err = c
            .get_with_timeout(
                &server.uri(),
                &HashMap::new(),
                std::time::Duration::from_millis(150),
            )
            .await
            .expect_err("slow upstream must time out");
        assert!(err.is_timeout(), "expected a timeout, got {err:?}");
    }

    /// 不带时限的 `post` 仍然不设上限——M3 流式路径依赖这一点。
    #[tokio::test]
    async fn plain_post_still_has_no_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(600))
                    .set_body_string("{}"),
            )
            .mount(&server)
            .await;

        let c = UpstreamClient::new();
        let resp = c
            .post(&server.uri(), &HashMap::new(), &json!({"x": 1}))
            .await
            .expect("plain post must not time out");
        assert_eq!(resp.status().as_u16(), 200);
    }
}
