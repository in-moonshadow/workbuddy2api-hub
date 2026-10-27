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
//!
//! 关卡 4.19（线 C）另加**用量记账**：[`build_usage_row`] 造一行与 Python
//! `record_usage` 同形的行，[`record_usage`] 把它 append 进
//! `usage/usage.jsonl`。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use serde_json::{Map, Value};

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

/// 一次 chat 的结果：上游应答 + 服务本次请求的账号。
///
/// `account_uid` 是**记账需要的**：Python 的 `record_usage` 把服务本次请求的
/// 账号 uid 写进用量行（`wb_proxy.py:706`），面板的「按账号」视图靠它。
/// Rust 侧在 `run_chat` 里趁池锁还握着时取出来（`Outcome::Served` 带着命中
/// 账号的下标）。
pub struct ChatRun {
    /// 上游应答（`Outcome::Served` 里带原始响应）。
    pub outcome: Outcome,
    /// 服务本次请求的账号 uid（上游 2xx 时有值；失败时 None）。
    pub account_uid: Option<String>,
}

/// 组装 `RetryOptions` 并跑一次上游请求。**不做 stream 分支** —— 流式由
/// M3-b 接管；此处非流式直读 body（用户批准的 5.4 设计点 b）。
pub async fn run_chat(upstream: &Upstream, params: ChatParams<'_>) -> ChatRun {
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
    let outcome = open_upstream(
        &upstream.client,
        &mut pool,
        &upstream.egress,
        &mut degrade,
        &opts,
    )
    .await;
    let account_uid = match &outcome {
        Outcome::Served(_, index) => pool.account(*index).map(|account| account.uid.clone()),
        Outcome::Rejected { .. } => None,
    };
    ChatRun {
        outcome,
        account_uid,
    }
}

// ---------------------------------------------------------------------------
// 用量记账（关卡 4.19 线 C）
// ---------------------------------------------------------------------------

/// 一次 chat 的记账上下文（够写一行用量）。
pub struct UsageContext<'a> {
    /// 请求的模型。
    pub model: &'a str,
    /// 客户端请求体（取 `stream` 标志）。
    pub payload: &'a Value,
    /// 上游 usage 块（没拿到 ⇒ `None`）。
    pub usage: Option<&'a Value>,
    /// 服务本次请求的账号 uid（不知道 ⇒ `None`）。
    pub account: Option<&'a str>,
    /// 记账用的 realm。
    pub realm: &'a str,
    /// 客户端 API key 的**标识**（Python 写的是 settings id，不是密钥本身）。
    ///
    /// Rust 侧只拿得到密钥本体（`Config.api_key`），**绝不能落盘**，故恒为空串。
    pub key: &'a str,
    /// 失败时的 HTTP 状态（成功 ⇒ `None`）。
    pub status: Option<u16>,
    /// 失败时的一句话说明（成功 ⇒ `None`）。
    pub message: Option<&'a str>,
    /// 请求开始时刻（epoch 秒），用来算墙钟耗时。
    pub started: f64,
    /// 流式首字延迟（毫秒）；非流式分支拿不到 ⇒ `None`（落盘为 null）。
    pub ttft_ms: Option<i64>,
    /// 流式生成耗时（毫秒）；非流式分支拿不到 ⇒ `None`（落盘为 null）。
    pub gen_ms: Option<i64>,
    /// OpenRouter 模型索引（供 ensure_policy / current_policy_id 使用）。
    ///
    /// `None` 表示尚未取价或取价失败；调用方（`record_chat_usage`）会在落盘前
    /// 从 `PriceRefresher` 填入。
    pub or_models: Option<std::collections::HashMap<String, serde_json::Value>>,
}

/// 造一行与 Python `record_usage`（`wb_proxy.py:665-740`）同形的用量行。
///
/// 字段与 Python 对齐：`at` / `iso` / `model` / `stream` / `outcome` /
/// `elapsed_ms` / `ttft_ms` / `gen_ms` / `usage_missing` / token 字段 /
/// `account` / `realm` / `key`；失败行另加 `error` / `status` / `message`
/// （Python `record_error`，:781-795）。
///
/// ⚠️ **`outcome` 用 Python 的词表（`completed` / `failed`），不是 `ok` /
/// `error`**：用量日志是 Python 与 Rust **共用**的同一个文件
/// （`<仓库>/usage/usage.jsonl`），Python 的 `row_outcome`（:624-634）原样读
/// 这个字段，而它的折叠把 `outcome != "completed"` 一律当失败 —— 写 `ok`
/// 会被 Python 记成失败，成功率直接算错。
///
/// `cost_policy`：从定价表取 policy id（Python `current_policy_id`，:717）；
/// 定价表未接入或模型无价时省略（与 Python 一致）。
/// `reasoning_effort`（Rust 侧不解析推理档位）暂不写。
pub fn build_usage_row(ctx: &UsageContext<'_>) -> Value {
    let failed = ctx.status.is_some();
    let at = now_epoch();
    let elapsed_ms = ((at - ctx.started) * 1000.0).round().max(0.0) as i64;

    let mut row = Map::new();
    row.insert("at".into(), Value::from(at));
    row.insert("iso".into(), Value::from(wb_tasks::usage::local_iso(at)));
    row.insert("model".into(), Value::from(ctx.model));
    row.insert(
        "stream".into(),
        Value::from(
            ctx.payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    row.insert(
        "outcome".into(),
        Value::from(if failed { "failed" } else { "completed" }),
    );
    if failed {
        row.insert("error".into(), Value::Bool(true));
        if let Some(status) = ctx.status {
            row.insert("status".into(), Value::from(status));
        }
        if let Some(message) = ctx.message {
            let text: String = message.chars().take(200).collect();
            row.insert("message".into(), Value::from(text));
        }
    }
    row.insert("elapsed_ms".into(), Value::from(elapsed_ms));
    // 流式分支会传入 ttft_ms / gen_ms；非流式分支为 None ⇒ 落盘 null
    // （Python 这两个字段在拿不到时也是 None）。gen_ms 若未显式给出但
    // ttft_ms 已知，则用 elapsed_ms - ttft_ms 推出（Python 同款）。
    let ttft_val = ctx.ttft_ms;
    let gen_val = ctx
        .gen_ms
        .or_else(|| ttft_val.map(|t| (elapsed_ms - t).max(0)));
    match ttft_val {
        Some(v) => {
            row.insert("ttft_ms".into(), Value::from(v));
        }
        None => {
            row.insert("ttft_ms".into(), Value::Null);
        }
    };
    match gen_val {
        Some(v) => {
            row.insert("gen_ms".into(), Value::from(v));
        }
        None => {
            row.insert("gen_ms".into(), Value::Null);
        }
    };

    // token 字段：规范要求「缺失则 0」，所以 5 个字段恒写；Python 在
    // usage 缺失时干脆不写这几个键（`row.update(fields)` 的 fields 为空），
    // 数值口径一致（缺 ⇒ 0）。
    let fields = wb_tasks::usage::extract_usage(ctx.usage);
    let has_usage = fields
        .as_object()
        .map(|map| !map.is_empty())
        .unwrap_or(false);
    if !has_usage {
        row.insert("usage_missing".into(), Value::Bool(true));
    }
    for name in [
        "prompt_tokens",
        "completion_tokens",
        "reasoning_tokens",
        "cached_tokens",
        "total_tokens",
    ] {
        let value = fields.get(name).cloned().unwrap_or(Value::from(0));
        row.insert(name.to_string(), value);
    }
    if let Some(credit) = fields.get("credit") {
        row.insert("credit".into(), credit.clone());
    }
    // Python 只在 `prompt_tokens > 0` 时写 `cache_hit_pct`（:727-729）。
    let prompt_tokens = fields
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if prompt_tokens > 0 {
        let cached = fields
            .get("cached_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let pct = (cached as f64 * 100.0 / prompt_tokens as f64 * 10.0).round() / 10.0;
        row.insert("cache_hit_pct".into(), Value::from(pct));
    }

    row.insert("account".into(), Value::from(ctx.account.unwrap_or("")));
    row.insert("realm".into(), Value::from(ctx.realm));
    row.insert("key".into(), Value::from(ctx.key));
    // cost_policy：从定价表取 policy id（Python `current_policy_id`，:717）。
    if let Some(pid) = wb_tasks::pricing_store::current_policy_id(ctx.model, ctx.or_models.as_ref())
    {
        row.insert("cost_policy".into(), Value::from(pid));
    }
    Value::Object(row)
}

/// 造一行用量并落盘。**落盘失败不影响原响应** —— 只打日志（Python
/// `_persist_usage` 同样只 `log(...)`，见 :752-757）。
pub fn record_usage(usage_dir: &Path, ctx: &UsageContext<'_>) {
    let row = build_usage_row(ctx);
    if let Err(err) = wb_tasks::usage::append_usage_row(usage_dir, &row) {
        eprintln!("[wb-api] usage persist failed: {err}");
    }
}

/// 当前 epoch 秒（浮点）。
fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
