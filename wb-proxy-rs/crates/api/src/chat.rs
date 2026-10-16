//! Chat 路由的进程级依赖与请求组装（M3 关卡 5.4）。
//!
//! 本模块只做三件事，**不重复实现 gateway 已有逻辑**：
//! 1. `extract_session_key`：移植 Python `wb_proxy.py:12822-12834` 的会话键优先级。
//! 2. `Upstream`：持有 `UpstreamClient` / `AccountPool` / `EgressState` /
//!    `DegradeGate`，供 `AppState` 单例持有 —— 池与降级窗口必须是**进程级**
//!    状态（Python 是模块全局 `POOL`/`DEGRADE`）。
//! 3. `run_chat`：把 HTTP 层参数装进 `RetryOptions` 并调
//!    `wb_gateway::retry::open_upstream`；出站 URL 拼接与出站头构造**由
//!    open_upstream 内部完成**（retry.rs:347-362），api 层不重复。

use axum::http::HeaderMap;
use serde_json::Value;

use wb_gateway::body::BodyOptions;
use wb_gateway::egress::EgressState;
use wb_gateway::outbound::UpstreamClient;
use wb_gateway::pool::{AccountPool, AccountState, PoolOptions};
use wb_gateway::prompt::DegradeGate;
use wb_gateway::retry::{open_upstream, Outcome, RetryOptions};

/// 会话键头名，顺序即优先级（Python :12822-12834）。
const SESSION_HEADERS: [&str; 4] = [
    "x-conversation-id",
    "conversation-id",
    "x-session-id",
    "session-id",
];

/// extract_session_key（wb_proxy.py:12822-12834）：头优先，其次 body 的三处。
///
/// 每处都 strip 后判非空；全空返回 None。`HeaderMap::get` 的键不区分大小写
/// （axum 内部按小写匹配），常量表里写小写只是让优先级一目了然。
pub fn extract_session_key(headers: &HeaderMap, payload: &Value) -> Option<String> {
    for name in SESSION_HEADERS {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    // body 的三处（顺序同 Python）。
    let from_body = payload
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("session_id").and_then(|v| v.as_str()))
        .or_else(|| {
            payload
                .get("metadata")
                .and_then(|m| m.get("conversation_id"))
                .and_then(|v| v.as_str())
        });
    from_body
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 进程级上游依赖（对应 Python 模块全局 `POOL` / `DEGRADE` / `EGRESS`）。
///
/// `pool` 与 `degrade` 用 `tokio::sync::Mutex`：`open_upstream` 需要 `&mut`
/// 访问（选号后记账、403 触发降级窗口），而 axum handler 可并发。锁在
/// `run_chat` 内跨 await 持有整个请求期间 —— 与 Python `threading.Lock` 语义
/// 一致（Python 同样在处理期间持锁），代价是 chat 请求串行化；M3 是阴影实例，
/// 正确性优先。
pub struct Upstream {
    pub client: UpstreamClient,
    pub pool: tokio::sync::Mutex<AccountPool>,
    pub egress: EgressState,
    pub degrade: tokio::sync::Mutex<DegradeGate>,
}

impl Upstream {
    /// 由账号列表构造（`AccountState` 已含 realm/product/token）。
    ///
    /// `PoolOptions` 用 default：`remaining_priority_enabled=false`、
    /// `expiring_window_days=7.0` —— 对应 Python 设置默认值
    /// （wb_settings.py:102,1152-1163），M5 接面板后再注入真实设置。
    pub fn new(accounts: Vec<AccountState>) -> Self {
        Self {
            client: UpstreamClient::new(),
            pool: tokio::sync::Mutex::new(AccountPool::new(accounts, PoolOptions::default())),
            egress: EgressState::default(),
            degrade: tokio::sync::Mutex::new(DegradeGate::default()),
        }
    }

    /// 就绪账号数（并发闸 resize 用）。
    pub async fn count_ready(&self, realm: &str, model: Option<&str>, now: f64) -> usize {
        self.pool.lock().await.count_ready(Some(realm), model, now)
    }
}

/// 一次 chat 请求的组装参数。字段与 `RetryOptions` 一一对应，但：
/// - `realm`/`now`/`utc_offset` 由调用方（handler）决定，不在此处读时钟；
/// - `base_url` 来自 `Config.upstream_base_url`（生产为 None，测试为 wiremock）。
pub struct ChatParams<'a> {
    pub realm: &'a str,
    pub model: Option<&'a str>,
    pub session_key: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub payload: &'a Value,
    pub opts: BodyOptions<'a>,
    pub base_url: Option<&'a str>,
    pub auto_switch: bool,
    /// realm 出口是否启用出口退避（M3 无代理 → 恒 false，对齐 Python 在无代理
    /// 时对 egress_backoff 的短路）。
    pub egress_backoff: bool,
    pub prompt_mode: &'a str,
    pub now: f64,
    pub utc_offset: i64,
}

/// 组装 `RetryOptions` 并跑一次上游请求。**不做 stream 分支** —— 流式由
/// M3-b 接管；此处非流式直读 body（用户批准的 5.4 设计点 b）。
pub async fn run_chat(upstream: &Upstream, params: ChatParams<'_>) -> Outcome {
    let opts = RetryOptions {
        realm: params.realm,
        model: params.model,
        session_key: params.session_key,
        conversation_id: params.conversation_id,
        payload: params.payload,
        opts: params.opts,
        base_url: params.base_url,
        auto_switch: params.auto_switch,
        egress_backoff: params.egress_backoff,
        prompt_mode: params.prompt_mode,
        now: params.now,
        utc_offset: params.utc_offset,
    };
    let mut pool = upstream.pool.lock().await;
    let mut degrade = upstream.degrade.lock().await;
    open_upstream(
        &upstream.client,
        &mut pool,
        &upstream.egress,
        &mut degrade,
        &opts,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn session_key_prefers_conversation_header() {
        let mut h = HeaderMap::new();
        h.insert("x-conversation-id", HeaderValue::from_static("c1"));
        h.insert("x-session-id", HeaderValue::from_static("s1"));
        assert_eq!(
            extract_session_key(&h, &json!({"session_id": "body"})).as_deref(),
            Some("c1")
        );
    }

    #[test]
    fn session_key_falls_back_to_body_metadata() {
        let h = HeaderMap::new();
        assert_eq!(
            extract_session_key(&h, &json!({"metadata": {"conversation_id": "m1"}})).as_deref(),
            Some("m1")
        );
    }

    #[test]
    fn session_key_blank_header_is_skipped() {
        let mut h = HeaderMap::new();
        h.insert("x-conversation-id", HeaderValue::from_static("   "));
        assert_eq!(
            extract_session_key(&h, &json!({"session_id": "s"})).as_deref(),
            Some("s")
        );
    }

    #[test]
    fn session_key_none_when_absent() {
        assert_eq!(extract_session_key(&HeaderMap::new(), &json!({})), None);
    }
}
