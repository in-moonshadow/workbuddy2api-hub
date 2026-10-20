//! ACP over streamable HTTP（移植 Python `wb_webagent.py`）。
//!
//! 每日活跃奖励认的是「跑完的 agent 会话」：桌面身分发的
//! `chat/completions` 不计数（issue #75 / #59 实测），只建会话不接沙箱同样
//! 不计数（issue #90 实测）。网页端的顺序是：
//!
//! 1. `GET <link>`，`Accept: text/event-stream` —— 服务端回一条 SSE 流，响应头
//!    带 `Acp-Connection-Id`；
//! 2. `POST <link>`（带 `Acp-Connection-Id`、`Content-Type: application/json`），
//!    body 是 JSON-RPC 请求：`initialize` → `session/load` → `session/prompt`；
//! 3. 服务端把 `session/update` 通知推回 SSE 流里，直到这一轮结束。
//!
//! ## 与 Python 的差异（有意为之，记录在 M4 账本）
//!
//! - Python 用裸 `http.client` + 线程读 SSE；本模块用 reqwest 的流式响应 +
//!   `tokio::spawn` 后台任务读 SSE，语义等价（后台只统计事件数与输出段数，
//!   判定「跑完」靠调用方给的 `poll_status` 轮询）。
//! - Python 支持账号级 HTTP 代理（CONNECT 隧道 + 显式 `Proxy-Authorization`）。
//!   本模块**不支持**代理：当前账号池的 `proxySlot` 全为空（已核实 10 个账号），
//!   且 reqwest 的代理是 client 级配置而非单请求级，与「按账号配代理」的语义
//!   冲突。账号开始配置代理后需要重新评估。
//! - Python 的 SSE 读到 `[DONE]` 或 EOF 才停；本模块读到 EOF 或连接关闭即停，
//!   不把 `[DONE]` 当终止符（SSE 事件流没有 `[DONE]` 约定，那是 chat 流式的）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::outbound::{describe_error, UpstreamClient};

/// ACP 协议版本（Python `wb_webagent.PROTOCOL_VERSION`）。
pub const PROTOCOL_VERSION: i64 = 1;

/// ACP 的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_webagent.AcpChannel._connect`：`http.client(..., timeout=self.timeout)`，
/// 默认 30 秒。Python 是**逐次读**的 socket 超时；Rust 侧用 reqwest 的总时限
/// （见 [`crate::outbound::UpstreamClient::post_with_timeout`]）近似。
pub const ACP_TIMEOUT: Duration = Duration::from_secs(30);

/// 打卡只需要 agent 自己把话说完，不需要客户端提供文件系统/终端能力
/// （Python `wb_webagent.CLIENT_CAPABILITIES`）。
pub fn client_capabilities() -> Value {
    json!({
        "fs": {"readTextFile": false, "writeTextFile": false},
        "terminal": false
    })
}

/// ACP 通道的失败（调用方转成可读的错误文本）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpError(pub String);

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AcpError {}

fn acp_err(msg: impl Into<String>) -> AcpError {
    AcpError(msg.into())
}

/// 一轮 ACP 会话的结果（字段与 Python `run_turn` 的返回 dict 对齐）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcpTurnResult {
    /// 这一轮是否真的跑完（`status == "completed"` 且无错误）。
    pub ok: bool,
    /// 会话状态（`completed` / `failed` / `error` / 空）。
    pub status: String,
    /// 收到的 `session/update` 事件数。
    pub events: u64,
    /// 其中 `agent_message_chunk` 的段数（= 实际输出段数）。
    pub chunks: u64,
    /// 这一轮耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 失败原因（成功时为空串）。
    pub error: String,
}

impl AcpTurnResult {
    /// Python 的结果 dict 形状（`wb_webagent.run_turn` 的返回值）。
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "status": self.status,
            "events": self.events,
            "chunks": self.chunks,
            "elapsed_ms": self.elapsed_ms,
            "error": self.error
        })
    }
}

/// ACP 通道：一条 SSE 收事件，POST 发请求。
///
/// 关键约束（Python `_connect` 的等价物）：SSE 的 GET 与 JSON-RPC 的 POST
/// **走同一个 `link`**，POST 必须带上 GET 响应头里的 `Acp-Connection-Id`。
/// 本模块用同一个 `reqwest::Client` 发两类请求，连接由 reqwest 池管理；
/// `Acp-Connection-Id` 由 [`AcpChannel::open`] 取到并在每次 POST 时带上。
pub struct AcpChannel {
    client: UpstreamClient,
    link: String,
    token: String,
    user_agent: String,
    connection_id: String,
    /// 后台 SSE 读取任务的计数器（`session/update` 数 / `agent_message_chunk` 数）。
    updates: Arc<AtomicU64>,
    chunks: Arc<AtomicU64>,
    /// 后台任务句柄；`close` 时 abort。
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl AcpChannel {
    /// 建立 SSE 通道，返回服务端给的 `Acp-Connection-Id`。
    ///
    /// 对齐 Python `AcpChannel.open`：GET 必须**恰好** 200（Python 写的是
    /// `if resp.status != 200`，不是 `is_success()`），且响应头必须带
    /// `Acp-Connection-Id`，否则报错。
    ///
    /// 这里只等**响应头**（`get` 在收到响应头时就返回），正文由后台任务读。
    /// 等响应头这段时间用 `ACP_TIMEOUT` 兜底：上游接了连接却迟迟不回响应头
    /// 时，Python 的 socket 超时会让 `getresponse()` 抛异常，Rust 也必须放弃。
    pub async fn open(
        client: UpstreamClient,
        link: &str,
        token: &str,
        user_agent: &str,
    ) -> Result<AcpChannel, AcpError> {
        let mut headers = std::collections::HashMap::new();
        headers.insert("Accept".to_string(), "text/event-stream".to_string());
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        headers.insert("User-Agent".to_string(), user_agent.to_string());

        let resp = match tokio::time::timeout(ACP_TIMEOUT, client.get(link, &headers)).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => return Err(acp_err(format!("SSE 通道请求失败: {}", describe_error(&e)))),
            Err(_) => {
                return Err(acp_err(format!(
                    "SSE 通道请求超时（{}s 内没等到响应头）",
                    ACP_TIMEOUT.as_secs()
                )))
            }
        };
        let status = resp.status();
        if status.as_u16() != 200 {
            return Err(acp_err(format!("SSE 通道返回 HTTP {}", status.as_u16())));
        }
        let connection_id = resp
            .headers()
            .get("Acp-Connection-Id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if connection_id.is_empty() {
            return Err(acp_err("SSE 通道没有返回 Acp-Connection-Id"));
        }

        let updates = Arc::new(AtomicU64::new(0));
        let chunks = Arc::new(AtomicU64::new(0));
        let reader = spawn_sse_reader(resp, Arc::clone(&updates), Arc::clone(&chunks));

        Ok(AcpChannel {
            client,
            link: link.to_string(),
            token: token.to_string(),
            user_agent: user_agent.to_string(),
            connection_id,
            updates,
            chunks,
            reader: Some(reader),
        })
    }

    /// 发一个 JSON-RPC 请求；成功返回 HTTP 状态码（正常是 202）。
    ///
    /// 对齐 Python `AcpChannel.request`：状态码不是 200/202 即报错。
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        request_id: i64,
    ) -> Result<u16, AcpError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params
        });
        let mut headers = std::collections::HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert(
            "Accept".to_string(),
            "application/json, text/event-stream".to_string(),
        );
        headers.insert("Acp-Connection-Id".to_string(), self.connection_id.clone());
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        );
        headers.insert("User-Agent".to_string(), self.user_agent.clone());

        let resp = self
            .client
            .post_with_timeout(&self.link, &headers, &body, ACP_TIMEOUT)
            .await
            .map_err(|e| acp_err(format!("{method} 请求失败: {}", describe_error(&e))))?;
        let status = resp.status().as_u16();
        if status != 200 && status != 202 {
            return Err(acp_err(format!("{method} 返回 HTTP {status}")));
        }
        Ok(status)
    }

    /// 当前收到的 `session/update` 事件数。
    pub fn updates(&self) -> u64 {
        self.updates.load(Ordering::Relaxed)
    }

    /// 当前收到的 `agent_message_chunk` 段数。
    pub fn chunks(&self) -> u64 {
        self.chunks.load(Ordering::Relaxed)
    }

    /// 关闭通道（中止后台 SSE 读取）。
    pub fn close(&mut self) {
        if let Some(handle) = self.reader.take() {
            handle.abort();
        }
    }
}

impl Drop for AcpChannel {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for AcpChannel {
    /// 手写而非 derive：`reqwest::Client` 与 `JoinHandle` 都不实现 `Debug`。
    /// 只暴露可诊断的连接信息，不打印 token。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpChannel")
            .field("link", &self.link)
            .field("connection_id", &self.connection_id)
            .field("updates", &self.updates())
            .field("chunks", &self.chunks())
            .finish()
    }
}

/// 后台读 SSE 流，统计 `session/update` 与 `agent_message_chunk`。
///
/// 对齐 Python `_read_events`：逐行读，`data:` 前缀的行解析成 JSON，
/// `method == "session/update"` 记一次事件，其 `params.update.sessionUpdate`
/// 为 `agent_message_chunk` 时再记一段输出。
fn spawn_sse_reader(
    mut resp: reqwest::Response,
    updates: Arc<AtomicU64>,
    chunks: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(_) => break,
            };
            buf.extend_from_slice(&chunk);
            // 按行切分；最后一段不完整的留在 buf 里等下一块。
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                let text = text.trim();
                if !text.starts_with("data:") {
                    continue;
                }
                let payload = text[5..].trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let message: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !message.is_object() {
                    continue;
                }
                if message.get("method").and_then(Value::as_str) == Some("session/update") {
                    updates.fetch_add(1, Ordering::Relaxed);
                    let session_update = message
                        .get("params")
                        .and_then(|p| p.get("update"))
                        .and_then(|u| u.get("sessionUpdate"))
                        .and_then(Value::as_str);
                    if session_update == Some("agent_message_chunk") {
                        chunks.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    })
}

/// 状态轮询回调：返回「这条会话现在什么状态」。
///
/// Python 的 `poll_status` 是同步闭包（内部用 `urllib` 同步 GET）；Rust 的
/// HTTP 是异步的，且稳定版没有 async closure，故用 boxed future 表达。
/// 返回值形状：`{"status": "completed" | "failed" | "error" | ""}`。
pub type PollStatus = Box<
    dyn for<'a> FnMut(
            &'a str,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>>
        + Send,
>;

/// 接上沙箱、把这一轮跑完。
///
/// `poll_status` 是调用方给的「这条会话现在什么状态」（查 console API);
/// 看到 `completed` 就算跑完，`failed`/`error` 直接停。
///
/// 对齐 Python `wb_webagent.run_turn`：三步 JSON-RPC 之后按 `poll_interval`
/// 轮询，最多等 `wait_seconds` 秒；期间失败按 `AcpError` 记到 `error`。
// 参数数目照抄 Python 的 `run_turn`（十个入参一一对应），不为了凑 clippy 而
// 改签名——与 `notes.rs` 的做法一致。
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    client: UpstreamClient,
    link: &str,
    token: &str,
    session_id: &str,
    cwd: &str,
    prompt: &str,
    user_agent: &str,
    mut poll_status: PollStatus,
    wait_seconds: u64,
    poll_interval: u64,
) -> AcpTurnResult {
    let started = std::time::Instant::now();
    let mut result = AcpTurnResult::default();

    let mut channel = match AcpChannel::open(client, link, token, user_agent).await {
        Ok(c) => c,
        Err(e) => {
            result.error = e.0;
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            return result;
        }
    };

    let run = async {
        channel
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": client_capabilities()
                }),
                1,
            )
            .await?;
        channel
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": if cwd.is_empty() { "/workspace" } else { cwd },
                    "mcpServers": []
                }),
                2,
            )
            .await?;
        channel
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": prompt}]
                }),
                3,
            )
            .await?;
        Ok::<(), AcpError>(())
    };

    if let Err(e) = run.await {
        result.error = e.0;
    }

    // 轮询到 completed / failed / 超时。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_seconds.max(1));
    let mut finished = false;
    while result.error.is_empty() && std::time::Instant::now() < deadline {
        let status = poll_status(session_id).await;
        let status = status
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        result.status = status.clone();
        if status == "completed" {
            finished = true;
            break;
        }
        if status == "failed" || status == "error" {
            result.error = format!("会话状态={status}");
            break;
        }
        if poll_interval > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;
        } else {
            // 轮询间隔为 0（测试用）时也让出一次执行权，避免空转占满 CPU。
            tokio::task::yield_now().await;
        }
    }

    // Python `wb_webagent.run_turn`（:246-253）的收尾顺序是「先算耗时、关通道，
    // 再**补查一次状态**，最后才判超时、算 ok」。这里照抄：
    // 耗时必须在补查**之前**算（补查的耗时不计入这一轮）。
    let elapsed_ms = started.elapsed().as_millis() as u64;
    channel.close();

    // 补查：循环退出后状态还是空时再查一次。「上一轮刚跑完、轮询没赶上」这个
    // 窗口很常见（轮询间隔 3s，会话可能刚好在两次轮询之间结束）。漏查会把
    // 已经跑完的会话判成超时，当天的积分白白丢掉。
    // 注意：补查只更新 `status`，不改 `finished`——Python 同样如此。
    if result.status.is_empty() {
        let status = poll_status(session_id).await;
        result.status = status
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }

    if !finished && result.error.is_empty() {
        result.error = format!(
            "会话在 {wait_seconds}s 内没有跑完（状态={}）",
            if result.status.is_empty() {
                "未知"
            } else {
                &result.status
            }
        );
    }
    result.ok = finished && result.error.is_empty();
    result.events = channel.updates();
    result.chunks = channel.chunks();
    result.elapsed_ms = elapsed_ms;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 把「固定返回一个 JSON」的同步闭包包装成 [`PollStatus`]。
    fn fixed(status: &'static str) -> PollStatus {
        Box::new(move |_session_id: &str| Box::pin(async move { json!({"status": status}) }))
    }

    /// 每次调用依次返回给定状态（测试「先 running 后 completed」这类序列）。
    fn sequence(statuses: Vec<&'static str>) -> PollStatus {
        let mut idx = 0usize;
        Box::new(move |_session_id: &str| {
            let s = statuses.get(idx).copied().unwrap_or("");
            idx += 1;
            Box::pin(async move { json!({"status": s}) })
        })
    }

    #[test]
    fn protocol_version_and_capabilities_match_python() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(
            client_capabilities(),
            json!({
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            })
        );
    }

    #[tokio::test]
    async fn open_requires_acp_connection_id_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: {}\n\n"))
            .mount(&server)
            .await;

        let err = AcpChannel::open(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "UA",
        )
        .await
        .expect_err("missing header must fail");
        assert_eq!(err.0, "SSE 通道没有返回 Acp-Connection-Id");
    }

    #[tokio::test]
    async fn open_rejects_non_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let err = AcpChannel::open(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "UA",
        )
        .await
        .expect_err("403 must fail");
        assert_eq!(err.0, "SSE 通道返回 HTTP 403");
    }

    /// 三个 JSON-RPC 调用各打一次，且头带 Acp-Connection-Id。
    #[tokio::test]
    async fn run_turn_sends_three_jsonrpc_calls_with_connection_id() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/acp"))
            .and(header("Accept", "text/event-stream"))
            .and(header("Authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string("data: {}\n\n"),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/acp"))
            .and(header("Acp-Connection-Id", "conn-1"))
            .respond_with(ResponseTemplate::new(202))
            .expect(3)
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "sess-1",
            "/workspace",
            "Hi",
            "UA",
            sequence(vec!["running", "completed"]),
            5,
            0,
        )
        .await;

        assert!(res.ok, "run_turn must succeed: {res:?}");
        assert_eq!(
            res.status, "completed",
            "polling stops as soon as status is completed"
        );
    }

    /// `initialize` 的 body 逐字对齐 Python。
    #[tokio::test]
    async fn initialize_body_matches_python() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;

        let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&captured);
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                sink.lock().unwrap().push(body);
                ResponseTemplate::new(202)
            })
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "sess-1",
            "/workspace",
            "Hi",
            "UA",
            fixed("completed"),
            5,
            0,
        )
        .await;
        assert!(res.ok, "{res:?}");

        let bodies = captured.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3, "three JSON-RPC calls");
        assert_eq!(bodies[0]["method"], "initialize");
        assert_eq!(bodies[0]["jsonrpc"], "2.0");
        assert_eq!(bodies[0]["id"], 1);
        assert_eq!(bodies[0]["params"]["protocolVersion"], 1);
        assert_eq!(
            bodies[0]["params"]["clientCapabilities"],
            json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
        );

        assert_eq!(bodies[1]["method"], "session/load");
        assert_eq!(bodies[1]["id"], 2);
        assert_eq!(bodies[1]["params"]["sessionId"], "sess-1");
        assert_eq!(bodies[1]["params"]["cwd"], "/workspace");
        assert_eq!(bodies[1]["params"]["mcpServers"], json!([]));

        assert_eq!(bodies[2]["method"], "session/prompt");
        assert_eq!(bodies[2]["id"], 3);
        assert_eq!(bodies[2]["params"]["sessionId"], "sess-1");
        assert_eq!(
            bodies[2]["params"]["prompt"],
            json!([{"type": "text", "text": "Hi"}])
        );
    }

    /// 状态为 failed 时不判成功。
    #[tokio::test]
    async fn run_turn_fails_when_status_is_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "s",
            "/workspace",
            "Hi",
            "UA",
            fixed("failed"),
            5,
            0,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error, "会话状态=failed");
    }

    /// 超时未完成 → 失败，error 文案与 Python 一致。
    #[tokio::test]
    async fn run_turn_times_out_when_never_completed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "s",
            "/workspace",
            "Hi",
            "UA",
            fixed("running"),
            1,
            0,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error, "会话在 1s 内没有跑完（状态=running）");
    }

    /// SSE 里的 session/update / agent_message_chunk 计数。
    #[tokio::test]
    async fn sse_events_and_chunks_are_counted() {
        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"method\":\"session/update\",\"params\":{\"update\":{\"sessionUpdate\":\"agent_message_chunk\"}}}\n\n",
            "data: {\"method\":\"session/update\",\"params\":{\"update\":{\"sessionUpdate\":\"agent_message_chunk\"}}}\n\n",
            "data: {\"method\":\"session/update\",\"params\":{\"update\":{\"sessionUpdate\":\"tool_call\"}}}\n\n",
            "data: {\"method\":\"other\"}\n\n"
        );
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "s",
            "/workspace",
            "Hi",
            "UA",
            fixed("completed"),
            5,
            0,
        )
        .await;
        assert!(res.ok);
        assert_eq!(res.events, 3, "three session/update events");
        assert_eq!(res.chunks, 2, "two agent_message_chunk");
    }

    /// POST 不是 200/202 → 报错。
    #[tokio::test]
    async fn run_turn_reports_non_accepted_post_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "s",
            "/workspace",
            "Hi",
            "UA",
            fixed("completed"),
            5,
            0,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error, "initialize 返回 HTTP 500");
    }

    /// SSE 的 GET 必须**恰好** 200：Python 写的是 `if resp.status != 200`，
    /// 2xx 的其它码（201/204）一样要拒。
    #[tokio::test]
    async fn open_rejects_201_even_though_it_is_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(201).insert_header("Acp-Connection-Id", "conn-1"))
            .mount(&server)
            .await;

        let err = AcpChannel::open(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "UA",
        )
        .await
        .expect_err("201 must fail");
        assert_eq!(err.0, "SSE 通道返回 HTTP 201");
    }

    /// 收尾补查：JSON-RPC 失败 ⇒ 轮询循环整段跳过（Python 里 `error` 非空，
    /// 循环条件 `while time.time() < deadline` 仍会跑，但状态始终为空）。
    /// 循环退出后 Python 会**再查一次状态**，把 `status` 填上——即使这一轮
    /// 已经失败，`status` 也要如实反映上游的状态。
    #[tokio::test]
    async fn run_turn_rechecks_status_after_the_loop() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        // JSON-RPC 直接 500 ⇒ 这一轮起不来。
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "s",
            "/workspace",
            "Hi",
            "UA",
            fixed("completed"),
            5,
            0,
        )
        .await;
        assert!(!res.ok, "这一轮本身是失败的");
        assert_eq!(res.error, "initialize 返回 HTTP 500");
        assert_eq!(
            res.status, "completed",
            "循环后必须补查一次状态（Python :249-253）"
        );
    }

    /// 补查也查不到时，状态留空、超时文案用「未知」。
    #[tokio::test]
    async fn run_turn_reports_unknown_status_when_poll_never_sees_a_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/acp"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let res = run_turn(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "s",
            "/workspace",
            "Hi",
            "UA",
            fixed(""),
            1,
            0,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.status, "");
        assert_eq!(res.error, "会话在 1s 内没有跑完（状态=未知）");
    }

    /// 出站头装配：Authorization / User-Agent / Accept 与 Python 一致。
    #[tokio::test]
    async fn sse_request_headers_match_python() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/acp"))
            .and(header("Accept", "text/event-stream"))
            .and(header("Authorization", "Bearer tok"))
            .and(header("User-Agent", "UA/1.0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Acp-Connection-Id", "conn-1")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&server)
            .await;

        let _ = AcpChannel::open(
            UpstreamClient::new(),
            &format!("{}/acp", server.uri()),
            "tok",
            "UA/1.0",
        )
        .await
        .expect("open must succeed");
    }
}
