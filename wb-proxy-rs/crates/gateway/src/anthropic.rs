//! Anthropic Messages API 翻译层（M3 Task 5），移植自 wb_proxy.py：
//! - messages_to_chat（:14088-14165）：入站 Messages 请求 → Chat Completions
//! - chat_to_messages（:14220-14264）：出站聚合对象 → Messages 响应
//! - stream_messages_events（:14281-14478）：出站流式 chat SSE → Messages SSE 帧
//! - anthropic_sse_frame（:13815-13819）：`event: X\ndata: {json}\n\n` 帧构造
//! - 辅助族（:13822-14085）：_anthropic_text/_json_text/_parse_tool_input/
//!   _content_to_text/_block_text/_image_part/_document_text/_system_text/
//!   _tool_choice/tools_to_chat/effort_from_messages/blocks_to_messages
//! - _anthropic_estimate_chat_tokens（:14267-14278）：count_tokens 本地估算
//! - _anthropic_usage/_message_id/_stop_reason（:14168-14217）
//!
//! 依赖：stream::estimate_tokens（CJK 估算）、stream::best_cached_tokens、
//! retry::anthropic_error_type。纯函数 + 一个 SSE 状态机（feed/finish 模式，
//! 与 forward::StreamForwarder 同构——api 层逐行喂入、按帧写出）。
//!
//! M3 裁剪：模型默认 max_tokens 来自定价表（model_catalog_meta），定价表属
//! M3-b 后台线程；由调用方注入 catalog_default_max，M3 传入 None（客户端显式
//! 给了 max_tokens 不受影响）。
//!
//! 基础转换函数签名用 Option<&Value>：Python 的 `_anthropic_text(obj.get("x"))`
//! 把缺失字段当 None 传给转换器，Rust 里 `.get()` 返回的 Option 直接透传，
//! None 与 Value::Null 同语义。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::retry::anthropic_error_type;
use crate::stream::{best_cached_tokens, estimate_tokens, strip_data_prefix};

// ---------------------------------------------------------------------------
// 基础转换（wb_proxy.py:13822-13934）
// ---------------------------------------------------------------------------

/// _anthropic_text（:13822-13829）：字符串原样、数字转字符串、其余空串。
pub fn anthropic_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(_) => String::new(),
    }
}

/// _anthropic_json_text（:13832-13840）：None → "{}"；字符串原样；其余紧凑 JSON。
pub fn anthropic_json_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "{}".into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
    }
}

/// _new_id（:13023）：prefix + 16 字节 CSPRNG 的 hex（对齐 uuid4().hex 的 32 位）。
pub fn new_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("os rng must be available");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

/// _anthropic_parse_tool_input（:13843-13858）：tool_use.input 的宽容反序列化。
pub fn parse_tool_input(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(Value::Object(_)) => value.unwrap().clone(),
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Value::Object(Default::default());
            }
            match serde_json::from_str::<Value>(s) {
                Ok(Value::Object(_)) => {
                    serde_json::from_str(s).unwrap_or_else(|_| json!({"value": s}))
                }
                Ok(parsed) => json!({"value": parsed}),
                Err(_) => json!({"raw": s}),
            }
        }
        Some(_) => json!({"value": value.unwrap()}),
    }
}

/// _anthropic_image_part（:13897-13910）：image 块 → OpenAI vision part。
pub fn image_part(block: &Value) -> Value {
    let mut url = String::new();
    if let Value::Object(map) = block {
        if let Some(source) = map.get("source").filter(|v| v.is_object()) {
            let stype = anthropic_text(source.get("type")).to_string();
            match stype.as_str() {
                "base64" => {
                    let media = anthropic_text(source.get("media_type"));
                    let media = if media.is_empty() {
                        "image/png".to_string()
                    } else {
                        media
                    };
                    let data = anthropic_text(source.get("data"));
                    url = format!("data:{media};base64,{data}");
                }
                "url" => url = anthropic_text(source.get("url")),
                "file" => {
                    url = anthropic_text(
                        source
                            .get("file_id")
                            .filter(|v| !v.is_null())
                            .or_else(|| source.get("url")),
                    );
                }
                _ => {}
            }
        }
    }
    json!({"type": "image_url", "image_url": {"url": url}})
}

/// _anthropic_document_text（:13913-13933）：document 块 → 文本摘要。
pub fn document_text(block: &Value) -> String {
    if !block.is_object() {
        return String::new();
    }
    let title = anthropic_text(block.get("title"));
    let source = block.get("source").filter(|v| v.is_object());
    let stype = anthropic_text(source.and_then(|s| s.get("type")));
    let body = match (stype.as_str(), source) {
        ("text", Some(s)) => anthropic_text(s.get("data")),
        ("content", Some(s)) => content_to_text(s.get("content"), None),
        ("base64", Some(s)) => {
            let media = anthropic_text(s.get("media_type"));
            let media = if media.is_empty() {
                "application/octet-stream".to_string()
            } else {
                media
            };
            format!("[document {media} omitted]")
        }
        ("url", Some(s)) => anthropic_text(s.get("url")),
        ("file", Some(s)) => anthropic_text(
            s.get("file_id")
                .filter(|v| !v.is_null())
                .or_else(|| s.get("url")),
        ),
        _ => String::new(),
    };
    if title.is_empty() {
        body
    } else {
        format!("{title}\n{body}")
    }
}

/// _anthropic_block_text（:13887-13894）：text/content 字段的文本抽取。
pub fn block_text(block: &Value) -> String {
    if !block.is_object() {
        return String::new();
    }
    if let Some(text) = block.get("text") {
        return anthropic_text(Some(text));
    }
    if let Some(content) = block.get("content") {
        return content_to_text(Some(content), None);
    }
    String::new()
}

/// _anthropic_content_to_text（:13861-13884）：content 字段 → 纯文本。
/// is_error=Some(true) 时前缀 "error: "（tool_result 的 is_error 语义）。
pub fn content_to_text(value: Option<&Value>, is_error: Option<bool>) -> String {
    let text = match value {
        None => String::new(),
        Some(Value::String(_)) => anthropic_text(value),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                if !item.is_object() {
                    continue;
                }
                let typ = anthropic_text(item.get("type"));
                match typ.as_str() {
                    "image" => {
                        let part = image_part(item);
                        let url = anthropic_text(part.get("image_url").and_then(|v| v.get("url")));
                        parts.push(url);
                    }
                    "document" => parts.push(document_text(item)),
                    _ => parts.push(block_text(item)),
                }
            }
            parts.concat()
        }
        Some(Value::Object(_)) => block_text(value.unwrap()),
        Some(_) => anthropic_text(value),
    };
    if is_error == Some(true) {
        format!("error: {text}")
    } else {
        text
    }
}

/// _anthropic_system_text（:13936-13948）：顶层/消息内 system 字段 → 文本。
pub fn system_text(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                if !item.is_object() {
                    continue;
                }
                let text = anthropic_text(item.get("text"));
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            parts.join("\n")
        }
        Some(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 帧构造与 ID/stop/usage 映射（wb_proxy.py:13815-13819, 14168-14217）
// ---------------------------------------------------------------------------

/// anthropic_sse_frame（:13815-13819）：`event: X\ndata: {json}\n\n` UTF-8。
/// payload 的 "type" 字段被 event 覆盖（Python dict(payload) 后再写 type）。
pub fn anthropic_sse_frame(event: &str, payload: &Value) -> Vec<u8> {
    let mut body = payload.clone();
    body["type"] = Value::String(event.into());
    let raw = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    format!("event: {event}\ndata: {raw}\n\n").into_bytes()
}

/// _anthropic_message_id（:14202-14206）：无值 → 新 msg_ id；非 msg_ 前缀补齐。
fn anthropic_message_id(value: Option<&Value>) -> String {
    let v = anthropic_text(value);
    if v.is_empty() {
        return new_id("msg_");
    }
    if v.starts_with("msg_") {
        v
    } else {
        format!("msg_{v}")
    }
}

/// _anthropic_stop_reason（:14209-14217）：finish_reason → stop_reason。
pub fn anthropic_stop_reason(finish_reason: &str) -> &'static str {
    let v = finish_reason.trim().to_lowercase();
    match v.as_str() {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" | "refusal" => "refusal",
        _ => "end_turn",
    }
}

/// _anthropic_usage（:14168-14199）：chat usage → Messages usage。
/// 缓存别名沿用 stream::best_cached_tokens（Python `_best_cached_tokens`）。
pub fn anthropic_usage(usage: &Value) -> Value {
    let mut out = json!({
        "input_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "output_tokens": 0,
    });
    if !usage.is_object() {
        return out;
    }
    if let Some(p) = usage.get("prompt_tokens").and_then(int_value) {
        out["input_tokens"] = json!(p);
    }
    if let Some(c) = usage.get("completion_tokens").and_then(int_value) {
        out["output_tokens"] = json!(c);
    }
    let cached = usage
        .get("prompt_cache_hit_tokens")
        .and_then(int_value)
        .or_else(|| {
            let best = best_cached_tokens(usage);
            (best > 0).then_some(best)
        });
    if let Some(c) = cached {
        out["cache_read_input_tokens"] = json!(c);
    }
    if let Some(w) = usage.get("prompt_cache_write_tokens").and_then(int_value) {
        out["cache_creation_input_tokens"] = json!(w);
    }
    if let Some(tier) = usage.get("service_tier") {
        if !tier.is_null() {
            out["service_tier"] = tier.clone();
        }
    }
    out
}

/// Python `int(x or 0)` 的宽容整数化（字符串/非数 → None）。
fn int_value(v: &Value) -> Option<i64> {
    let n = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    Some(n)
}

// ---------------------------------------------------------------------------
// 工具翻译（wb_proxy.py:13951-14025）
// ---------------------------------------------------------------------------

/// _anthropic_tool_choice（:13951-13967）：Anthropic tool_choice → chat 形式。
pub fn anthropic_tool_choice(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => {
            let val = s.trim().to_lowercase();
            let mapped = match val.as_str() {
                "auto" => "auto".to_string(),
                "any" => "required".to_string(),
                "none" => "none".to_string(),
                "required" => "required".to_string(),
                _ => val,
            };
            Some(json!(mapped))
        }
        Value::Object(_) => {
            let typ = anthropic_text(value.get("type")).trim().to_lowercase();
            match typ.as_str() {
                "auto" => Some(json!("auto")),
                "any" => Some(json!("required")),
                "none" => Some(json!("none")),
                "tool" => Some(json!({
                    "type": "function",
                    "function": {"name": anthropic_text(value.get("name"))},
                })),
                _ => None,
            }
        }
        _ => None,
    }
}

/// anthropic_tools_to_chat（:13970-13994）：Anthropic 工具 → flat function。
/// 返回 (chat 工具, 跳过名单)。已有 function 形状的原样保留。
pub fn anthropic_tools_to_chat(tools: &Value) -> (Vec<Value>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let Some(items) = tools.as_array() else {
        return (out, skipped);
    };
    for item in items {
        if !item.is_object() {
            continue;
        }
        if item.get("function").is_some_and(|f| f.is_object()) {
            out.push(item.clone());
            continue;
        }
        let name = anthropic_text(item.get("name"));
        let name = if name.is_empty() {
            anthropic_text(item.get("type"))
        } else {
            name
        };
        let mut schema = item.get("input_schema").cloned();
        if schema.is_none() {
            schema = item.get("parameters").cloned();
        }
        let Some(schema) = schema else {
            if !name.is_empty() {
                skipped.push(name);
            }
            continue;
        };
        let mut fn_map = json!({"name": name});
        if let Some(desc) = item.get("description") {
            if !desc.is_null() {
                fn_map["description"] = desc.clone();
            }
        }
        fn_map["parameters"] = schema;
        out.push(json!({"type": "function", "function": fn_map}));
    }
    (out, skipped)
}

/// anthropic_effort_from_messages（:13997-14025）：output_config/thinking → 推理档位。
pub fn anthropic_effort_from_messages(payload: &Value) -> Option<String> {
    if let Value::Object(_) = payload.get("output_config").unwrap_or(&Value::Null) {
        let effort = anthropic_text(payload.get("output_config").and_then(|c| c.get("effort")));
        if !effort.trim().is_empty() {
            return Some(effort);
        }
    }
    let thinking = payload.get("thinking").unwrap_or(&Value::Null);
    if !thinking.is_object() {
        return None;
    }
    let typ = anthropic_text(thinking.get("type")).trim().to_lowercase();
    match typ.as_str() {
        "disabled" => return Some("none".into()),
        "enabled" => {}
        _ => return None,
    }
    let budget = thinking
        .get("budget_tokens")
        .and_then(int_value)
        .unwrap_or(0);
    if budget <= 0 {
        return None;
    }
    if budget >= 32000 {
        Some("xhigh".into())
    } else if budget >= 16000 {
        Some("high".into())
    } else if budget >= 8000 {
        Some("medium".into())
    } else {
        Some("low".into())
    }
}

// ---------------------------------------------------------------------------
// 入站翻译：messages_to_chat（wb_proxy.py:14028-14165）
// ---------------------------------------------------------------------------

/// _anthropic_blocks_to_messages（:14028-14085）：content 块列表 → 多条 chat
/// 消息（tool_result 先出，再拼 assistant/user 消息）。
pub fn blocks_to_messages(role: &str, blocks: &Value) -> Vec<Value> {
    let mut tool_calls = Vec::new();
    let mut tool_messages = Vec::new();
    let mut content_parts = Vec::new();
    let mut texts = Vec::new();
    let mut has_non_text = false;
    let Some(items) = blocks.as_array() else {
        return Vec::new();
    };
    for block in items {
        if !block.is_object() {
            continue;
        }
        let typ = anthropic_text(block.get("type"));
        match typ.as_str() {
            "thinking" | "redacted_thinking" => continue,
            "tool_use" | "server_tool_use" => {
                tool_calls.push(json!({
                    "id": anthropic_text(block.get("id")),
                    "type": "function",
                    "function": {
                        "name": anthropic_text(block.get("name")),
                        "arguments": anthropic_json_text(block.get("input")),
                    },
                }));
                continue;
            }
            "tool_result" => {
                tool_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": anthropic_text(block.get("tool_use_id")),
                    "content": content_to_text(
                        block.get("content"),
                        block.get("is_error").map(|v| v.as_bool().unwrap_or(false)),
                    ),
                }));
                continue;
            }
            "image" => {
                let part = image_part(block);
                if anthropic_text(part.get("image_url").and_then(|v| v.get("url"))).is_empty() {
                    continue;
                }
                has_non_text = true;
                content_parts.push(part);
                continue;
            }
            "document" => {
                has_non_text = true;
                content_parts.push(json!({"type": "text", "text": document_text(block)}));
                continue;
            }
            _ => {}
        }
        let text = block_text(block);
        texts.push(text.clone());
        content_parts.push(json!({"type": "text", "text": text}));
    }
    let mut out = tool_messages;
    if tool_calls.is_empty() && texts.is_empty() && !has_non_text {
        return out;
    }
    let mut msg = json!({"role": if role.is_empty() { "user" } else { role }});
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    if has_non_text {
        msg["content"] = Value::Array(content_parts);
    } else if !texts.is_empty() {
        msg["content"] = Value::String(texts.concat());
    } else {
        msg["content"] = Value::String(String::new());
    }
    out.push(msg);
    out
}

/// messages_to_chat（:14088-14165）。返回 Err(msg) 时对应 400
/// invalid_request_error（Anthropic 信封，见 api 层）。
pub fn messages_to_chat(
    payload: &Value,
    catalog_default_max: Option<i64>,
) -> Result<Value, String> {
    if !payload.is_object() {
        return Err("request body must be a JSON object".into());
    }
    let model = anthropic_text(payload.get("model")).trim().to_string();
    if model.is_empty() {
        return Err("model is required".into());
    }
    let messages_in = payload.get("messages").unwrap_or(&Value::Null);
    if !messages_in.is_array() {
        return Err("messages must be a list".into());
    }
    if messages_in.as_array().unwrap().is_empty() {
        return Err("messages must not be empty".into());
    }
    let mut chat = json!({"model": model, "messages": []});
    for key in ["temperature", "top_p"] {
        if let Some(v) = payload.get(key) {
            if !v.is_null() {
                chat[key] = v.clone();
            }
        }
    }
    let max_tokens = payload.get("max_tokens").unwrap_or(&Value::Null);
    if !max_tokens.is_null() {
        chat["max_tokens"] = max_tokens.clone();
    } else if let Some(default_max) = catalog_default_max {
        chat["max_tokens"] = json!(default_max);
    }
    if payload.get("stream").is_some() {
        chat["stream"] = json!(payload["stream"].as_bool().unwrap_or(false));
    }
    let stops = payload.get("stop_sequences").unwrap_or(&Value::Null);
    if let Some(list) = stops.as_array() {
        if !list.is_empty() {
            chat["stop"] = stops.clone();
        }
    }
    let metadata = payload.get("metadata").unwrap_or(&Value::Null);
    if let Some(uid) = metadata.get("user_id") {
        if !uid.is_null() {
            chat["user"] = uid.clone();
        }
    }
    let (tools, skipped) = anthropic_tools_to_chat(payload.get("tools").unwrap_or(&Value::Null));
    if !tools.is_empty() {
        chat["tools"] = Value::Array(tools);
    }
    if let Some(choice) = anthropic_tool_choice(payload.get("tool_choice").unwrap_or(&Value::Null))
    {
        chat["tool_choice"] = choice;
    }
    let raw_choice = payload.get("tool_choice").unwrap_or(&Value::Null);
    if raw_choice.is_object()
        && raw_choice
            .get("disable_parallel_tool_use")
            .and_then(|v| v.as_bool())
            == Some(true)
    {
        chat["parallel_tool_calls"] = json!(false);
    }
    if let Some(effort) = anthropic_effort_from_messages(payload) {
        chat["reasoning_effort"] = json!(effort);
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut system = system_text(payload.get("system"));
    if !skipped.is_empty() {
        let note = format!(
            "These Anthropic server tools are not available here: {}. Do not call them.",
            skipped.join(", ")
        );
        system = if system.is_empty() {
            note
        } else {
            format!("{system}\n{note}")
        };
    }
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    for (index, item) in messages_in.as_array().unwrap().iter().enumerate() {
        if !item.is_object() {
            continue;
        }
        let role = {
            let r = anthropic_text(item.get("role"));
            let r = r.trim();
            if r.is_empty() {
                "user".to_string()
            } else {
                r.to_string()
            }
        };
        let content = item.get("content");
        if role == "system" || role == "developer" {
            // Claude Code >= 2.1.286 的 mid-conversation system beta：保留原位
            let text = system_text(content);
            let text = if text.is_empty() {
                content_to_text(content, None)
            } else {
                text
            };
            if !text.is_empty() {
                messages.push(json!({"role": "system", "content": text}));
            }
            continue;
        }
        if role != "user" && role != "assistant" {
            return Err(format!(
                "messages[{index}].role {role:?} must be user or assistant; \
                 pass a system prompt in the top-level system field"
            ));
        }
        match content {
            Some(v) if v.is_string() => {
                messages.push(json!({"role": role, "content": v}));
            }
            Some(v) if v.is_array() => {
                messages.extend(blocks_to_messages(&role, v));
            }
            Some(v) if !v.is_null() => {
                let text = anthropic_text(Some(v));
                if !text.is_empty() {
                    messages.push(json!({"role": role, "content": text}));
                }
            }
            _ => {}
        }
    }
    chat["messages"] = Value::Array(messages);
    Ok(chat)
}

// ---------------------------------------------------------------------------
// 出站折叠：chat_to_messages（wb_proxy.py:14220-14264）
// ---------------------------------------------------------------------------

/// chat_to_messages：聚合后的 chat.completion → Anthropic Messages 对象。
/// reasoning_content 变 thinking 块；tool_calls 变 tool_use 块；finish_reason
/// 映射 stop_reason。
pub fn chat_to_messages(obj: &Value) -> Value {
    let choices = obj
        .get("choices")
        .filter(|v| v.is_array())
        .unwrap_or(&Value::Null);
    let mut message = Value::Object(Default::default());
    let mut finish = String::new();
    if let Some(first) = choices
        .as_array()
        .and_then(|c| c.first())
        .filter(|c| c.is_object())
    {
        if let Some(m) = first.get("message").filter(|m| m.is_object()) {
            message = m.clone();
        }
        finish = first
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    let mut content: Vec<Value> = Vec::new();
    let reasoning = anthropic_text(message.get("reasoning_content"));
    if !reasoning.is_empty() {
        content.push(json!({"type": "thinking", "thinking": reasoning}));
    }
    let text = match message.get("content") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|p| p.is_object())
            .map(block_text)
            .collect::<Vec<_>>()
            .concat(),
        other => anthropic_text(other),
    };
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            if !call.is_object() {
                continue;
            }
            let fn_map = call
                .get("function")
                .filter(|f| f.is_object())
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let call_id = anthropic_text(call.get("id"));
            let call_id = if call_id.is_empty() {
                new_id("toolu_")
            } else {
                call_id
            };
            content.push(json!({
                "type": "tool_use",
                "id": call_id,
                "name": anthropic_text(fn_map.get("name")),
                "input": parse_tool_input(fn_map.get("arguments")),
            }));
        }
    }
    json!({
        "id": anthropic_message_id(obj.get("id")),
        "type": "message",
        "role": "assistant",
        "model": anthropic_text(obj.get("model")),
        "content": content,
        "stop_reason": anthropic_stop_reason(&finish),
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(obj.get("usage").unwrap_or(&Value::Null)),
    })
}

// ---------------------------------------------------------------------------
// 本地估算：count_tokens（wb_proxy.py:14267-14278）
// ---------------------------------------------------------------------------

/// _anthropic_estimate_chat_tokens：messages + tool_calls + tools 逐段
/// estimate_tokens 求和。与 Python 完全同口径（含 content 缺失时 "{}" 占位）。
pub fn estimate_chat_tokens(chat: &Value) -> i64 {
    let mut total = 0i64;
    if !chat.is_object() {
        return 0;
    }
    let empty_msgs: Vec<Value> = Vec::new();
    for msg in chat
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_msgs)
    {
        if !msg.is_object() {
            continue;
        }
        total += estimate_tokens(&anthropic_json_text(msg.get("content")));
        let empty_calls: Vec<Value> = Vec::new();
        for call in msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty_calls)
        {
            if call.is_object() {
                total += estimate_tokens(&anthropic_json_text(Some(call)));
            }
        }
    }
    let empty_tools: Vec<Value> = Vec::new();
    for tool in chat
        .get("tools")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_tools)
    {
        total += estimate_tokens(&anthropic_json_text(Some(tool)));
    }
    total
}

// ---------------------------------------------------------------------------
// 出站流式：MessagesSseTranslator（wb_proxy.py:14281-14478）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ToolEntry {
    block: i64,
    id: String,
    name: String,
    opened: bool,
}

/// stream_messages_events 的状态机。api 层逐行 feed 上游 chat SSE，
/// finish() 补尾帧（content_block_stop × N + message_delta + message_stop）。
/// 帧产出与 Python generator 逐字节一致（帧字节由 anthropic_sse_frame 构造）。
pub struct MessagesSseTranslator {
    started: bool,
    failed: bool,
    id: String,
    model: String,
    stop: String,
    thinking_open: bool,
    thinking_idx: i64,
    text_open: bool,
    text_idx: i64,
    next: i64,
    tools: HashMap<i64, ToolEntry>,
    tool_order: Vec<i64>,
    usage: Option<Value>,
}

impl MessagesSseTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            started: false,
            failed: false,
            id: String::new(),
            model: model.to_string(),
            stop: "end_turn".to_string(),
            thinking_open: false,
            thinking_idx: 0,
            text_open: false,
            text_idx: 0,
            next: 0,
            tools: HashMap::new(),
            tool_order: Vec::new(),
            usage: None,
        }
    }

    /// 用完流后取回上游 usage（Python holder["usage"]，供 usage 记录）。
    pub fn usage(&self) -> Option<Value> {
        self.usage.clone()
    }

    fn ensure_start(&mut self, chunk: Option<&Value>) -> Vec<Vec<u8>> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if let Some(chunk) = chunk {
            let id = anthropic_text(chunk.get("id"));
            if !id.is_empty() {
                self.id = id;
            }
            let model = anthropic_text(chunk.get("model"));
            if !model.is_empty() {
                self.model = model;
            }
        }
        let id = anthropic_message_id(Some(&Value::String(self.id.clone())));
        let model = self.model.clone();
        vec![anthropic_sse_frame(
            "message_start",
            &json!({
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        )]
    }

    fn ensure_thinking(&mut self) -> Vec<Vec<u8>> {
        if self.thinking_open {
            return Vec::new();
        }
        self.thinking_idx = self.next;
        self.next += 1;
        self.thinking_open = true;
        vec![anthropic_sse_frame(
            "content_block_start",
            &json!({
                "index": self.thinking_idx,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
        )]
    }

    fn ensure_text(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        if self.text_open {
            return frames;
        }
        if self.thinking_open {
            self.thinking_open = false;
            frames.push(anthropic_sse_frame(
                "content_block_stop",
                &json!({"index": self.thinking_idx}),
            ));
        }
        self.text_idx = self.next;
        self.next += 1;
        self.text_open = true;
        frames.push(anthropic_sse_frame(
            "content_block_start",
            &json!({
                "index": self.text_idx,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        frames
    }

    fn close_thinking(&mut self, frames: &mut Vec<Vec<u8>>) {
        if self.thinking_open {
            self.thinking_open = false;
            frames.push(anthropic_sse_frame(
                "content_block_stop",
                &json!({"index": self.thinking_idx}),
            ));
        }
    }

    fn close_text(&mut self, frames: &mut Vec<Vec<u8>>) {
        if self.text_open {
            self.text_open = false;
            frames.push(anthropic_sse_frame(
                "content_block_stop",
                &json!({"index": self.text_idx}),
            ));
        }
    }

    /// 喂一行上游 SSE（"data: ..." 或 "[DONE]"），返回本行产出的帧字节。
    pub fn feed(&mut self, line: &str) -> Vec<Vec<u8>> {
        if self.failed {
            return Vec::new();
        }
        let data = strip_data_prefix(line);
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            return Vec::new();
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut frames = Vec::new();
        if let Some(err) = chunk.get("error").filter(|e| e.is_object()) {
            self.failed = true;
            let message = anthropic_text(err.get("message"));
            let message = if message.is_empty() {
                "upstream error".to_string()
            } else {
                message
            };
            frames.push(anthropic_sse_frame(
                "error",
                &json!({
                    "error": {
                        "type": anthropic_error_type(502),
                        "message": message,
                    }
                }),
            ));
            return frames;
        }
        let start = self.ensure_start(Some(&chunk));
        frames.extend(start);
        // usage 在 choices 守卫之前捕获：末帧带 usage + finish_reason 且无 delta
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) else {
            return frames;
        };
        let Some(choice) = choices.first().filter(|c| c.is_object()) else {
            return frames;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if !delta.is_object() {
            return frames;
        }
        if let Some(reason) = delta.get("reasoning_content").filter(|v| v.is_string()) {
            let reason = reason.as_str().unwrap_or("");
            if !reason.is_empty() {
                frames.extend(self.ensure_thinking());
                frames.push(anthropic_sse_frame(
                    "content_block_delta",
                    &json!({
                        "index": self.thinking_idx,
                        "delta": {"type": "thinking_delta", "thinking": reason},
                    }),
                ));
            }
        }
        if let Some(text) = delta.get("content").filter(|v| v.is_string()) {
            let text = text.as_str().unwrap_or("");
            if !text.is_empty() {
                frames.extend(self.ensure_text());
                frames.push(anthropic_sse_frame(
                    "content_block_delta",
                    &json!({
                        "index": self.text_idx,
                        "delta": {"type": "text_delta", "text": text},
                    }),
                ));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                if !call.is_object() {
                    continue;
                }
                let idx = call.get("index").and_then(int_value).unwrap_or(0);
                let call_id = anthropic_text(call.get("id"));
                let fn_map = call.get("function").unwrap_or(&Value::Null);
                let call_name = anthropic_text(fn_map.get("name"));
                let args = match fn_map.get("arguments") {
                    Some(v) if !v.is_null() => anthropic_text(Some(v)),
                    _ => String::new(),
                };
                // 先算 should_open 再闭块、再拿 entry：close_* 是 &mut self 方法、
                // entry 借 self.tools 字段，顺序必须闭块在前（避免 E0499）。
                let should_open = match self.tools.get(&idx) {
                    Some(e) => {
                        !e.opened && (!e.id.is_empty() || !e.name.is_empty() || !args.is_empty())
                    }
                    None => !call_id.is_empty() || !call_name.is_empty() || !args.is_empty(),
                };
                if should_open {
                    self.close_thinking(&mut frames);
                    self.close_text(&mut frames);
                }
                let entry = self.tools.entry(idx).or_default();
                if !call_id.is_empty() {
                    entry.id = call_id;
                }
                if !call_name.is_empty() {
                    entry.name = call_name;
                }
                if should_open {
                    entry.opened = true;
                    entry.block = self.next;
                    self.next += 1;
                    if entry.id.is_empty() {
                        entry.id = format!("toolu_{idx}");
                    }
                    if !self.tool_order.contains(&idx) {
                        self.tool_order.push(idx);
                    }
                    frames.push(anthropic_sse_frame(
                        "content_block_start",
                        &json!({
                            "index": entry.block,
                            "content_block": {
                                "type": "tool_use",
                                "id": entry.id.clone(),
                                "name": entry.name.clone(),
                                "input": {},
                            },
                        }),
                    ));
                }
                if entry.opened && !args.is_empty() {
                    frames.push(anthropic_sse_frame(
                        "content_block_delta",
                        &json!({
                            "index": entry.block,
                            "delta": {"type": "input_json_delta", "partial_json": args},
                        }),
                    ));
                }
            }
        }
        let finish = choice.get("finish_reason").unwrap_or(&Value::Null);
        if let Some(f) = finish.as_str() {
            if !f.is_empty() {
                self.stop = anthropic_stop_reason(f).to_string();
            }
        }
        frames
    }

    /// 流末收尾：未启动 → message_start(None)；关闭所有打开的块；message_delta
    /// + message_stop。failed 时无尾帧（Python `if state["failed"]: return`）。
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        if self.failed {
            return frames;
        }
        if !self.started {
            frames.extend(self.ensure_start(None));
        }
        self.close_thinking(&mut frames);
        self.close_text(&mut frames);
        let tool_blocks: Vec<i64> = self
            .tool_order
            .iter()
            .filter_map(|idx| self.tools.get(idx).filter(|e| e.opened).map(|e| e.block))
            .collect();
        for block in tool_blocks {
            frames.push(anthropic_sse_frame(
                "content_block_stop",
                &json!({"index": block}),
            ));
        }
        let stop = self.stop.clone();
        let usage = self
            .usage()
            .map(|u| anthropic_usage(&u))
            .unwrap_or_else(|| {
                json!({
                    "input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 0,
                })
            });
        frames.push(anthropic_sse_frame(
            "message_delta",
            &json!({
                "delta": {"stop_reason": stop, "stop_sequence": Value::Null},
                "usage": usage,
            }),
        ));
        frames.push(anthropic_sse_frame("message_stop", &json!({})));
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ss(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    // ---- messages_to_chat ----

    #[test]
    fn messages_to_chat_translates_basic_request() {
        let payload = json!({
            "model": "hy4-preview",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "temperature": 0.2,
            "stop_sequences": ["END"],
            "stream": false,
        });
        let chat = messages_to_chat(&payload, None).unwrap();
        assert_eq!(chat["model"], "hy4-preview");
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][0]["content"], "hi");
        assert_eq!(chat["max_tokens"], 1024);
        assert_eq!(chat["temperature"], json!(0.2));
        assert_eq!(chat["stop"], json!(["END"]));
        assert_eq!(chat["stream"], json!(false));
    }

    #[test]
    fn messages_to_chat_system_and_catalog_default() {
        // 顶层 system → 首条 system 消息
        let payload = json!({
            "model": "hy4-preview",
            "system": [{"type": "text", "text": "be nice"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = messages_to_chat(&payload, Some(8192)).unwrap();
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "be nice");
        // 客户端没给 max_tokens 时用注入的默认
        assert_eq!(chat["max_tokens"], 8192);
    }

    #[test]
    fn messages_to_chat_rejects_bad_input() {
        assert!(messages_to_chat(&json!([]), None).is_err()); // 非对象
        assert!(messages_to_chat(&json!({"model": "m"}), None).is_err()); // 无 messages
        assert!(messages_to_chat(&json!({"model": "m", "messages": []}), None).is_err()); // 空
        assert!(messages_to_chat(&json!({"model": "", "messages": [ss("u", "x")]}), None).is_err());
        // 非法 role
        let bad = json!({"model": "m", "messages": [{"role": "robot", "content": "x"}]});
        assert!(messages_to_chat(&bad, None).is_err());
    }

    #[test]
    fn messages_to_chat_blocks_tool_use_and_result() {
        let payload = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "calc",
                     "input": {"a": 1}},
                ],
            }, {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "2"},
                ],
            }],
        });
        let chat = messages_to_chat(&payload, None).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // 跨消息保持顺序：assistant(tool_use) 消息在前、user(tool_result) 的
        // tool 消息在后（Python _anthropic_blocks_to_messages 逐消息处理）
        let assistant = &msgs[0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "calc");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        let tool = &msgs[1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "t1");
        assert_eq!(tool["content"], "2");
    }

    #[test]
    fn messages_to_chat_tools_and_effort() {
        let payload = json!({
            "model": "m",
            "messages": [ss("user", "hi")],
            "tools": [{"name": "t", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"},
            "thinking": {"type": "enabled", "budget_tokens": 20000},
        });
        let chat = messages_to_chat(&payload, None).unwrap();
        assert_eq!(chat["tools"][0]["function"]["name"], "t");
        assert_eq!(chat["tool_choice"], "required");
        assert_eq!(chat["reasoning_effort"], "high");
    }

    #[test]
    fn messages_to_chat_skipped_tools_appended_to_system() {
        let payload = json!({
            "model": "m",
            "system": "base",
            "messages": [ss("user", "hi")],
            "tools": [{"name": "server_tool_no_schema"}],
        });
        let chat = messages_to_chat(&payload, None).unwrap();
        let system = chat["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with(
            "base\nThese Anthropic server tools are not available here: server_tool_no_schema"
        ));
    }

    // ---- chat_to_messages ----

    #[test]
    fn chat_to_messages_folds_text_and_usage() {
        let obj = json!({
            "id": "chatcmpl-1", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5,
                      "total_tokens": 15, "prompt_cache_hit_tokens": 3},
        });
        let out = chat_to_messages(&obj);
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["id"], "msg_chatcmpl-1");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "ok");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 5);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 3);
    }

    #[test]
    fn chat_to_messages_thinking_and_tool_use() {
        let obj = json!({
            "choices": [{"message": {
                "reasoning_content": "think...",
                "content": "",
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "calc", "arguments": "{\"a\":1}"}}],
            }, "finish_reason": "tool_calls"}],
        });
        let out = chat_to_messages(&obj);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "think...");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["name"], "calc");
        assert_eq!(content[1]["input"]["a"], 1);
        assert_eq!(out["stop_reason"], "tool_use");
    }

    // ---- estimate_chat_tokens ----

    #[test]
    fn estimate_chat_tokens_sums_messages_and_tools() {
        // 纯 CJK：每条 = 2 CJK 字 + 其余保底 1 = 3；两条 = 6
        let chat = json!({
            "messages": [{"role": "user", "content": "你好"},
                         {"role": "assistant", "content": "世界"}],
        });
        assert_eq!(estimate_chat_tokens(&chat), 6);
        // tool_calls / tools 也计入（相对增大，数值取决于 JSON 序列化长度）
        let base =
            estimate_chat_tokens(&json!({"messages": [{"role": "assistant", "content": "hi"}]}));
        let with_tools = estimate_chat_tokens(&json!({
            "messages": [{"role": "assistant", "content": "hi",
                          "tool_calls": [{"function": {"name": "x", "arguments": "{}"}}]}],
            "tools": [{"function": {"name": "y", "parameters": {}}}],
        }));
        assert!(with_tools > base);
    }

    // ---- 帧构造 ----

    #[test]
    fn anthropic_sse_frame_shape_and_type_override() {
        let frame = anthropic_sse_frame("message_stop", &json!({}));
        assert_eq!(
            String::from_utf8(frame).unwrap(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let frame = anthropic_sse_frame("ping", &json!({"type": "ignored", "a": 1}));
        let text = String::from_utf8(frame).unwrap();
        // Python `dict` 覆盖已有键时保留原位置（`body["type"] = event`，:13817），
        // 于是 `{"type": ..., "a": 1}` 覆盖后仍是 `type` 在前。开启 serde_json
        // `preserve_order` 后 Rust 侧同样是「按键存在则原位替换」，故断言键序不变。
        assert!(text.starts_with("event: ping\ndata: {\"type\":\"ping\",\"a\":1}"));
    }

    #[test]
    fn anthropic_stop_reason_mappings() {
        assert_eq!(anthropic_stop_reason("stop"), "end_turn");
        assert_eq!(anthropic_stop_reason("length"), "max_tokens");
        assert_eq!(anthropic_stop_reason("tool_calls"), "tool_use");
        assert_eq!(anthropic_stop_reason("content_filter"), "refusal");
        assert_eq!(anthropic_stop_reason(""), "end_turn");
    }

    // ---- SSE 状态机 ----

    fn sse_line(data: &str) -> String {
        format!("data: {data}")
    }

    #[test]
    fn sse_stream_text_deltas_and_message_stop() {
        let mut t = MessagesSseTranslator::new("m");
        let mut out = Vec::new();
        out.extend(t.feed(&sse_line(
            r#"{"id":"c1","model":"m","choices":[{"delta":{"role":"assistant","content":"he"},"index":0}]}"#,
        )));
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{"content":"llo"},"index":0}]}"#,
        )));
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":4,"completion_tokens":5}}"#,
        )));
        out.extend(t.finish());
        let frames: Vec<String> = out
            .iter()
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect();
        // message_start → content_block_start(text) → text_delta×2 → (finish) →
        // content_block_stop → message_delta → message_stop
        assert!(frames[0].starts_with("event: message_start"));
        assert!(frames[0].contains("\"type\":\"message\""));
        assert!(frames[1].starts_with("event: content_block_start"));
        assert!(frames[1].contains("\"type\":\"text\""));
        assert!(frames[2].starts_with("event: content_block_delta"));
        assert!(frames[2].contains("\"text\":\"he\""));
        assert!(frames[3].contains("\"text\":\"llo\""));
        assert!(frames[4].starts_with("event: content_block_stop"));
        assert!(frames[5].starts_with("event: message_delta"));
        assert!(frames[5].contains("\"stop_reason\":\"end_turn\""));
        assert!(frames[5].contains("\"input_tokens\":4"));
        assert!(frames[6].starts_with("event: message_stop"));
        assert_eq!(frames.len(), 7);
    }

    #[test]
    fn sse_stream_thinking_then_text_orders_blocks() {
        let mut t = MessagesSseTranslator::new("m");
        let mut out = Vec::new();
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{"reasoning_content":"think"},"index":0}]}"#,
        )));
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{"content":"answer"},"index":0}]}"#,
        )));
        out.extend(t.finish());
        let frames: Vec<String> = out
            .iter()
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect();
        // [0]=message_start → [1]=block_start(thinking) → [2]=thinking_delta →
        // [3]=block_stop(0) → [4]=block_start(text) → [5]=text_delta →
        // [6]=block_stop(1) → [7]=message_delta → [8]=message_stop
        assert_eq!(frames.len(), 9);
        assert!(frames[0].starts_with("event: message_start"));
        assert!(frames[1].contains("\"type\":\"thinking\""));
        assert!(frames[2].contains("\"thinking\":\"think\""));
        assert!(frames[3].starts_with("event: content_block_stop"));
        assert!(frames[3].contains("\"index\":0"));
        assert!(frames[4].contains("\"type\":\"text\""));
        assert!(frames[5].contains("\"text\":\"answer\""));
        assert!(frames[6].starts_with("event: content_block_stop"));
        assert!(frames[6].contains("\"index\":1"));
        assert!(frames[7].starts_with("event: message_delta"));
    }

    #[test]
    fn sse_stream_tool_use_frames_and_input_json_delta() {
        let mut t = MessagesSseTranslator::new("m");
        let mut out = Vec::new();
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"calc","arguments":""}}]},"index":0}]}"#,
        )));
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":"}}]},"index":0}]}"#,
        )));
        out.extend(t.feed(&sse_line(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls","index":0}]}"#,
        )));
        out.extend(t.finish());
        let frames: Vec<String> = out
            .iter()
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect();
        // [0]=message_start → [1]=block_start(tool_use) → [2],[3]=input_json_delta×2
        // → [4]=block_stop → [5]=message_delta(tool_use) → [6]=message_stop
        assert_eq!(frames.len(), 7);
        assert!(frames[0].starts_with("event: message_start"));
        assert!(frames[1].starts_with("event: content_block_start"));
        assert!(frames[1].contains("\"type\":\"tool_use\""));
        assert!(frames[1].contains("\"id\":\"call_9\""));
        assert!(frames[1].contains("\"name\":\"calc\""));
        assert!(frames[2].starts_with("event: content_block_delta"));
        assert!(frames[2].contains("\"input_json_delta\""));
        // 帧内 data 行的 JSON 已二次转义：partial_json 的裸值 {"a": 在帧里写作 {\"a\":
        assert!(frames[2].contains(r#"{\"a\":"#));
        assert!(frames[3].contains("1}"));
        assert!(frames[3].contains("1}"));
        assert!(frames[4].starts_with("event: content_block_stop"));
        assert!(frames[5].starts_with("event: message_delta"));
        assert!(frames[5].contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn sse_stream_error_frame_terminates() {
        let mut t = MessagesSseTranslator::new("m");
        let out = t.feed(&sse_line(
            r#"{"error": {"message": "boom", "type": "server_error"}}"#,
        ));
        assert_eq!(out.len(), 1);
        let text = String::from_utf8_lossy(&out[0]).to_string();
        assert!(text.starts_with("event: error"));
        assert!(text.contains("\"type\":\"api_error\""));
        assert!(text.contains("\"message\":\"boom\""));
        // failed 后 feed/finish 不再产出
        assert!(t
            .feed(&sse_line(r#"{"choices":[{"delta":{"content":"x"}}]}"#))
            .is_empty());
        assert!(t.finish().is_empty());
    }

    #[test]
    fn sse_stream_empty_upstream_yields_synthetic_message_start() {
        let mut t = MessagesSseTranslator::new("m");
        let out = t.finish();
        assert_eq!(out.len(), 3); // message_start + message_delta + message_stop
        let text = String::from_utf8_lossy(&out[0]).to_string();
        assert!(text.starts_with("event: message_start"));
    }

    #[test]
    fn sse_stream_skips_done_and_non_data_lines() {
        let mut t = MessagesSseTranslator::new("m");
        assert!(t.feed("").is_empty());
        assert!(t.feed(": comment").is_empty());
        assert!(t.feed("data: [DONE]").is_empty());
        assert!(t.feed("not json").is_empty());
        assert!(!t.started);
        assert!(!t
            .feed(&sse_line(
                r#"{"choices":[{"delta":{"content":"x"},"index":0}]}"#
            ))
            .is_empty());
    }
}
