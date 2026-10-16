//! 出站 HTTP 客户端（T4b）：向真实上游发送 build_upstream_body 产物 + 出站头。
//! 封装 reqwest（连接池 + rustls），暴露 `post`；SSE 读流、响应分类与重试
//! 由 T4c/T4d 基于 `reqwest::Response` 组装。
//!
//! 契约测试一律走 wiremock mock 上游（M3 约束），本模块不接触真实上游。

/// 出站客户端：默认连接池（复用 reqwest 默认配置）。
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
}
