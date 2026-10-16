//! 打印型工具调用恢复（leaked/DSML），移植自 wb_proxy.py:
//! - :11201-11227 DSML 常量与 parse_dsml_tool_calls（全宽 DSML 形式）
//! - :11251-11376 LEAK_* 常量 / parse_leaked_tool_calls（parameter 驱动的混合形式）
//! - :11408-11471 could_be_leaked_call / LeakedToolCallStreamFilter（流式缓冲）
//! - :11379-11405 repair_leaked_tool_calls（聚合端）
//!
//! 两种拼写互斥补充：模型打印调用而不发结构化 tool_calls 时，客户端会看到
//! 纯文本、找不到调用、回合空转。Rust 实现对齐 Python 语义；arguments 的
//! JSON 序列化用紧凑分隔（Python 带 ", " 空格，客户端反序列化无差异，
//! M6 shadow 对照记录差异）。

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;

/// `call_` + 32 位随机 hex，对齐 Python `_new_id("call_")`。
pub(crate) fn new_call_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("os rng must be available");
    format!(
        "call_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

// ---- DSML 全宽形式（wb_proxy.py:11201-11204）----
// TAG_START = r"<[^>]*DSML[^>]*"
static DSML_CALLS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<[^>]*DSML[^>]*calls>(.*?)</[^>]*DSML[^>]*calls>").unwrap());
static DSML_INVOKE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<[^>]*DSML[^>]*invoke\s+name=["']([^"']+)["']>(.*?)</[^>]*invoke>"#).unwrap()
});
static DSML_PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<[^>]*DSML[^>]*parameter\s+name=["']([^"']+)["'][^>]*>(.*?)</[^>]*parameter>"#,
    )
    .unwrap()
});

/// parse_dsml_tool_calls（wb_proxy.py:11205-11227）：全宽 DSML 块。
/// 返回 (Some(calls), clean_text) 或 (None, 原文本)。
pub fn parse_dsml_tool_calls(text: &str) -> (Option<Vec<Value>>, String) {
    if text.is_empty() || !text.contains("DSML") {
        return (None, text.to_string());
    }
    let Some(cap) = DSML_CALLS_RE.captures(text) else {
        return (None, text.to_string());
    };
    let calls_block = cap[1].to_string();
    let mut tool_calls = Vec::new();
    for inv in DSML_INVOKE_RE.captures_iter(&calls_block) {
        let func_name = inv[1].to_string();
        let params_block = inv[2].to_string();
        let mut params = serde_json::Map::new();
        for p in DSML_PARAM_RE.captures_iter(&params_block) {
            params.insert(p[1].to_string(), Value::String(p[2].trim().to_string()));
        }
        tool_calls.push(serde_json::json!({
            "id": new_call_id(),
            "name": func_name,
            "arguments": serde_json::to_string(&Value::Object(params)).unwrap_or_default(),
        }));
    }
    let start = cap.get(0).map(|m| m.start()).unwrap_or(0);
    let end = cap.get(0).map(|m| m.end()).unwrap_or(0);
    let clean = format!("{} {}", text[..start].trim(), text[end..].trim());
    (Some(tool_calls), clean.trim().to_string())
}

// ---- 混合打印形式（wb_proxy.py:11251-11274）----
static LEAK_PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<\s*(?:\|\s*DSML\s*\|\s*)?(?P<tag>parameter|param|arg|argument)\b(?P<attrs>[^>]*)>(?P<value>.*?)</\s*(?:\|\s*DSML\s*\|\s*)?(?:parameter|param|arg|argument)\s*>",
    )
    .unwrap()
});
static LEAK_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<\s*/?\s*(?:\|\s*DSML\s*\|\s*)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)(?P<attrs>[^>]*)>")
        .unwrap()
});
static LEAK_NAME_ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"name\s*=\s*["']([^"']*)["']"#).unwrap());
static NORMALIZE_BARS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"｜+\s*DSML\s*｜+").unwrap());

const LEAK_WRAPPER_TAGS: [&str; 4] = ["calls", "tool_calls", "function_calls", "function_call"];
const LEAK_PARAM_TAGS: [&str; 4] = ["parameter", "param", "arg", "argument"];

/// 流式过滤器必须按住文本不放的标签前缀（wb_proxy.py:11266-11274）。
const LEAK_TAG_PREFIXES: [&str; 28] = [
    "<run_code",
    "<run_shell",
    "<invoke",
    "<parameter",
    "<param",
    "<arg",
    "<argument",
    "<calls",
    "<tool_calls",
    "<function_calls",
    "<function_call",
    "<think",
    "<thinking",
    "</run_code",
    "</run_shell",
    "</invoke",
    "</parameter",
    "</param",
    "</arg",
    "</argument",
    "</calls",
    "</tool_calls",
    "</function_calls",
    "</function_call",
    "</think",
    "</thinking",
    "<|dsml|",
    "</|dsml|",
];

/// _normalize_dsml_bars（wb_proxy.py:11277-11279）：全宽 DSML 记号折回 ASCII。
fn normalize_dsml_bars(text: &str) -> String {
    NORMALIZE_BARS_RE.replace_all(text, "|DSML|").into_owned()
}

/// could_be_leaked_call（wb_proxy.py:11408-11416）：`tail`（从 `<` 起）是否
/// 仍可能长成打印型调用标记。
pub fn could_be_leaked_call(tail: &str) -> bool {
    let low = normalize_dsml_bars(tail).to_lowercase();
    if let Some(m) = LEAK_TAG_RE.captures(&low) {
        let whole = m.get(0).map(|g| g.as_str()).unwrap_or("");
        if LEAK_TAG_PREFIXES.iter().any(|p| whole.starts_with(p)) {
            return true;
        }
    }
    if low.chars().count() > 96 || low.contains('>') {
        return false;
    }
    LEAK_TAG_PREFIXES
        .iter()
        .any(|p| p.starts_with(&low) || low.starts_with(p))
}

/// _strip_spans（wb_proxy.py:11282-11294）：删去每段 [start, end) 再折叠空白。
fn strip_spans(text: &str, mut spans: Vec<(usize, usize)>) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_unstable();
    let mut kept = String::new();
    let mut last = 0usize;
    for (start, end) in spans {
        if start < last {
            continue;
        }
        kept.push_str(&text[last..start]);
        last = end;
    }
    kept.push_str(&text[last..]);
    kept.trim().to_string()
}

struct FoundParam {
    start: usize,
    name: String,
    value: String,
    span: (usize, usize),
}

/// parse_leaked_tool_calls（wb_proxy.py:11297-11376）：parameter 驱动的
/// 混合形式解析。返回 (Some(calls), clean_text) 或 (None, 原文本)。
pub fn parse_leaked_tool_calls(text: &str) -> (Option<Vec<Value>>, String) {
    if text.is_empty() || !text.contains('<') {
        return (None, text.to_string());
    }
    let original = text.to_string();
    let norm = normalize_dsml_bars(text);
    let mut found = Vec::new();
    for m in LEAK_PARAM_RE.captures_iter(&norm) {
        let attrs = m.name("attrs").map(|g| g.as_str()).unwrap_or("");
        let Some(attr) = LEAK_NAME_ATTR_RE.captures(attrs) else {
            continue;
        };
        let pname = attr.get(1).map(|g| g.as_str()).unwrap_or("");
        if pname.is_empty() {
            continue;
        }
        let span = (
            m.get(0).map(|g| g.start()).unwrap_or(0),
            m.get(0).map(|g| g.end()).unwrap_or(0),
        );
        found.push(FoundParam {
            start: span.0,
            name: pname.to_string(),
            value: m
                .name("value")
                .map(|g| g.as_str().trim())
                .unwrap_or("")
                .to_string(),
            span,
        });
    }
    if found.is_empty() {
        return (None, original);
    }
    let tags: Vec<Captures> = LEAK_TAG_RE.captures_iter(&norm).collect();

    #[derive(Clone)]
    struct Call {
        name: String,
        params: serde_json::Map<String, Value>,
        /// 拥有调用的开标签**字符位置**（Python `owner.start()`）。
        open: Option<usize>,
        spans: Vec<(usize, usize)>,
    }
    let mut calls: Vec<Call> = Vec::new();
    for p in &found {
        let mut owner: Option<usize> = None;
        for tag in tags.iter() {
            let tstart = tag.get(0).map(|g| g.start()).unwrap_or(0);
            if tstart >= p.start {
                break;
            }
            let whole = tag.get(0).map(|g| g.as_str()).unwrap_or("");
            if whole.trim_start().starts_with("</") {
                continue;
            }
            let tname = tag
                .name("name")
                .map(|g| g.as_str())
                .unwrap_or("")
                .to_lowercase();
            if LEAK_WRAPPER_TAGS.contains(&tname.as_str())
                || LEAK_PARAM_TAGS.contains(&tname.as_str())
            {
                continue;
            }
            owner = Some(tstart);
        }
        let mut name = String::new();
        if let Some(tstart) = owner {
            let owner_tag = tags
                .iter()
                .find(|t| t.get(0).map(|g| g.start()).unwrap_or(0) == tstart);
            if let Some(tag) = owner_tag {
                let attrs = tag.name("attrs").map(|g| g.as_str()).unwrap_or("");
                if let Some(attr) = LEAK_NAME_ATTR_RE.captures(attrs) {
                    name = attr
                        .get(1)
                        .map(|g| g.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                }
                if name.is_empty() {
                    name = tag
                        .name("name")
                        .map(|g| g.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        if name.is_empty() {
            continue;
        }
        let key = owner;
        if let Some(last_call) = calls.last_mut() {
            if last_call.open == key {
                last_call
                    .params
                    .insert(p.name.clone(), Value::String(p.value.clone()));
                last_call.spans.push(p.span);
                continue;
            }
        }
        let mut params = serde_json::Map::new();
        params.insert(p.name.clone(), Value::String(p.value.clone()));
        calls.push(Call {
            name: name.trim().to_string(),
            params,
            open: key,
            spans: vec![p.span],
        });
    }
    if calls.is_empty() {
        return (None, original);
    }
    let mut regions = Vec::new();
    for call in &calls {
        let first = call
            .open
            .unwrap_or_else(|| call.spans.iter().map(|(s, _)| *s).min().unwrap_or(0));
        let mut last = call.spans.iter().map(|(_, e)| *e).max().unwrap_or(0);
        // 吞掉最后一个参数后尾随的 closing tags
        loop {
            let mut nxt = last;
            while nxt < norm.len() && matches!(norm.as_bytes()[nxt], b' ' | b'\t' | b'\r' | b'\n') {
                nxt += 1;
            }
            match LEAK_TAG_RE.captures_at(&norm, nxt) {
                Some(m) => {
                    let whole = m.get(0).map(|g| g.as_str()).unwrap_or("");
                    if !whole.trim_start().starts_with("</") {
                        break;
                    }
                    last = m.get(0).map(|g| g.end()).unwrap_or(nxt);
                }
                None => break,
            }
        }
        regions.push((first, last));
    }
    let mut clean = strip_spans(&norm, regions);
    // Wrapper 与 region 未覆盖的杂散标签（<|DSML| calls>、 thinking...）也是标记
    let mut leftover = Vec::new();
    for m in LEAK_TAG_RE.captures_iter(&clean) {
        let whole = m.get(0).map(|g| g.as_str()).unwrap_or("");
        if normalize_dsml_bars(whole).to_uppercase().contains("DSML") {
            leftover.push((
                m.get(0).map(|g| g.start()).unwrap_or(0),
                m.get(0).map(|g| g.end()).unwrap_or(0),
            ));
        } else {
            let tname = m
                .name("name")
                .map(|g| g.as_str())
                .unwrap_or("")
                .to_lowercase();
            if LEAK_WRAPPER_TAGS.contains(&tname.as_str()) {
                leftover.push((
                    m.get(0).map(|g| g.start()).unwrap_or(0),
                    m.get(0).map(|g| g.end()).unwrap_or(0),
                ));
            }
        }
    }
    clean = strip_spans(&clean, leftover);
    let out: Vec<Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": new_call_id(),
                "name": c.name,
                "arguments": serde_json::to_string(&Value::Object(c.params.clone()))
                    .unwrap_or_default(),
            })
        })
        .collect();
    (Some(out), clean)
}

/// repair_leaked_tool_calls（wb_proxy.py:11379-11405）：把打印在 content 里的
/// 调用搬进 tool_calls。仅当无结构化 tool_calls 时触发；返回是否改动。
pub fn repair_leaked_tool_calls(message: &mut Value) -> bool {
    let Some(obj) = message.as_object_mut() else {
        return false;
    };
    if let Some(Value::Array(existing)) = obj.get("tool_calls") {
        if !existing.is_empty() {
            return false;
        }
    }
    let Some(Value::String(content)) = obj.get("content") else {
        return false;
    };
    if !content.contains('<') {
        return false;
    }
    let (mut calls, clean) = parse_leaked_tool_calls(content);
    if calls.is_none() {
        let (c, clean_text) = parse_dsml_tool_calls(content);
        calls = c;
        obj.insert("content".into(), Value::String(clean_text));
    } else {
        obj.insert("content".into(), Value::String(clean));
    }
    let Some(calls) = calls else {
        return false;
    };
    obj.insert(
        "tool_calls".into(),
        Value::Array(
            calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.get("id").cloned().unwrap_or_default(),
                        "type": "function",
                        "function": {
                            "name": c.get("name").cloned().unwrap_or_default(),
                            "arguments": c.get("arguments").cloned().unwrap_or_default(),
                        }
                    })
                })
                .collect(),
        ),
    );
    true
}

/// 流式过滤器产物。
#[derive(Debug, Clone, PartialEq)]
pub enum FeedItem {
    Text(String),
    Calls(Vec<Value>),
}

/// LeakedToolCallStreamFilter（wb_proxy.py:11419-11471）：按住可能打印调用
/// 的文本，确定是散文立即放出，流末残留解析成调用。
pub struct LeakedToolCallStreamFilter {
    buf: String,
}

impl Default for LeakedToolCallStreamFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl LeakedToolCallStreamFilter {
    pub const MAX_HOLD: usize = 262_144;

    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    fn release_all(&mut self) -> Vec<FeedItem> {
        let out = std::mem::take(&mut self.buf);
        if out.is_empty() {
            vec![]
        } else {
            vec![FeedItem::Text(out)]
        }
    }

    /// feed（wb_proxy.py:11439-11456）：追加文本；能确定是散文立即放行，
    /// 否则继续缓冲（返回空）。buf 超过 MAX_HOLD 强制放行。
    pub fn feed(&mut self, text: &str) -> Vec<FeedItem> {
        if text.is_empty() {
            return vec![];
        }
        self.buf.push_str(text);
        let bytes = self.buf.as_bytes();
        let mut i = 0usize;
        loop {
            // 找下一个 '<'
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            if i >= bytes.len() {
                break; // 无 '<' → for/else → release_all
            }
            if could_be_leaked_call(&self.buf[i..]) {
                if i > 0 {
                    let head = self.buf[..i].to_string();
                    self.buf = self.buf[i..].to_string();
                    return vec![FeedItem::Text(head)];
                }
                return vec![]; // i==0：首个字符即可能调用开头，继续缓冲
            }
            i += 1;
        }
        self.release_all()
    }

    /// flush（wb_proxy.py:11458-11471）：解析残留；无调用则原文放出。
    pub fn flush(&mut self) -> Vec<FeedItem> {
        if self.buf.is_empty() {
            return vec![];
        }
        let buf = std::mem::take(&mut self.buf);
        let (mut calls, clean) = parse_leaked_tool_calls(&buf);
        if calls.is_none() {
            let (c, clean_text) = parse_dsml_tool_calls(&buf);
            calls = c;
            if !clean_text.is_empty() {
                return vec![FeedItem::Text(clean_text)];
            }
        }
        let Some(calls) = calls else {
            return vec![FeedItem::Text(buf)];
        };
        let mut out = Vec::new();
        if !clean.is_empty() {
            out.push(FeedItem::Text(clean));
        }
        out.push(FeedItem::Calls(calls));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 参照值来自真实 Python（fetch_leaked_refs.py，2026-10-16）----

    const HYBRID: &str = "<run_code><parameter name=\"code\">print(1)</parameter>\
        </run_code></invoke></|DSML| parameter></|DSML| invoke></|DSML| calls>";

    #[test]
    fn parse_leaked_hybrid_form() {
        let (calls, clean) = parse_leaked_tool_calls(HYBRID);
        let calls = calls.expect("hybrid must parse");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "run_code");
        assert_eq!(calls[0]["arguments"], r#"{"code":"print(1)"}"#);
        assert!(calls[0]["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(clean, "");
    }

    #[test]
    fn parse_leaked_rejects_dsml_and_prose() {
        let dsml = "<|DSML|| calls><|DSML|| invoke name=\"run_code\">\
            <|DSML|| parameter name=\"code\">ls</|DSML|| parameter>\
            </|DSML|| invoke></|DSML|| calls>";
        let (calls, clean) = parse_leaked_tool_calls(dsml);
        assert!(calls.is_none());
        assert_eq!(clean, dsml); // 原样
        assert_eq!(parse_leaked_tool_calls("plain prose").0, None);
        assert_eq!(
            parse_leaked_tool_calls("use the < operator in your code").0,
            None
        );
        assert_eq!(parse_leaked_tool_calls("").0, None);
    }

    #[test]
    fn parse_dsml_full_form() {
        let dsml = "<|DSML|| calls><|DSML|| invoke name=\"run_code\">\
            <|DSML|| parameter name=\"code\">ls</|DSML|| parameter>\
            </|DSML|| invoke></|DSML|| calls>";
        let (calls, clean) = parse_dsml_tool_calls(dsml);
        let calls = calls.expect("dsml must parse");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "run_code");
        assert_eq!(calls[0]["arguments"], r#"{"code":"ls"}"#);
        assert_eq!(clean, "");
    }

    #[test]
    fn parse_dsml_rejects_hybrid_and_prose() {
        let (calls, _) = parse_dsml_tool_calls(HYBRID);
        assert!(calls.is_none());
        assert_eq!(parse_dsml_tool_calls("plain prose").0, None);
    }

    #[test]
    fn stream_filter_buffers_and_flushes() {
        let mut f = LeakedToolCallStreamFilter::new();
        let out0 = f.feed("Let me check the repo state");
        assert_eq!(
            out0,
            vec![FeedItem::Text("Let me check the repo state".into())]
        );
        let out1 = f.feed(
            " by running: <run_code><parameter name=\"code\">git status</parameter></run_code>",
        );
        assert_eq!(out1, vec![FeedItem::Text(" by running: ".into())]);
        // closing tags 仍是潜在调用 → 继续缓冲
        assert_eq!(
            f.feed("</invoke></|DSML| parameter></|DSML| invoke></|DSML| calls>"),
            vec![]
        );
        assert_eq!(f.feed(" That should do it."), vec![]);
        let flushed = f.flush();
        assert_eq!(flushed.len(), 2);
        match &flushed[0] {
            FeedItem::Text(t) => assert_eq!(t, "That should do it."),
            _ => panic!("first flush item must be text"),
        }
        match &flushed[1] {
            FeedItem::Calls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0]["name"], "run_code");
                assert_eq!(calls[0]["arguments"], r#"{"code":"git status"}"#);
            }
            _ => panic!("second flush item must be calls"),
        }
    }

    #[test]
    fn stream_filter_releases_plain_prose_immediately() {
        let mut f = LeakedToolCallStreamFilter::new();
        assert_eq!(
            f.feed("the weather today is fine and sunny"),
            vec![FeedItem::Text("the weather today is fine and sunny".into())]
        );
        // 用 `<` 运算符的普通文本不是调用 → 同样立即放行
        assert_eq!(
            f.feed("use the < operator in your code"),
            vec![FeedItem::Text("use the < operator in your code".into())]
        );
        assert_eq!(f.flush(), vec![]);
    }

    #[test]
    fn repair_moves_printed_call_into_tool_calls() {
        let mut msg = serde_json::json!({
            "role": "assistant",
            "content": "Let me check: <run_code><parameter name=\"code\">git status</parameter></run_code></invoke>"
        });
        assert!(repair_leaked_tool_calls(&mut msg));
        assert_eq!(msg["content"], "Let me check:");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "run_code");
        // 纯散文不动
        let mut noop = serde_json::json!({"role": "assistant", "content": "plain prose"});
        assert!(!repair_leaked_tool_calls(&mut noop));
        // 已有结构化 tool_calls 不覆盖
        let mut existing = serde_json::json!({
            "role": "assistant", "content": "<run_code>x</run_code>",
            "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]
        });
        assert!(!repair_leaked_tool_calls(&mut existing));
    }
}
