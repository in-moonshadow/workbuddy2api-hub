//! 流式转发编排（wb_proxy.py:18457-18582）：把上游 SSE 行流折成下游
//! chat.completion.chunk 事件 payload 列表。
//!
//! 调用方（Task 5 路由层）逐行喂入 feed()（行须已 strip_data_prefix），
//! 流结束调 finish() 取 leak flush / finish 块 / 空流 error / [DONE] 补发，
//! 再用 estimate_usage() 取账本用的 usage（上游无 usage 或 total=0 时兜底估算）。
//!
//! 本模块只产出 payload 串，不碰 HTTP 写入——路由层负责 `data: {payload}\n\n`。

use crate::leaked::{FeedItem, LeakedToolCallStreamFilter};
use crate::stream::estimate_tokens;
use serde_json::{json, Value};

/// chat_chunk（wb_proxy.py:11474-11476）：一个 Chat Completions 流式块。
/// `created` 用注入的 now（秒），测试可固定。
pub fn chat_chunk(
    model: &str,
    delta: Value,
    finish: Option<&str>,
    chunk_id: &str,
    now: f64,
) -> Value {
    json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": now as i64,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

/// chat_text_chunk（wb_proxy.py:11485-11486）。
pub fn chat_text_chunk(model: &str, text: &str, chunk_id: &str, now: f64) -> Value {
    chat_chunk(model, json!({"content": text}), None, chunk_id, now)
}

/// chat_tool_call_chunks（wb_proxy.py:11489-11499）：恢复的调用按结构化
/// tool_calls delta 逐条下发，index 递增。
pub fn chat_tool_call_chunks(model: &str, calls: &[Value], chunk_id: &str, now: f64) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        let delta = json!({
            "tool_calls": [{
                "index": i,
                "id": call.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "type": "function",
                "function": {
                    "name": call.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "arguments": call.get("arguments").and_then(|v| v.as_str()).unwrap_or(""),
                },
            }]
        });
        out.push(chat_chunk(model, delta, None, chunk_id, now));
    }
    out
}

/// chat_finish_chunk（wb_proxy.py:11502-11503）。
pub fn chat_finish_chunk(model: &str, reason: &str, chunk_id: &str, now: f64) -> Value {
    chat_chunk(model, json!({}), Some(reason), chunk_id, now)
}

/// 流式转发器：一次上游流的状态机。
pub struct StreamForwarder {
    model: String,
    chunk_id: String,
    last_usage: Option<Value>,
    streamed_text: Vec<String>,
    filter: LeakedToolCallStreamFilter,
    synthesized_calls: bool,
    pending_finish: Option<String>,
    emitted: bool,
}

impl StreamForwarder {
    pub fn new(model: &str, chunk_id: &str) -> Self {
        Self {
            model: model.into(),
            chunk_id: chunk_id.into(),
            last_usage: None,
            streamed_text: Vec::new(),
            filter: LeakedToolCallStreamFilter::new(),
            synthesized_calls: false,
            pending_finish: None,
            emitted: false,
        }
    }

    /// release（wb_proxy.py:18481-18490）：filter 产出的动作落到下游事件。
    /// 任何写入都算 emitted（Python `write_chunk` 语义）。
    fn release(&mut self, actions: Vec<FeedItem>, out: &mut Vec<String>, now: f64) {
        for action in actions {
            match action {
                FeedItem::Text(t) => {
                    if !t.is_empty() {
                        let p = chat_text_chunk(&self.model, &t, &self.chunk_id, now);
                        out.push(serde_json::to_string(&p).expect("chunk serializes"));
                        self.emitted = true;
                    }
                }
                FeedItem::Calls(calls) => {
                    self.synthesized_calls = true;
                    for p in chat_tool_call_chunks(&self.model, &calls, &self.chunk_id, now) {
                        out.push(serde_json::to_string(&p).expect("chunk serializes"));
                    }
                    self.emitted = true;
                }
            }
        }
    }

    /// 处理上游一行 data 载荷（调用方须已 strip_data_prefix；[DONE] 行由
    /// finish 后置处理，此处与 Python 同样跳过）。返回下游事件 payload 串。
    pub fn feed(&mut self, line: &str, now: f64) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if line.is_empty() || line == "[DONE]" || line.starts_with(':') {
            return out;
        }
        let parsed = serde_json::from_str::<Value>(line);
        match parsed {
            Ok(Value::Object(_)) => {
                let mut chunk = parsed.unwrap();
                if let Some(u) = chunk.get("usage") {
                    let take = match &self.last_usage {
                        None => true,
                        Some(prev) => {
                            let prev_total = prev
                                .get("total_tokens")
                                .and_then(|t| t.as_i64())
                                .unwrap_or(0);
                            u.get("total_tokens").and_then(|t| t.as_i64()).unwrap_or(0)
                                >= prev_total
                        }
                    };
                    if take {
                        self.last_usage = Some(u.clone());
                    }
                }
                if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
                    for ch in choices {
                        if let Some(delta) = ch.get("delta") {
                            if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str())
                            {
                                if !r.is_empty() {
                                    self.streamed_text.push(r.to_string());
                                }
                            }
                            if let Some(piece) = delta.get("content").and_then(|v| v.as_str()) {
                                if !piece.is_empty() {
                                    self.streamed_text.push(piece.to_string());
                                }
                                // Python :18518-18519：content 按住走 leak filter，
                                // release 决定放行散文或合成 tool_calls
                                let actions = self.filter.feed(piece);
                                self.release(actions, &mut out, now);
                            }
                        }
                        if let Some(fr) = ch.get("finish_reason").and_then(|v| v.as_str()) {
                            // 终止符按住：是否 tool_calls 只在流末知道
                            self.pending_finish = Some(fr.to_string());
                        }
                    }
                }
                // Python :18518/18525：content 与 finish_reason 从转发块剥离
                // （content 走 leak_filter，finish 流末再定）
                if let Some(choices) = chunk.get_mut("choices").and_then(|c| c.as_array_mut()) {
                    for ch in choices.iter_mut() {
                        if let Some(delta) = ch.get_mut("delta").and_then(|d| d.as_object_mut()) {
                            delta.remove("content");
                        }
                        if let Some(fr) = ch.get_mut("finish_reason") {
                            *fr = Value::Null;
                        }
                    }
                }
                let text = serde_json::to_string(&chunk).expect("chunk serializes");
                if let Some(cleaned) = crate::stream::clean_chunk(&text) {
                    self.emitted = true;
                    out.push(cleaned);
                }
            }
            Ok(_) | Err(_) => {
                // 非 dict（JSON 数组/标量）或非 JSON 行：Python `else data`
                // 分支走原文 clean_chunk
                if let Some(cleaned) = crate::stream::clean_chunk(line) {
                    self.emitted = true;
                    out.push(cleaned);
                }
            }
        }
        out
    }

    /// 流结束（wb_proxy.py:18532-18564）：flush 残留 + finish 块（流末才知
    /// tool_calls）+ 空流 error + [DONE] 补发。
    pub fn finish(&mut self, now: f64) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let actions = self.filter.flush();
        self.release(actions, &mut out, now);
        if self.synthesized_calls || self.pending_finish.is_some() {
            let reason = if self.synthesized_calls {
                "tool_calls"
            } else {
                self.pending_finish.as_deref().unwrap_or("stop")
            };
            let p = chat_finish_chunk(&self.model, reason, &self.chunk_id, now);
            out.push(serde_json::to_string(&p).expect("chunk serializes"));
        }
        if !self.emitted {
            let err =
                json!({"error": {"message": "empty upstream stream", "type": "server_error"}});
            out.push(err.to_string());
        }
        out.push("[DONE]".into());
        out
    }

    /// usage 兜底估算（wb_proxy.py:18567-18577）：上游无 usage 或 total=0
    /// 且确有文本 → 按 streamed_text 估算；否则原样返回。None = 无可记数。
    pub fn estimate_usage(&self) -> Option<Value> {
        let has_zero_total = self
            .last_usage
            .as_ref()
            .and_then(|u| u.get("total_tokens").and_then(|t| t.as_i64()))
            .unwrap_or(0)
            == 0;
        if self.last_usage.is_none() || has_zero_total {
            let full = self.streamed_text.concat();
            if !full.is_empty() {
                let comp = estimate_tokens(&full);
                let prompt = (comp / 2).max(1);
                return Some(json!({
                    "prompt_tokens": prompt,
                    "completion_tokens": comp,
                    "total_tokens": prompt + comp,
                    "completion_tokens_details": {"reasoning_tokens": 0},
                    "prompt_tokens_details": {"cached_tokens": 0},
                }));
            }
        }
        self.last_usage.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::EgressState;
    use crate::outbound::UpstreamClient;
    use crate::pool::{AccountPool, PoolOptions};
    use crate::prompt::DegradeGate;
    use crate::retry::{open_upstream, Outcome, RetryOptions};
    use crate::stream::strip_data_prefix;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW: f64 = 1_792_000_000.0;

    fn account(uid: &str, realm: &str) -> crate::pool::AccountState {
        let mut a = crate::pool::AccountState::new(uid, realm);
        a.access_token = "test-token".into();
        a
    }

    fn opts<'a>(
        realm: &'a str,
        payload: &'a serde_json::Value,
        base_url: Option<&'a str>,
    ) -> RetryOptions<'a> {
        RetryOptions {
            realm,
            model: Some("gpt-5"),
            session_key: Some("sk"),
            conversation_id: None,
            payload,
            opts: crate::body::BodyOptions::default(),
            base_url,
            auto_switch: false,
            egress_backoff: false,
            prompt_mode: "passthrough",
            now: NOW,
            utc_offset: 8 * 3600,
        }
    }

    /// 上游 SSE 响应 → 已 strip 的 data 载荷行列表（wiremock 全链路喂 feed 用）。
    async fn sse_lines(resp: reqwest::Response) -> Vec<String> {
        let body = resp.text().await.unwrap();
        body.lines()
            .map(strip_data_prefix)
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect()
    }

    #[test]
    fn chunk_builders_match_python_shape() {
        let t = chat_text_chunk("gpt-5", "hi", "chatcmpl-wb", NOW);
        assert_eq!(t["object"], "chat.completion.chunk");
        assert_eq!(t["created"], NOW as i64);
        assert_eq!(t["choices"][0]["delta"]["content"], "hi");
        assert_eq!(t["choices"][0]["finish_reason"], Value::Null);

        let calls = json!([{"id": "call_1", "name": "fn", "arguments": "{}"}]);
        let tc = chat_tool_call_chunks("gpt-5", calls.as_array().unwrap(), "chatcmpl-wb", NOW);
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(
            tc[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "fn"
        );

        let f = chat_finish_chunk("gpt-5", "stop", "chatcmpl-wb", NOW);
        assert_eq!(f["choices"][0]["finish_reason"], "stop");
        assert!(f["choices"][0]["delta"].as_object().unwrap().is_empty());
    }

    #[test]
    fn feed_skips_done_and_comment_lines() {
        let mut fw = StreamForwarder::new("gpt-5", "chatcmpl-wb");
        assert!(fw.feed("[DONE]", NOW).is_empty());
        assert!(fw.feed("", NOW).is_empty());
        assert!(fw.feed(": keep-alive", NOW).is_empty());
    }

    #[test]
    fn usage_fallback_estimates_from_streamed_text() {
        // 上游无 usage → 兜底估算，格式对齐 Python :18569-18577
        let mut fw = StreamForwarder::new("gpt-5", "chatcmpl-wb");
        let chunk = json!({
            "id": "c1", "model": "gpt-5",
            "choices": [{"index": 0, "delta": {"content": "hello world"}, "finish_reason": null}]
        });
        fw.feed(&chunk.to_string(), NOW);
        let u = fw.estimate_usage().unwrap();
        assert_eq!(u["completion_tokens_details"]["reasoning_tokens"], 0);
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 0);
        assert!(u["completion_tokens"].as_i64().unwrap() > 0);
        assert_eq!(
            u["total_tokens"].as_i64().unwrap(),
            u["prompt_tokens"].as_i64().unwrap() + u["completion_tokens"].as_i64().unwrap()
        );
        // 上游有 usage 且 total>0 → 原样透传，不估算
        let mut fw2 = StreamForwarder::new("gpt-5", "chatcmpl-wb");
        let uc = json!({
            "id": "c2", "model": "gpt-5",
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        });
        fw2.feed(&uc.to_string(), NOW);
        assert_eq!(fw2.estimate_usage().unwrap()["total_tokens"], 30);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_forward_full_chain() {
        // 全链路：wiremock 上游 → open_upstream → feed 逐行 → 断言下游事件
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"cmpl-1\",\"model\":\"gpt-5\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"cmpl-1\",\"model\":\"gpt-5\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("Content-Type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let mut p = AccountPool::new(vec![account("u1", "intl")], PoolOptions::default());
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let payload = json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]});
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", &payload, Some(&server.uri())),
        )
        .await;
        let Outcome::Served(resp, _) = out else {
            panic!("expected Served");
        };
        let mut fw = StreamForwarder::new("gpt-5", "chatcmpl-wb");
        let mut events: Vec<String> = Vec::new();
        for line in sse_lines(resp).await {
            if line == "[DONE]" {
                continue;
            }
            events.extend(fw.feed(&line, NOW));
        }
        events.extend(fw.finish(NOW));

        // 首事件：content "Hello" 经 leak filter 放行为 text chunk
        let first: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(first["choices"][0]["delta"]["content"], "Hello");
        // 第二个转发块：content 被剥离，reasoning_content 保留，finish 按住
        let second: Value = serde_json::from_str(&events[1]).unwrap();
        assert_eq!(second["choices"][0]["delta"]["reasoning_content"], "think");
        assert!(second["choices"][0]["delta"].get("content").is_none());
        assert!(second["choices"][0]["finish_reason"].is_null());
        // 流末：finish stop + [DONE]
        let last2 = &events[events.len() - 2..];
        assert_eq!(last2[1], "[DONE]");
        let fin: Value = serde_json::from_str(&last2[0]).unwrap();
        assert_eq!(fin["choices"][0]["finish_reason"], "stop");
        // usage 原样（total=15 > 0 不估算）
        assert_eq!(fw.estimate_usage().unwrap()["total_tokens"], 15);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_recovers_leaked_tool_calls_at_end() {
        // 泄漏的 DSML 调用在流末恢复为 tool_calls，finish_reason=tool_calls
        let server = MockServer::start().await;
        let leaked = "Thought done.\n<run_code><parameter name=\"code\">ls -la</parameter></run_code></invoke>";
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "id": "cmpl-2", "model": "gpt-5",
                "choices": [{"index": 0, "delta": {"content": leaked}, "finish_reason": "stop"}]
            })
        );
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let mut p = AccountPool::new(vec![account("u1", "intl")], PoolOptions::default());
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let payload =
            json!({"model": "gpt-5", "messages": [{"role": "user", "content": "weather?"}]});
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", &payload, Some(&server.uri())),
        )
        .await;
        let Outcome::Served(resp, _) = out else {
            panic!("expected Served");
        };
        let mut fw = StreamForwarder::new("gpt-5", "chatcmpl-wb");
        let mut events: Vec<String> = Vec::new();
        for line in sse_lines(resp).await {
            if line == "[DONE]" {
                continue;
            }
            events.extend(fw.feed(&line, NOW));
        }
        events.extend(fw.finish(NOW));
        // 流末合成 tool_calls 块 + finish tool_calls + [DONE]
        assert!(fw.synthesized_calls);
        let fin: Value = serde_json::from_str(&events[events.len() - 2]).unwrap();
        assert_eq!(fin["choices"][0]["finish_reason"], "tool_calls");
        let calls: Vec<Value> = events
            .iter()
            .filter_map(|ev| {
                let v: Value = serde_json::from_str(ev).ok()?;
                v["choices"][0]["delta"]
                    .get("tool_calls")
                    .and_then(|t| t.as_array())
                    .cloned()
            })
            .flatten()
            .collect();
        assert!(!calls.is_empty(), "must emit synthesized tool call");
        assert_eq!(calls[0]["function"]["name"], "run_code");
    }

    #[tokio::test(start_paused = true)]
    async fn empty_stream_yields_error_then_done() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let mut p = AccountPool::new(vec![account("u1", "intl")], PoolOptions::default());
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let payload = json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]});
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", &payload, Some(&server.uri())),
        )
        .await;
        let Outcome::Served(resp, _) = out else {
            panic!("expected Served");
        };
        let mut fw = StreamForwarder::new("gpt-5", "chatcmpl-wb");
        for line in sse_lines(resp).await {
            fw.feed(&line, NOW);
        }
        let events = fw.finish(NOW);
        // 空流：error 块 + [DONE]，无 finish 块
        let err: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(err["error"]["message"], "empty upstream stream");
        assert_eq!(events[1], "[DONE]");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn nonstream_aggregate_full_chain() {
        // 非流式聚合端到端：wiremock 上游回 SSE 行 → aggregate_stream 折成
        // chat.completion，usage 兜底估算
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"cmpl-3\",\"model\":\"gpt-5\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"one \"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"cmpl-3\",\"model\":\"gpt-5\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"two\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let mut p = AccountPool::new(vec![account("u1", "intl")], PoolOptions::default());
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let payload = json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]});
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", &payload, Some(&server.uri())),
        )
        .await;
        let Outcome::Served(resp, _) = out else {
            panic!("expected Served");
        };
        let lines = sse_lines(resp).await;
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let agg = crate::stream::aggregate_stream(&refs, "gpt-5", "cmpl-3", NOW);
        assert_eq!(agg["object"], "chat.completion");
        assert_eq!(agg["choices"][0]["message"]["content"], "one two");
        assert_eq!(agg["choices"][0]["finish_reason"], "stop");
        // 无 usage → 兜底估算
        let u = &agg["usage"];
        assert!(u["completion_tokens"].as_i64().unwrap() > 0);
        assert_eq!(
            u["total_tokens"].as_i64().unwrap(),
            u["prompt_tokens"].as_i64().unwrap() + u["completion_tokens"].as_i64().unwrap()
        );
    }
}
