//! 关卡 4.14：国际版每日活跃（daily_chat）——**桌面腿 + 网页腿 ACP 会话**。
//!
//! 权威参照 Python `wb_accounts.py`：
//! - `daily_chat_web` / `_daily_chat_web_upstream`（:1461-1539）——网页腿
//! - `daily_chat` / `_daily_chat_upstream`（:1560-1661）——两腿编排
//! - `web_headers`（:1449-1459）
//!
//! 网页腿的 ACP 会话协议（`initialize` / `session/load` / `session/prompt`
//! + 状态轮询）已移植到 `wb_gateway::acp`（对照 Python `wb_webagent.py`）。
//!
//! ## 两条腿
//!
//! 1. **桌面腿**：`POST {chat_base}/v2/chat/completions`，body 逐字照抄
//!    Python（`model` / `messages` / `stream` / `max_tokens` /
//!    `reasoning_effort`），再过 `wb_gateway::body::build_upstream_body`
//!    变形（注入 system 提示、强制 `stream:true`）。
//!    `chat_base` **按账号身份**（realm/product）解析——国际版是
//!    `www.workbuddy.ai`，不是国内版常量（这是 M4-b 的核心缺口）。
//! 2. **网页腿**：网页版 console 建会话 → 取沙箱 `link`/`token` → ACP 三步
//!    JSON-RPC → 轮询到 `completed`。**这才是官方「每日活跃」计数的腿**
//!    （issue #75/#59：只建会话不算数；#90：不接沙箱会话永远停��� CREATING）。
//!
//! ## 渠道校验（code 11128）
//!
//! 桌面腿可能被上游 WAF 以 `HTTP 400` + `"code": 11128` 拦下（「这次调用没
//! 通过渠道校验」）。这不是网络故障，改请求头也修不好。Python 的处理是
//! **降级**而非失败：把桌面腿记成「这一腿没走成」，继续跑网页腿——否则一天
//! 的 30/50 积分会因为一条**不计数**的腿而整天丢光。
//!
//! ## `lastDailyChat` 什么时候写
//!
//! **只在网页腿真的跑完时写**（Python :1650-1656）。桌面腿成功但网页腿失败
//! 时若照写，当天后续巡检会直接跳过该账号，那天的 30/50 积分静默丢掉且
//! 不再重试（面板还显示成功）。没开网页腿（`daily_chat_web=false`）时维持
//! 旧语义：桌面腿成功即算完成。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

use wb_gateway::acp::{self, PollStatus};
use wb_gateway::body::{build_upstream_body, BodyOptions};
use wb_gateway::endpoints::chat_base_url;
use wb_gateway::outbound::{describe_error, UpstreamClient};
use wb_storage::{write_idempotent_fields, IdempotentFields};

use crate::activity;
use crate::growth_runner::Clock;

// ---------------------------------------------------------------------------
// 常量（逐字对齐 Python wb_accounts.py:39-55）
// ---------------------------------------------------------------------------

/// 每日活跃用的模型（Python `DAILY_CHAT_MODEL`）。
pub const DAILY_CHAT_MODEL: &str = "deepseek-v4.1-flash";
/// 网页腿的提示词（Python `DAILY_CHAT_WEB_PROMPT`）。
pub const DAILY_CHAT_WEB_PROMPT: &str = "Hi";
/// 网页版 app 的 User-Agent（Python `WEB_USER_AGENT`）。
pub const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
/// 网页版 console 的 origin（Python `WEB_ORIGIN`）。国际版固定这个域名。
pub const WEB_ORIGIN: &str = "https://www.workbuddy.ai";
/// 会话集合的路径部分（Python `WEB_CONVERSATIONS_URL` 的 path）。
pub const WEB_CONVERSATIONS_PATH: &str = "/console/as/conversations/";
/// 桌面对话路径（Python `wb_proxy.CHAT_PATH`）。
pub const CHAT_PATH: &str = "/v2/chat/completions";
/// 网页腿的等待上限（秒）（Python `WEB_TURN_TIMEOUT`，默认 120）。
pub const WEB_TURN_TIMEOUT: u64 = 120;
/// 网页腿的状态轮询间隔（秒）。
pub const WEB_POLL_INTERVAL: u64 = 3;
/// 桌面腿的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_accounts.py:1589` 的 `urlopen(req, timeout=20)`。
pub const DESKTOP_TIMEOUT: Duration = Duration::from_secs(20);
/// 网页腿 console 调用的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_accounts.py:1495`（建会话）与 `:1545`（查会话/查状态）的
/// `urlopen(..., timeout=30)`。
pub const WEB_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// 配置 / 上下文 / 结果
// ---------------------------------------------------------------------------

/// 每日活跃的运行配置。
///
/// 端点可注入：契约测试必须把两条腿都指向 mock 上游（M4 硬约束「不得对真实
/// 上游发任何请求」），生产口径则按账号身份解析。
#[derive(Debug, Clone)]
pub struct DailyChatConfig {
    /// 桌面腿的 chat base；`None` = 按账号 realm/product 解析
    /// （[`wb_gateway::endpoints::chat_base_url`]）。
    pub chat_base: Option<String>,
    /// 网页腿的 console base；`None` = 国际版 [`WEB_ORIGIN`]。
    pub web_base: Option<String>,
    /// 网页腿 ACP 的等待上限（秒）。
    pub web_turn_timeout: u64,
    /// 网页腿的状态轮询间隔（秒）。
    pub web_poll_interval: u64,
}

impl Default for DailyChatConfig {
    fn default() -> Self {
        Self {
            chat_base: None,
            web_base: None,
            web_turn_timeout: WEB_TURN_TIMEOUT,
            web_poll_interval: WEB_POLL_INTERVAL,
        }
    }
}

impl DailyChatConfig {
    /// 桌面腿的 chat base：优先用注入值，否则按账号身份解析。
    pub fn resolved_chat_base(&self, realm: &str, product: &str) -> String {
        self.chat_base
            .clone()
            .unwrap_or_else(|| chat_base_url(realm, product))
    }

    /// 网页腿的 console base：优先用注入值，否则国际版 [`WEB_ORIGIN`]。
    pub fn resolved_web_base(&self) -> String {
        self.web_base
            .clone()
            .unwrap_or_else(|| WEB_ORIGIN.to_string())
    }
}

/// 每日活跃需要的账号身份。
pub struct DailyChatAccount<'a> {
    pub uid: &'a str,
    pub token: &'a str,
    /// `intl` / `cn`；每日活跃只对国际版开放（Python :1573）。
    pub realm: &'a str,
    /// 桌面端身分（`workbuddy` / `vscode` / `cli`），决定 chat base。
    pub product: &'a str,
    /// 桌面腿的出站头（`wb_gateway::headers::chat_request_headers` 的产物）。
    pub chat_headers: &'a HashMap<String, String>,
}

/// 落盘上下文（形状对齐 `checkin::CheckinCtx`）。
pub struct DailyChatCtx<'a> {
    /// 账号池目录（`accounts/`）。
    pub dir: &'a Path,
    /// 账号文件名 `<safe_uid>.json`（**不是**裸 uid）——`wb_storage` 只认这个。
    pub account_name: &'a str,
    /// 本地时间源；`lastDailyChat` 用它格式化。
    pub clock: &'a dyn Clock,
}

/// 每日活跃的结果。
///
/// `body` 的键集与顺序逐字对齐 Python `_daily_chat_upstream` 的返回 dict；
/// `persist_error` 是 Rust 侧独有的（判定成功但落盘失败时可见），
/// 与 `checkin::CheckinOutcome` 同款做法。
#[derive(Debug, Clone)]
pub struct DailyChatOutcome {
    pub body: Value,
    /// 判定成功但 `lastDailyChat` 落盘失败时的原因。
    pub persist_error: Option<String>,
}

impl DailyChatOutcome {
    /// 失败结果：Python 失败分支一律是 `{"ok": false, "error": ...}`。
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            body: json!({"ok": false, "error": error.into()}),
            persist_error: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// Python `text[:n]`：按**字符**截断（不是字节）。
fn truncate(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Python `urllib.parse.quote(s)`：默认 `safe='/'`，保留字母数字与
/// `_.-~/`，其余按 UTF-8 字节做 `%XX`（大写十六进制）。
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Python `%s` 格式化一个 JSON 值（`None` → `"None"`、`True` → `"True"`）。
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python 的真值判断（`if not conversation:` 这类）。
///
/// Python 里 `0`、`""`、`[]`、`{}`、`False`、`None` 都是假值。Rust 的
/// `Value` 没有统一的「空」概念，得按类型分别判。
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// 从一个对象里按顺序取第一个非空字符串字段。
fn first_str<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|k| obj.get(*k).and_then(Value::as_str))
        .find(|s| !s.is_empty())
}

/// 上游「渠道校验」拦截：body 里出现 `"code": 11128`。
///
/// 对齐 Python `_DAILY_CHAT_CHANNEL_RE = re.compile(r'"code"\s*:\s*"?11128"?')`。
/// 命中即把桌面腿降级到网页腿（而不是判失败）。
pub fn is_channel_blocked(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut from = 0usize;
    while let Some(pos) = find_sub(bytes, b"\"code\"", from) {
        let mut i = pos + 6;
        i = skip_ws(bytes, i);
        if i < bytes.len() && bytes[i] == b':' {
            i = skip_ws(bytes, i + 1);
            // 数字前的引号是可选的（`"code": "11128"` 也算命中）。
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
            }
            if bytes.len() >= i + 5 && &bytes[i..i + 5] == b"11128" {
                return true;
            }
        }
        from = pos + 6;
    }
    false
}

/// 朴素子串查找（返回第一个命中的起始下标）。
fn find_sub(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// 跳过 Python `\s` 认得的空白（空格 / 制表 / 换行 / 回车 / 换页 / 竖直制表）。
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b) {
        i += 1;
    }
    i
}

/// 网页版 app 的出站头（Python `wb_accounts.web_headers` :1449-1459）。
///
/// 只有 bearer 与 `X-User-Id`，**没有**桌面端指纹。
/// `origin` 传网页腿的 console base（生产 = `WEB_ORIGIN`）。
pub fn web_headers(uid: &str, token: &str, origin: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    headers.insert("X-User-Id".to_string(), uid.to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert(
        "Accept".to_string(),
        "application/json, text/plain, */*".to_string(),
    );
    headers.insert("Origin".to_string(), origin.to_string());
    headers.insert("Referer".to_string(), format!("{origin}/app"));
    headers.insert("User-Agent".to_string(), WEB_USER_AGENT.to_string());
    headers
}

/// `settings.json` 的 `daily_chat_web` 开关（Python `wb_settings.daily_chat_web`）。
///
/// 键缺失 / `null` → **开**（默认开，Python :1200-1211）；值必须是 JSON 的
/// `true` 才算开（`"true"` 字符串、1 都不算——对齐 Python 的 `value is True`）。
pub fn settings_daily_chat_web(accounts_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(accounts_dir.join("settings.json")) else {
        return true;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return true;
    };
    match doc.get("daily_chat_web") {
        None | Some(Value::Null) => true,
        Some(v) => v.as_bool() == Some(true),
    }
}

// ---------------------------------------------------------------------------
// 桌面腿
// ---------------------------------------------------------------------------

/// 桌面腿的三种结局。
enum DesktopLeg {
    /// HTTP 2xx。Python 只看状态码，不看 body 里的 `code`。
    Ok,
    /// HTTP 4xx/5xx 且 body 含 `"code": 11128` → 渠道校验拦截，**降级**。
    ChannelBlocked(String),
    /// 其他失败。
    Failed(String),
}

/// 跑一次桌面腿的轻量对话。
async fn run_desktop_leg(
    client: &UpstreamClient,
    acc: &DailyChatAccount<'_>,
    chat_base: &str,
) -> DesktopLeg {
    let url = format!("{chat_base}{CHAT_PATH}");
    // body 逐字照抄 Python `_daily_chat_upstream`（:1583-1589）。
    let payload = json!({
        "model": DAILY_CHAT_MODEL,
        "messages": [{"role": "user", "content": "Hi"}],
        "stream": true,
        "max_tokens": 10,
        "reasoning_effort": "none",
    });
    let body = build_upstream_body(&payload, &BodyOptions::default());
    match client
        .post_with_timeout(&url, acc.chat_headers, &body, DESKTOP_TIMEOUT)
        .await
    {
        Ok(resp) if resp.status().is_success() => DesktopLeg::Ok,
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let note = format!("HTTP {}: {}", status.as_u16(), truncate(&text, 100));
            if is_channel_blocked(&text) {
                DesktopLeg::ChannelBlocked(note)
            } else {
                DesktopLeg::Failed(note)
            }
        }
        Err(e) => DesktopLeg::Failed(describe_error(&e)),
    }
}

// ---------------------------------------------------------------------------
// 网页腿
// ---------------------------------------------------------------------------

/// 网页腿：建会话 → 取沙箱 → ACP 跑完这一轮。
///
/// 返回形状对齐 Python `_daily_chat_web_upstream`：
/// 成功 `{"ok": true, "status": "completed", "chunks": n, "elapsed_ms": n,
/// "conversation": id, "msg": "网页通道会话跑完：…"}`；失败 `{"ok": false,
/// "error": …}`（尽量带 `conversation`）。
///
/// **不记活动历史**：Python 的 `daily_chat_web(prompt, trigger)` 里 trigger
/// 只由**直接调用**它的那一层给（面板路由给 `manual`）；`daily_chat` 内部
/// 调它时传 `trigger=None`（那一步属于同一次尝试，由外层记一行）。Rust 侧
/// 的对应入口是 [`daily_chat_web_recorded`]（带用量目录 ⇒ 能写历史）。
pub async fn daily_chat_web(
    client: &UpstreamClient,
    cfg: &DailyChatConfig,
    acc: &DailyChatAccount<'_>,
    prompt: Option<&str>,
) -> Value {
    daily_chat_web_impl(client, cfg, acc, prompt, None).await
}

/// 网页腿 + **记一行活动历史**（Python `wb_accounts.daily_chat_web` 的
/// `record_attempt` 调用点，:1478）。
///
/// `usage_dir` 是活动历史文件所在目录（`usage/`）——历史写在用量目录里，
/// 与 Python 的 `wb_activity.history_path()` 同一个文件。
pub async fn daily_chat_web_recorded(
    client: &UpstreamClient,
    cfg: &DailyChatConfig,
    acc: &DailyChatAccount<'_>,
    prompt: Option<&str>,
    usage_dir: &Path,
) -> Value {
    daily_chat_web_impl(client, cfg, acc, prompt, Some(usage_dir)).await
}

/// 网页腿的实现；`history` 非空 ⇒ 跑完记一行活动历史。
async fn daily_chat_web_impl(
    client: &UpstreamClient,
    cfg: &DailyChatConfig,
    acc: &DailyChatAccount<'_>,
    prompt: Option<&str>,
    history: Option<&Path>,
) -> Value {
    let result = daily_chat_web_raw(client, cfg, acc, prompt).await;
    record_daily_chat(history, acc, "", &result);
    result
}

/// 网页腿本体（不记活动历史）。
async fn daily_chat_web_raw(
    client: &UpstreamClient,
    cfg: &DailyChatConfig,
    acc: &DailyChatAccount<'_>,
    prompt: Option<&str>,
) -> Value {
    if acc.realm != "intl" {
        return json!({"ok": false, "error": "web daily chat is only for international accounts"});
    }
    let web_base = cfg.resolved_web_base();
    let headers = web_headers(acc.uid, acc.token, &web_base);
    let body = json!({
        "prompt": prompt.unwrap_or(DAILY_CHAT_WEB_PROMPT),
        "model": DAILY_CHAT_MODEL,
        // 网页端建会话时固定带上这两项（抓包所得），保持请求形态一致。
        "conversationOrigin": "workbuddy-app",
        "plugins": [{"name": "weixinpay", "marketplace": "codebuddy-builtin"}],
    });
    let url = format!("{web_base}{WEB_CONVERSATIONS_PATH}");

    let resp = match client
        .post_with_timeout(&url, &headers, &body, WEB_TIMEOUT)
        .await
    {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": describe_error(&e)}),
    };
    if !resp.status().is_success() {
        // 429/6004 是「这个模型的窗口额度用满」，不是普通网络失败：Python 记成
        // cap（`_note_daily_chat_cap`）。Rust 侧的 cap 冷却接线属 M5，此处只保留
        // 错误信息（错误体截断到 120 字符，对齐 Python）。
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return json!({
            "ok": false,
            "error": format!("HTTP {}: {}", status.as_u16(), truncate(&text, 120)),
        });
    }
    let text = resp.text().await.unwrap_or_default();
    let payload: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return json!({"ok": false, "error": "unexpected response"}),
    };
    if !payload.is_object() {
        return json!({"ok": false, "error": "unexpected response"});
    }
    // Python：`payload.get("code") not in (0, None)` 判失败（`False == 0` 在
    // Python 里也成立，这里一并对齐）。
    let code_ok = match payload.get("code") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => !*b,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(_) => false,
    };
    // Python `(payload.get("data") or {}).get("id")`：**不限定类型**——上游可能
    // 给字符串，也可能给数字（URL 里再 `quote(str(conversation))`）。Rust 侧
    // 以前只认字符串，数字 id 会被当成「没有 id」，整条腿直接失败。
    let conversation_value = payload.get("data").and_then(|d| d.get("id")).cloned();
    let conversation_ok = matches!(&conversation_value, Some(v) if py_truthy(v));
    let conversation = conversation_value
        .as_ref()
        .map(|v| py_str(Some(v)))
        .unwrap_or_default();
    // Python 把**原值**塞回结果（`result["conversation"] = conversation`），
    // 上游给数字就是数字；URL 里才用 `str()` 形式。
    let conversation_raw = conversation_value.clone().unwrap_or(Value::Null);
    if !code_ok || !conversation_ok {
        return json!({
            "ok": false,
            "error": format!(
                "code={} msg={}",
                py_str(payload.get("code")),
                py_str(payload.get("msg"))
            ),
        });
    }

    // 建完只是排队：接上沙箱、请求这一轮，它才会真的跑起来（issue #90）。
    let session_url = format!(
        "{web_base}{WEB_CONVERSATIONS_PATH}{}/session",
        percent_encode(&conversation)
    );
    let session_text = match client
        .get_with_timeout(&session_url, &headers, WEB_TIMEOUT)
        .await
    {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            return json!({
                "ok": false,
                "conversation": conversation_raw.clone(),
                "error": format!(
                    "session 查询失败: HTTP {}: {}",
                    status.as_u16(),
                    truncate(&text, 120)
                ),
            });
        }
        Err(e) => {
            return json!({
                "ok": false,
                "conversation": conversation_raw.clone(),
                "error": format!("session 查询失败: {}", describe_error(&e)),
            });
        }
    };
    let session_doc: Value = serde_json::from_str(&session_text).unwrap_or(Value::Null);
    let data = session_doc.get("data").cloned().unwrap_or(Value::Null);
    let link = first_str(&data, &["link", "endpoint"])
        .unwrap_or("")
        .to_string();
    let sandbox_token = data
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_id = first_str(&data, &["sessionId", "session_id"])
        .map(str::to_string)
        .unwrap_or_else(|| conversation.clone());
    let cwd = data
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("/workspace")
        .to_string();
    if link.is_empty() || sandbox_token.is_empty() {
        return json!({
            "ok": false,
            "conversation": conversation_raw.clone(),
            "error": "沙箱未就绪（没有 link/token）",
        });
    }

    let turn = acp::run_turn(
        client.clone(),
        &link,
        &sandbox_token,
        &session_id,
        &cwd,
        prompt.unwrap_or(DAILY_CHAT_WEB_PROMPT),
        WEB_USER_AGENT,
        status_poller(client, &web_base, &conversation, acc.uid, acc.token),
        cfg.web_turn_timeout,
        cfg.web_poll_interval,
    )
    .await;

    let mut result = turn.to_json();
    result["conversation"] = conversation_raw.clone();
    if result.get("ok").and_then(Value::as_bool) == Some(true) {
        let chunks = result.get("chunks").and_then(Value::as_i64).unwrap_or(0);
        let elapsed = result
            .get("elapsed_ms")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        result["msg"] = json!(format!("网页通道会话跑完：{chunks} 段输出，{elapsed} ms"));
    }
    result
}

/// 造一条「查这条会话现在什么状态」的轮询回调（ACP 的 `poll_status`）。
///
/// Python 用同步 urllib GET；Rust 的 HTTP 是异步的，`wb_gateway::acp::PollStatus`
/// 用 boxed future 表达同一条语义。
fn status_poller(
    client: &UpstreamClient,
    web_base: &str,
    conversation: &str,
    uid: &str,
    token: &str,
) -> PollStatus {
    let client = client.clone();
    let web_base = web_base.to_string();
    let conversation = conversation.to_string();
    let uid = uid.to_string();
    let token = token.to_string();
    Box::new(move |_session_id: &str| {
        let client = client.clone();
        let web_base = web_base.clone();
        let conversation = conversation.clone();
        let uid = uid.clone();
        let token = token.clone();
        Box::pin(async move {
            let url = format!(
                "{web_base}{WEB_CONVERSATIONS_PATH}{}",
                percent_encode(&conversation)
            );
            let headers = web_headers(&uid, &token, &web_base);
            let status = match client.get_with_timeout(&url, &headers, WEB_TIMEOUT).await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|doc| {
                            doc.get("data")
                                .and_then(|d| d.get("status"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_default()
                }
                _ => String::new(),
            };
            json!({ "status": status })
        })
    })
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

/// 写 `lastDailyChat = clock.local_now()`。
///
/// 失败只回 `Some(原因)`，绝不上抛——调用方不得因此翻转「今天已完成」的结论。
fn persist_last_daily_chat(ctx: &DailyChatCtx<'_>) -> Option<String> {
    let fields = IdempotentFields {
        last_daily_chat: Some(ctx.clock.local_now()),
        ..IdempotentFields::default()
    };
    write_idempotent_fields(ctx.dir, ctx.account_name, &fields)
        .err()
        .map(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 活动历史（关卡 4.20 线 B）
// ---------------------------------------------------------------------------

/// 记一次每日活跃尝试（Python `wb_activity.record_attempt`）。
///
/// `history` 为 `None` ⇒ 不记（Python 的 `trigger=None`：那一步属于同一次
/// 尝试，由外层记一行）。
///
/// **写不进去只打日志**：活动历史是运维元数据，写不进去不该把一次成功的
/// 对话变成失败（Python `record()` 绝不抛异常，同款）。
fn record_daily_chat(
    history: Option<&Path>,
    acc: &DailyChatAccount<'_>,
    nickname: &str,
    result: &Value,
) {
    let Some(usage_dir) = history else {
        return;
    };
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let message = result
        .get("msg")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| result.get("error").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| {
            if ok {
                "成功".to_string()
            } else {
                "失败".to_string()
            }
        });
    activity::record_attempt(
        usage_dir,
        acc.uid,
        nickname,
        acc.realm,
        activity::TASK_DAILY_CHAT,
        activity::TRIGGER_UNKNOWN,
        ok,
        &message,
    );
}

// ---------------------------------------------------------------------------
// 编排
// ---------------------------------------------------------------------------

/// 国际版每日活跃对话。
///
/// `web` 为 `None` 时按 `settings.json` 的 `daily_chat_web` 决定（默认开）；
/// `Some(true/false)` 显式指定。
///
/// 关键语义（对齐 Python `_daily_chat_upstream`）：
/// - 桌面腿 2xx → 成功；随后按开关跑网页腿
/// - 桌面腿 4xx/5xx 且 body 含 `"code": 11128` → **降级**：网页腿单独跑
/// - 桌面腿其他失败 → 直接失败
/// - `lastDailyChat` **只在网页腿成功（或没开网页腿）时写**
///
/// **活动历史**：整次尝试记一行（Python `wb_accounts.py:1570`）；网页腿那一步
/// 不单独记（Python 内部调用传 `trigger=None`）。非国际版账号**不记**
/// （Python 的早退在 `record_attempt` 之前）。
pub async fn daily_chat(
    client: &UpstreamClient,
    acc: &DailyChatAccount<'_>,
    cfg: &DailyChatConfig,
    ctx: &DailyChatCtx<'_>,
    web: Option<bool>,
) -> DailyChatOutcome {
    if acc.realm != "intl" {
        return DailyChatOutcome::failed("daily chat is only for international accounts");
    }
    // Python：`web = bool(self.path) and wb_settings.daily_chat_web(dirname)`。
    // 账号没有落盘路径（测试注入的空 credential_file）时不开网页腿。
    let web_enabled = match web {
        Some(v) => v,
        None => !ctx.account_name.is_empty() && settings_daily_chat_web(ctx.dir),
    };
    let chat_base = cfg.resolved_chat_base(acc.realm, acc.product);

    let outcome = match run_desktop_leg(client, acc, &chat_base).await {
        DesktopLeg::Ok => {
            let mut body = Map::new();
            body.insert("ok".to_string(), json!(true));
            body.insert("msg".to_string(), json!("每日活跃对话成功完成"));
            let mut web_ok = true;
            if web_enabled {
                let res = daily_chat_web(client, cfg, acc, None).await;
                web_ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let msg = if web_ok {
                    format!(
                        "每日活跃对话成功完成（网页通道 {}：{} 段输出，{} ms）",
                        res.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed"),
                        res.get("chunks").and_then(Value::as_i64).unwrap_or(0),
                        res.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0),
                    )
                } else {
                    format!(
                        "每日活跃对话成功完成（网页通道失败：{}）",
                        res.get("error").and_then(Value::as_str).unwrap_or("")
                    )
                };
                body.insert("msg".to_string(), json!(msg));
                body.insert("web".to_string(), res);
            }
            let persist_error = if web_ok {
                persist_last_daily_chat(ctx)
            } else {
                None
            };
            DailyChatOutcome {
                body: Value::Object(body),
                persist_error,
            }
        }
        DesktopLeg::ChannelBlocked(note) => {
            // 没开网页腿就真的没有别的路可走了，照旧算失败。
            if !web_enabled {
                DailyChatOutcome::failed(note)
            } else {
                let res = daily_chat_web(client, cfg, acc, None).await;
                let web_ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let mut body = Map::new();
                body.insert("ok".to_string(), json!(web_ok));
                body.insert("msg".to_string(), json!(""));
                body.insert("web".to_string(), res.clone());
                if web_ok {
                    let msg = format!(
                        "桌面腿被渠道校验拦截（{}），网页通道 {}：{} 段输出，{} ms",
                        note,
                        res.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed"),
                        res.get("chunks").and_then(Value::as_i64).unwrap_or(0),
                        res.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0),
                    );
                    body.insert("msg".to_string(), json!(msg));
                    let persist_error = persist_last_daily_chat(ctx);
                    DailyChatOutcome {
                        body: Value::Object(body),
                        persist_error,
                    }
                } else {
                    // 两条腿都没成：今天不算做过，后续巡检继续重试。
                    let msg = format!(
                        "桌面腿被渠道校验拦截（{}），网页通道也失败：{}",
                        note,
                        res.get("error").and_then(Value::as_str).unwrap_or("")
                    );
                    body.insert("msg".to_string(), json!(msg.clone()));
                    body.insert("error".to_string(), json!(msg));
                    DailyChatOutcome {
                        body: Value::Object(body),
                        persist_error: None,
                    }
                }
            }
        }
        DesktopLeg::Failed(err) => DailyChatOutcome::failed(err),
    };

    // 整次尝试记一行（Python `wb_accounts.py:1570` 的 record_attempt）。
    let usage_dir = crate::api_routes::usage_dir_from(ctx.dir);
    let nickname = activity::account_nickname(ctx.dir, ctx.account_name);
    record_daily_chat(Some(&usage_dir), acc, &nickname, &outcome.body);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_blocked_matches_python_regex() {
        // Python r'"code"\s*:\s*"?11128"?'
        assert!(is_channel_blocked(r#"{"code": 11128, "msg": "渠道校验"}"#));
        assert!(is_channel_blocked(r#"{"code":11128}"#));
        assert!(is_channel_blocked(r#"{"code": "11128"}"#));
        assert!(is_channel_blocked(r#"{"code"  :  "11128"}"#));
        assert!(!is_channel_blocked(r#"{"code": 11127}"#));
        // Python 的正则**未锚定**：`111280` 里也含 `11128`，`re.search` 会命中。
        // 这里保持与 Python 一致（宁可多降级一次，也不把渠道拦截误判成普通失败）。
        assert!(is_channel_blocked(r#"{"code": 111280}"#));
        assert!(!is_channel_blocked("no code here"));
    }

    #[test]
    fn percent_encode_matches_python_quote() {
        assert_eq!(percent_encode("abc-123_x.y"), "abc-123_x.y");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a/b"), "a/b");
        assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("中文中文", 2), "中文");
        assert_eq!(truncate("abcdef", 3), "abc");
    }

    #[test]
    fn web_headers_match_python() {
        let h = web_headers("uid-1", "tok", WEB_ORIGIN);
        assert_eq!(h["Authorization"], "Bearer tok");
        assert_eq!(h["X-User-Id"], "uid-1");
        assert_eq!(h["Content-Type"], "application/json");
        assert_eq!(h["Accept"], "application/json, text/plain, */*");
        assert_eq!(h["Origin"], "https://www.workbuddy.ai");
        assert_eq!(h["Referer"], "https://www.workbuddy.ai/app");
        assert_eq!(h["User-Agent"], WEB_USER_AGENT);
    }

    #[test]
    fn production_endpoints_resolve_by_account_identity() {
        let cfg = DailyChatConfig::default();
        // 国际版：桌面腿与网页腿都指向 www.workbuddy.ai（不是国内版常量）。
        assert_eq!(
            cfg.resolved_chat_base("intl", "workbuddy"),
            "https://www.workbuddy.ai"
        );
        assert_eq!(cfg.resolved_web_base(), "https://www.workbuddy.ai");
        // 国内版账号的桌面腿走 copilot.tencent.com（证明是按身份解析，不是写死）。
        assert_eq!(
            cfg.resolved_chat_base("cn", "workbuddy"),
            "https://copilot.tencent.com"
        );
    }

    #[test]
    fn settings_toggle_defaults_on() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        // 文件不存在 → 默认开
        assert!(settings_daily_chat_web(dir.path()));
        // 键缺失 → 默认开
        std::fs::write(dir.path().join("settings.json"), "{}").expect("write");
        assert!(settings_daily_chat_web(dir.path()));
        // null → 默认开
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"daily_chat_web": null}"#,
        )
        .expect("write");
        assert!(settings_daily_chat_web(dir.path()));
        // 显式 false → 关
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"daily_chat_web": false}"#,
        )
        .expect("write");
        assert!(!settings_daily_chat_web(dir.path()));
        // 显式 true → 开
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"daily_chat_web": true}"#,
        )
        .expect("write");
        assert!(settings_daily_chat_web(dir.path()));
        // 非布尔（字符串）→ 关（对齐 Python 的 `value is True`）
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"daily_chat_web": "true"}"#,
        )
        .expect("write");
        assert!(!settings_daily_chat_web(dir.path()));
    }

    #[test]
    fn desktop_body_carries_python_fields() {
        // 桌面腿 body 逐字对齐 Python `_daily_chat_upstream`。
        let payload = json!({
            "model": DAILY_CHAT_MODEL,
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": true,
            "max_tokens": 10,
            "reasoning_effort": "none",
        });
        let body = build_upstream_body(&payload, &BodyOptions::default());
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some(DAILY_CHAT_MODEL)
        );
        assert_eq!(body.get("max_tokens").and_then(Value::as_i64), Some(10));
        assert_eq!(
            body.get("reasoning_effort").and_then(Value::as_str),
            Some("none")
        );
        // build_upstream_body 强制流式
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
    }
}
