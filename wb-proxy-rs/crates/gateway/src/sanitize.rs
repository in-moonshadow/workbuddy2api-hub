//! 消息内容指纹清洗，移植自 wb_proxy.py sanitize_text（:10728-10747）、
//! sanitize_content（:10748-10761）、sanitize_tool_calls（:10762-10777）与
//! sanitize_messages（:10778-10792）。
//!
//! 目的：代理/客户端注入的系统提示词（Claude Code / Codex / OMO 等）带明显
//! 指纹（billing header、cc_ 参数、固定短语），上游风控会按指纹关联流量；
//! 命中指纹的文本按固定链变形，无指纹时**原样返回且不 strip**（快路径）。

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// 指纹字面量表（wb_proxy.py:10663-10671）：任一 `in` 命中即进入变形链。
const SANITIZE_FEATURES: [&str; 7] = [
    "x-anthropic-billing-header",
    "cc_entrypoint=",
    "You are Claude Code",
    "Main branch (",
    "You are a coding agent running in the Codex CLI",
    "github.com/anthropics/",
    "11128",
];

/// 成对重写（wb_proxy.py:10672-10682），按序 replace。
const SANITIZE_REWRITES: [(&str, &str); 5] = [
    (
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    (
        "You are a coding agent running in the Codex CLI, a terminal-based coding assistant.",
        "You are a coding agent running in the Codex CLI tool, a terminal-based coding assistant.",
    ),
    (
        "To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
        "To provide feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
    ),
    ("11128", "11-128"),
];

const OMO_FULL: &str = "Sisyphus-Junior - Focused executor from OhMyOpenCode";

/// ③ 剥带值的 billing header 段：`x-anthropic-billing-header: <值>;`（值到分号/行尾）。
static BILLING_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"x-anthropic-billing-header:[^;\r\n]*;?\s*").unwrap());

/// ④ 剥 `cc_<key>=<值>;` 段，循环直到文本不再含 `cc_`。
static CC_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bcc_[a-z0-9_]+=[^;\r\n]*;?\s*").unwrap());

/// 裸 billing header 词（大小写不敏感），用于指纹判定。
static BARE_BILLING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bx-anthropic-billing-header\b").unwrap());

/// Unicode 折叠（İ→i、ı→i、ſ→s）后小写，对齐 Python 的 ufold 语义。
fn fold_lower(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'İ' | 'ı' => 'i',
            'ſ' => 's',
            _ => c.to_lowercase().next().unwrap_or(c),
        })
        .collect()
}

/// 指纹判定：字面量表任一命中，或裸 billing 词正则命中，或 OMO 全指纹折叠命中。
fn has_fingerprint(text: &str) -> bool {
    if SANITIZE_FEATURES.iter().any(|f| text.contains(f)) {
        return true;
    }
    if BARE_BILLING_RE.is_match(text) {
        return true;
    }
    fold_lower(text).contains(&fold_lower(OMO_FULL))
}

/// 变形链：OMO 剥尾 → REWRITES 替换 → 剥 billing 段 → 循环剥 cc_ 段 →
/// 裸词换名 → strip。**仅指纹命中才走此链**。
pub fn sanitize_text(text: &str) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut t = text.to_string();
    if fold_lower(&t).contains(&fold_lower(OMO_FULL)) {
        t = t.replace(" from OhMyOpenCode", "");
    }
    for (old, new) in SANITIZE_REWRITES {
        t = t.replace(old, new);
    }
    t = BILLING_PATTERN.replace_all(&t, "").into_owned();
    while t.contains("cc_") {
        t = CC_PATTERN.replace_all(&t, "").into_owned();
    }
    t = t.replace("x-anthropic-billing-header", "x-anthropic-billing-hdr");
    t.trim().to_string()
}

/// content 清洗：str → sanitize_text；list → 仅 text 段复制清洗（image 等原样）；
/// 其余类型原样。
pub fn sanitize_content(content: &Value) -> Value {
    match content {
        Value::String(s) => Value::String(sanitize_text(s)),
        Value::Array(items) => {
            let out: Vec<Value> = items
                .iter()
                .map(|item| {
                    if let Some(obj) = item.as_object() {
                        if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(Value::String(text)) = obj.get("text") {
                                let mut clone = obj.clone();
                                clone.insert("text".into(), Value::String(sanitize_text(text)));
                                return Value::Object(clone);
                            }
                        }
                    }
                    item.clone()
                })
                .collect();
            Value::Array(out)
        }
        other => other.clone(),
    }
}

/// tool_calls 清洗：仅 function.arguments 为 str 的 dict 项复制清洗。
pub fn sanitize_tool_calls(tool_calls: &Value) -> Value {
    let Value::Array(calls) = tool_calls else {
        return tool_calls.clone();
    };
    Value::Array(
        calls
            .iter()
            .map(|call| {
                let Some(obj) = call.as_object() else {
                    return call.clone();
                };
                let Some(fn_obj) = obj.get("function").and_then(|f| f.as_object()).cloned() else {
                    return call.clone();
                };
                let Some(Value::String(arguments)) = fn_obj.get("arguments").cloned() else {
                    return call.clone();
                };
                let mut new_fn = fn_obj;
                new_fn.insert("arguments".into(), Value::String(sanitize_text(&arguments)));
                let mut clone = obj.clone();
                clone.insert("function".into(), Value::Object(new_fn));
                Value::Object(clone)
            })
            .collect(),
    )
}

/// 逐条浅拷贝清洗 content / reasoning_content（仅 str）/ tool_calls；
/// 非 dict 原样透传。恒返回新 Vec。
pub fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m.as_object() {
            Some(obj) => {
                let mut clone = obj.clone();
                if let Some(content) = obj.get("content") {
                    clone.insert("content".into(), sanitize_content(content));
                }
                if let Some(Value::String(rc)) = obj.get("reasoning_content") {
                    clone.insert("reasoning_content".into(), Value::String(sanitize_text(rc)));
                }
                if let Some(tool_calls) = obj.get("tool_calls") {
                    clone.insert("tool_calls".into(), sanitize_tool_calls(tool_calls));
                }
                Value::Object(clone)
            }
            None => m.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参照值取自真实 Python（fetch_sanitize_refs.py 实测，2026-10-16）：
    /// 断言 = repr(wb_proxy.sanitize_text(输入))。
    #[test]
    fn sanitize_text_matches_python_refs() {
        let claude = "You are Claude Code, Anthropic's official CLI for Claude. \
                      x-anthropic-billing-header: cc_version=2.1.300.9ab; cc_entrypoint=cli";
        assert_eq!(
            sanitize_text(claude),
            "You are Claude Code, Anthropic's official CLI tool for Claude."
        );
        let main = "Main branch (you will usually use this for PRs) 11128 \
                    Sisyphus-Junior - Focused executor from OhMyOpenCode";
        assert_eq!(
            sanitize_text(main),
            "Default branch (you will usually use this for PRs) 11-128 \
             Sisyphus-Junior - Focused executor"
        );
        assert_eq!(sanitize_text("no fingerprint here"), "no fingerprint here");
        assert_eq!(
            sanitize_text("x-anthropic-billing-header"),
            "x-anthropic-billing-hdr"
        );
        assert_eq!(
            sanitize_text("x-anthropic-billing-header: a=b; done"),
            "done"
        );
    }

    #[test]
    fn no_fingerprint_is_untouched_including_whitespace() {
        // 快路径：不 strip
        assert_eq!(sanitize_text("  plain text  "), "  plain text  ");
    }

    #[test]
    fn sanitize_messages_only_touches_text_parts() {
        let messages = serde_json::json!([
            {"role": "user", "content": "no fingerprint"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "11128 fingerprint"},
                {"type": "image_url", "image_url": {"url": "data:..."}},
            ]},
            {"role": "assistant", "content": "x", "tool_calls": [
                {"id": "t1", "type": "function", "function": {"name": "f", "arguments": "You are Claude Code, Anthropic's official CLI for Claude."}}
            ]},
        ]);
        let out = sanitize_messages(messages.as_array().unwrap());
        assert_eq!(out[0]["content"], "no fingerprint");
        // text 段被清洗，image 段原样
        assert_eq!(out[1]["content"][0]["text"], "11-128 fingerprint");
        assert_eq!(out[1]["content"][1]["type"], "image_url");
        // tool_calls 的 arguments 被清洗（命中 Claude Code 指纹）
        let args = out[2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert!(args.contains("CLI tool for Claude"));
    }
}
