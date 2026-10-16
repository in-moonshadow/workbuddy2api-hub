//! Responses API（`/v1/responses`、`/responses`）↔ Chat Completions 的结构翻译层。
//!
//! 移植自 wb_proxy.py：
//! - `_flatten_content`（:13025-13057）
//! - `_responses_input_to_messages`（:13525-13738）
//! - `responses_to_chat`（:13741-13778）
//! - `_responses_usage`（:14481-14495）
//! - `chat_to_response`（:14496-14589）
//!
//! M3 范围（用户 2026-10-16 裁决「结构翻译进 M3，流式推 M3-b」）：只做**非流式
//! 结构翻译**。以下支线推迟 M3-b，本模块不实现、也不假装实现：
//! - namespace 工具展开/还原（:13175-13300 的 `expand_namespace_tools` /
//!   `apply_namespace_to_calls`）——需 `_namespace_map` 贯穿请求与响应两侧；
//! - web-tools 注入与代跑（`wb_webtools.install_tool_defs`、`web_tools_active`）；
//! - DSML tool calls 兜底（:11205 `parse_dsml_tool_calls`）；
//! - 引用注释（`build_citations`）；
//! - 流式 `stream_responses_events`（:14590-15085，496 行）。
//!
//! custom（freeform）工具的降级与重膨胀**在 M3 范围内**：它只依赖请求自身的
//! `tools` 声明，无外部状态。

use serde_json::{json, Map, Value};

use crate::anthropic::{anthropic_json_text, new_id};

// ---------------------------------------------------------------------------
// _flatten_content（:13025-13057）
// ---------------------------------------------------------------------------

/// `_flatten_content`：把 Responses 风格 content 摊平成文本；若含图片则保留
/// OpenAI vision parts（结构化数组）。
///
/// 返回 `Value`：`String`（纯文本）或 `Array`（多模态 parts）。
/// 注意 Python 第 13057 行 `chr(10).join(t for t in texts if t)` —— **空文本段被过滤**。
pub fn flatten_content(content: Option<&Value>) -> Value {
    let Some(content) = content else {
        return Value::String(String::new());
    };
    match content {
        Value::Null => Value::String(String::new()),
        Value::String(s) => Value::String(s.clone()),
        // 非字符串非数组 → str(content)（:13031-13032）
        other if !other.is_array() => Value::String(python_str(other)),
        Value::Array(pieces) => {
            let mut texts: Vec<String> = Vec::new();
            let mut parts: Vec<Value> = Vec::new();
            for piece in pieces {
                match piece {
                    Value::String(s) => {
                        texts.push(s.clone());
                        parts.push(json!({"type": "text", "text": s}));
                        continue;
                    }
                    Value::Object(_) => {}
                    // Python 的 `not isinstance(piece, dict)` → continue（:13039-13040）
                    _ => continue,
                }
                let ptype = piece.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(
                    ptype,
                    "input_text" | "output_text" | "text" | "summary_text"
                ) {
                    let t = piece.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    texts.push(t.to_string());
                    parts.push(json!({"type": "text", "text": t}));
                } else if matches!(ptype, "input_image" | "image_url" | "image")
                    || piece.get("image_url").is_some()
                {
                    // url = piece.image_url or piece.url；若 image_url 是 dict 取其 url（:13047-13049）
                    let url_field = piece.get("image_url").or_else(|| piece.get("url"));
                    let url = match url_field {
                        Some(Value::Object(_)) => url_field
                            .and_then(|v| v.get("url"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        Some(Value::String(s)) => Some(s.clone()),
                        _ => None,
                    };
                    // 无 url 但有 data → 拼 data URI，mime 取 mimeType/mime_type，默认 image/png（:13050-13052）
                    let url = url.or_else(|| {
                        piece.get("data").and_then(|v| v.as_str()).map(|data| {
                            let mime = piece
                                .get("mimeType")
                                .or_else(|| piece.get("mime_type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("image/png");
                            format!("data:{mime};base64,{data}")
                        })
                    });
                    if let Some(url) = url {
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                }
            }
            // 只要出现图片 part，就整体保留结构化 parts（:13055-13056）
            if parts
                .iter()
                .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("image_url"))
            {
                return Value::Array(parts);
            }
            Value::String(
                texts
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        // Bool/Number/Object 由上面的守卫分支处理；此处仅为穷尽性兜底，不可达
        _ => Value::String(python_str(content)),
    }
}

/// 近似 Python `str(value)`：字符串原样、bool 用 True/False、null 用 None、
/// 数字直出，容器用 JSON。
fn python_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        Value::Number(n) => n.to_string(),
        other => anthropic_json_text(Some(other)),
    }
}

// ---------------------------------------------------------------------------
// _responses_input_to_messages（:13525-13738）
// ---------------------------------------------------------------------------

/// `_responses_input_to_messages`：把 Responses 的 `input` 项转成 chat messages。
///
/// M3 不做 namespace 还原（那发生在回程 `chat_to_response`），故这里无 `_namespace_map`。
pub fn responses_input_to_messages(payload: &Value) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    // instructions 非空字符串 → 前置 system 消息（:13528-13530）
    if let Some(Value::String(instr)) = payload.get("instructions") {
        if !instr.trim().is_empty() {
            messages.push(json!({"role": "system", "content": instr}));
        }
    }
    let mut pending_reasoning = String::new();
    let Some(inp) = payload.get("input") else {
        return messages;
    };
    match inp {
        Value::String(s) => messages.push(json!({"role": "user", "content": s})),
        Value::Array(items) => {
            for item in items {
                if let Value::String(s) = item {
                    messages.push(json!({"role": "user", "content": s}));
                    continue;
                }
                if !item.is_object() {
                    continue;
                }
                match item.get("type").and_then(|v| v.as_str()) {
                    None | Some("message") | Some("user") => {
                        push_message_item(&mut messages, item, &mut pending_reasoning);
                    }
                    Some("reasoning") => {
                        push_reasoning_item(&mut messages, item, &mut pending_reasoning);
                    }
                    Some("function_call_output") => {
                        push_function_call_output(&mut messages, item);
                    }
                    Some("function_call") => {
                        push_tool_call_item(&mut messages, item, &mut pending_reasoning, false);
                    }
                    Some("custom_tool_call") => {
                        push_tool_call_item(&mut messages, item, &mut pending_reasoning, true);
                    }
                    Some("custom_tool_call_output") => {
                        push_simple_tool_output(&mut messages, item);
                    }
                    Some("agent_message") => {
                        push_agent_message(&mut messages, item);
                    }
                    Some(other) => {
                        // :13736-13737 记警告后丢弃
                        warn_unhandled_item(other);
                    }
                }
            }
        }
        // 其它类型（数字/布尔/对象）：Python 走 `isinstance(input, list)` 之外的分支，
        // 即不做任何事（:13738 之后无 else）
        _ => {}
    }
    messages
}

/// 未知 input item 类型告警（对应 :13736-13737 的 `log.warning`）。
/// M3 用 stderr；5.4 接线时如需接入统一日志再替换。
fn warn_unhandled_item(itype: &str) {
    eprintln!("[responses] unsupported input item type: {itype}");
}

/// `type` 为 None/"message"/"user" 的项（:13543-13566）。
fn push_message_item(messages: &mut Vec<Value>, item: &Value, pending: &mut String) {
    let body = flatten_content(item.get("content"));
    if is_empty_content(&body) {
        return;
    }
    let mut role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
    // developer 归一到 system（:13547-13548）
    if role == "developer" {
        role = "system";
    }
    // assistant 文本与前一条 assistant 合并，避免打断 tool 序列（:13549-13560）
    if role == "assistant" {
        if let Some(prev) = messages.last_mut().filter(|m| is_role(m, "assistant")) {
            let existing = prev.get("content").cloned().unwrap_or(Value::Null);
            let merged = match existing {
                Value::String(s) if !s.is_empty() => {
                    Value::String(format!("{s}\n{}", body_text(&body)))
                }
                _ => body.clone(),
            };
            if let Some(o) = prev.as_object_mut() {
                o.insert("content".into(), merged);
                if !pending.is_empty() && !o.contains_key("reasoning_content") {
                    o.insert(
                        "reasoning_content".into(),
                        Value::String(std::mem::take(pending)),
                    );
                }
            }
            return;
        }
    }
    let mut msg = Map::new();
    msg.insert("role".into(), Value::String(role.to_string()));
    msg.insert("content".into(), body);
    if role == "assistant" && !pending.is_empty() {
        msg.insert(
            "reasoning_content".into(),
            Value::String(std::mem::take(pending)),
        );
    }
    messages.push(Value::Object(msg));
}

/// `type == "reasoning"`（:13567-13589）。
fn push_reasoning_item(messages: &mut [Value], item: &Value, pending: &mut String) {
    // summary 为数组时取各段 text 换行拼接；字符串直接用（:13572-13578）
    let mut r_text = match item.get("summary") {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.is_object())
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    if r_text.is_empty() {
        r_text = text_of(item.get("content"));
    }
    if r_text.is_empty() {
        return;
    }
    // 前一条是 assistant 就回填，否则暂存（:13586-13589）
    if let Some(prev) = messages.last_mut().filter(|m| is_role(m, "assistant")) {
        if let Some(o) = prev.as_object_mut() {
            o.insert("reasoning_content".into(), Value::String(r_text));
        }
    } else {
        *pending = r_text;
    }
}

/// `type == "function_call_output"`（:13590-13638）。
fn push_function_call_output(messages: &mut Vec<Value>, item: &Value) {
    let raw_out = item.get("output");
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    if call_id.is_empty() {
        // 孤儿 function_call_output：包装成用户指令（:13592-13622）
        let txt = match raw_out {
            Some(Value::Array(_)) => flatten_content(raw_out),
            Some(Value::Object(_)) => Value::String(anthropic_json_text(raw_out)),
            other => Value::String(other.map(python_str).unwrap_or_default()),
        };
        if let Value::Array(parts) = txt {
            // 多模态：前缀文本 part 后跟原 parts（:13599-13614）
            let mut content = vec![json!({"type": "text", "text": ORPHAN_HINT})];
            content.extend(parts);
            messages.push(json!({"role": "user", "content": content}));
            return;
        }
        let text = text_of(Some(&txt));
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            messages.push(json!({
                "role": "user",
                "content": format!("{ORPHAN_HINT}\n\n{trimmed}"),
            }));
        }
        return;
    }
    let content = match raw_out {
        Some(Value::Array(_)) => flatten_content(raw_out),
        Some(Value::Object(o)) => {
            // 图片对象走多模态，其余序列化为 JSON（:13626-13629）
            let is_image = matches!(
                o.get("type").and_then(|v| v.as_str()),
                Some("input_image") | Some("image_url") | Some("image")
            ) || o.contains_key("image_url");
            if is_image {
                flatten_content(raw_out)
            } else {
                Value::String(anthropic_json_text(raw_out))
            }
        }
        Some(Value::String(s)) => Value::String(s.clone()),
        other => Value::String(other.map(python_str).unwrap_or_default()),
    };
    messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
}

/// `type == "function_call"` / `"custom_tool_call"`（:13639-13702）。
///
/// `custom` 为真时把 `input` 包成 `{"input": ...}`（:13675-13682）。
fn push_tool_call_item(
    messages: &mut Vec<Value>,
    item: &Value,
    pending: &mut String,
    custom: bool,
) {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = if custom {
        let raw = item.get("input");
        let raw_str = match raw {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(_)) | Some(Value::Array(_)) => anthropic_json_text(raw),
            Some(Value::Null) | None => String::new(),
            Some(other) => python_str(other),
        };
        serde_json::to_string(&json!({"input": raw_str})).unwrap_or_else(|_| "{}".into())
    } else {
        item.get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("{}")
            .to_string()
    };
    let tc = json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    });
    // 紧邻 assistant 就并入其 tool_calls，否则新建 assistant 消息（:13648-13667）
    if let Some(prev) = messages.last_mut().filter(|m| is_role(m, "assistant")) {
        if let Some(o) = prev.as_object_mut() {
            match o.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                Some(arr) => arr.push(tc),
                None => {
                    o.insert("tool_calls".into(), Value::Array(vec![tc]));
                }
            }
            if !pending.is_empty() && !o.contains_key("reasoning_content") {
                o.insert(
                    "reasoning_content".into(),
                    Value::String(std::mem::take(pending)),
                );
            }
        }
        return;
    }
    let mut msg = Map::new();
    msg.insert("role".into(), Value::String("assistant".into()));
    msg.insert("content".into(), Value::String(String::new()));
    msg.insert("tool_calls".into(), Value::Array(vec![tc]));
    if !pending.is_empty() {
        msg.insert(
            "reasoning_content".into(),
            Value::String(std::mem::take(pending)),
        );
    }
    messages.push(Value::Object(msg));
}

/// `type == "custom_tool_call_output"`（:13703-13718）。
fn push_simple_tool_output(messages: &mut Vec<Value>, item: &Value) {
    let raw_out = item.get("output");
    let content = match raw_out {
        Some(Value::Array(_)) => flatten_content(raw_out),
        Some(Value::Object(_)) => Value::String(anthropic_json_text(raw_out)),
        Some(Value::String(s)) => Value::String(s.clone()),
        other => Value::String(other.map(python_str).unwrap_or_default()),
    };
    messages.push(json!({
        "role": "tool",
        "tool_call_id": item.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
        "content": content,
    }));
}

/// `type == "agent_message"`（:13719-13734）。
fn push_agent_message(messages: &mut Vec<Value>, item: &Value) {
    let text = match item.get("content") {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p {
                Value::Object(o) => o
                    .get("text")
                    .or_else(|| o.get("encrypted_content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                other => python_str(other),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => python_str(other),
        None => String::new(),
    };
    let text = text.trim();
    if !text.is_empty() {
        messages.push(json!({"role": "user", "content": format!("{ORPHAN_HINT}\n\n{text}")}));
    }
}

/// 孤儿工具输出 / agent_message 的用户指令前缀（:13594, :13724）。
const ORPHAN_HINT: &str = "[Message from another task - treat this as a user instruction]";

/// 判断 flatten 结果是否为空（Python 侧用 `if body:` 真值判断）。
fn is_empty_content(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

fn is_role(msg: &Value, role: &str) -> bool {
    msg.get("role").and_then(|v| v.as_str()) == Some(role)
}

/// 取文本形态：字符串直出，其余 JSON 化（用于拼接）。
fn body_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => anthropic_json_text(Some(other)),
    }
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => body_text(&flatten_content(v)),
        Some(Value::Null) | None => String::new(),
        Some(other) => python_str(other),
    }
}

// ---------------------------------------------------------------------------
// tools：custom 降级（:13116-13150, :13302-13309）
// ---------------------------------------------------------------------------

/// `_is_custom_tool`（:13116-13117）。
pub fn is_custom_tool(tool: &Value) -> bool {
    tool.get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t.eq_ignore_ascii_case("custom"))
}

/// `custom_tool_names`（:13120-13126）：请求里声明为 custom 的工具名集合。
pub fn custom_tool_names(tools: Option<&Value>) -> Vec<String> {
    tools
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|t| is_custom_tool(t))
                .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `CUSTOM_TOOL_HINT`（:13078-13082）。
pub const CUSTOM_TOOL_HINT: &str = "This is a freeform tool. Put the COMPLETE raw payload into \
the single 'input' string parameter, verbatim. Do not wrap it in JSON, do not wrap \
it in markdown code fences, do not add commentary.";

/// `_downgrade_custom_tool`（:13129-13150）：custom → 单参 `input` 的函数工具。
fn downgrade_custom_tool(tool: &Value) -> Value {
    let desc = tool
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let fmt = tool.get("format");
    // format.definition 存在时追加 Grammar 段（:13134-13135）
    let extra = match fmt.and_then(|f| f.get("definition")) {
        Some(def) => format!("\n\nGrammar:\n{}", python_str(def)),
        None => String::new(),
    };
    json!({
        "type": "function",
        "name": tool.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "description": format!("{desc}\n\n{CUSTOM_TOOL_HINT}{extra}").trim().to_string(),
        "parameters": {
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Complete raw payload for this tool, verbatim.",
                }
            },
            "required": ["input"],
        },
    })
}

/// `_tools_for_chat`（:13302-13309）：custom 工具降级为函数工具，其余不动，
/// 非 dict 项丢弃。M3 不含 namespace 展开（推迟 M3-b）。
pub fn tools_for_chat(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|t| t.is_object())
                .map(|t| {
                    if is_custom_tool(t) {
                        downgrade_custom_tool(t)
                    } else {
                        t.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Python 真值判断的近似：None/False/空串/空数组/空对象/0 为假。
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
    }
}

// ---------------------------------------------------------------------------
// responses_to_chat（:13741-13778）
// ---------------------------------------------------------------------------

/// `responses_to_chat`：Responses 请求体 → Chat Completions 请求体。
///
/// M3 不注入 web-tools、不展开 namespace（推迟 M3-b，见模块头注释）。
/// `catalog_default_max` 对应 Python 的 `model_default_max_output_tokens(model)`：
/// 定价/默认值表推迟 M3-b，M3 传 `None` 表示查不到。
pub fn responses_to_chat(payload: &Value, catalog_default_max: Option<i64>) -> Value {
    let mut chat = Map::new();
    chat.insert(
        "model".into(),
        payload.get("model").cloned().unwrap_or(Value::Null),
    );
    chat.insert(
        "messages".into(),
        Value::Array(responses_input_to_messages(payload)),
    );
    // 顶层透传 temperature / top_p / seed（:13745-13747）
    for key in ["temperature", "top_p", "seed"] {
        if let Some(v) = payload.get(key) {
            if !v.is_null() {
                chat.insert(key.into(), v.clone());
            }
        }
    }
    // max_output_tokens → max_tokens，缺失时用目录默认值（:13748-13753）
    match payload.get("max_output_tokens") {
        Some(v) if !v.is_null() => {
            chat.insert("max_tokens".into(), v.clone());
        }
        _ => {
            if let Some(default_max) = catalog_default_max {
                chat.insert("max_tokens".into(), json!(default_max));
            }
        }
    }
    // reasoning.effort 优先于顶层 reasoning_effort（:13754-13761）
    let nested_effort = payload
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let top_effort = payload
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    if let Some(effort) = nested_effort.or(top_effort) {
        chat.insert("reasoning_effort".into(), Value::String(effort.to_string()));
    }
    // tools：降级 custom，其余原样（:13762-13765，namespace 展开推迟 M3-b）
    if let Some(tools) = payload.get("tools").filter(|v| is_truthy(v)) {
        chat.insert("tools".into(), Value::Array(tools_for_chat(tools)));
    }
    if let Some(tc) = payload.get("tool_choice").filter(|v| is_truthy(v)) {
        chat.insert("tool_choice".into(), tc.clone());
    }
    if let Some(p) = payload.get("parallel_tool_calls") {
        if !p.is_null() {
            chat.insert("parallel_tool_calls".into(), p.clone());
        }
    }
    Value::Object(chat)
}

// ---------------------------------------------------------------------------
// _responses_usage（:14481-14495）
// ---------------------------------------------------------------------------

/// `_responses_usage`：空 usage → None；否则映射 token 字段。
pub fn responses_usage(u: Option<&Value>) -> Option<Value> {
    let u = u.filter(|v| is_truthy(v))?;
    let det = u
        .get("completion_tokens_details")
        .cloned()
        .unwrap_or(Value::Null);
    let pdet = u
        .get("prompt_tokens_details")
        .cloned()
        .unwrap_or(Value::Null);
    // cached_tokens 三级回退：
    // prompt_cache_hit_tokens → completion_tokens_details → prompt_tokens_details（:14489-14490）
    let cached = u
        .get("prompt_cache_hit_tokens")
        .filter(|v| is_truthy(v))
        .or_else(|| det.get("cached_tokens").filter(|v| is_truthy(v)))
        .or_else(|| pdet.get("cached_tokens").filter(|v| is_truthy(v)))
        .cloned()
        .unwrap_or(json!(0));
    Some(json!({
        "input_tokens": u.get("prompt_tokens").cloned().unwrap_or(json!(0)),
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": u.get("completion_tokens").cloned().unwrap_or(json!(0)),
        "output_tokens_details": {
            "reasoning_tokens": det.get("reasoning_tokens").cloned().unwrap_or(json!(0)),
        },
        "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
    }))
}

// ---------------------------------------------------------------------------
// chat_to_response（:14496-14589）
// ---------------------------------------------------------------------------

/// `chat_to_response` 的请求侧元信息（:14503-14506）——原样回显客户端宣告能力。
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    pub parallel_tool_calls: Option<Value>,
    pub tool_choice: Option<Value>,
    pub tools: Option<Value>,
}

/// `chat_to_response`：Chat Completions 响应体 → Responses 响应体。
///
/// M3 不做 namespace 还原（`:14577-14579` 的 `apply_namespace_to_calls`）、
/// 不做 DSML 兜底（`:14543-14556`）、不生成引用注释（`build_citations`）。
/// `created_at` 由调用方传入秒级时间戳，便于测试确定性。
pub fn chat_to_response(
    chat_obj: &Value,
    model: &str,
    custom_names: &[String],
    request_meta: &RequestMeta,
    created_at: i64,
) -> Value {
    let choice = chat_obj
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let msg = choice.get("message").cloned().unwrap_or_else(|| json!({}));
    let text = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let reasoning = msg
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut output: Vec<Value> = Vec::new();
    // reasoning → reasoning 项（:14514-14520）
    if !reasoning.is_empty() {
        output.push(json!({
            "id": new_id("rs_"),
            "type": "reasoning",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    // tool_calls → function_call / custom_tool_call（:14521-14542）
    if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in calls {
            let fn_obj = tc.get("function").cloned().unwrap_or_else(|| json!({}));
            let call_id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| new_id("call_"));
            let name = fn_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = fn_obj
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !name.is_empty() && custom_names.iter().any(|n| n == name) {
                output.push(json!({
                    "id": new_id("ctc_"),
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "input": unwrap_custom_input(args),
                }));
            } else {
                // arguments 缺省为 "{}"（:14541）
                let args = if args.is_empty() { "{}" } else { args };
                output.push(json!({
                    "id": new_id("fc_"),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": args,
                }));
            }
        }
    }
    // 有文本，或完全没有 output → 补 message 项（:14557-14565）
    if !text.is_empty() || output.is_empty() {
        let content = if text.is_empty() {
            json!([])
        } else {
            json!([{"type": "output_text", "text": text, "annotations": []}])
        };
        output.push(json!({
            "id": new_id("msg_"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": content,
        }));
    }
    let finish = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop");
    let mut obj = Map::new();
    obj.insert("id".into(), Value::String(new_id("resp_")));
    obj.insert("object".into(), Value::String("response".into()));
    obj.insert("created_at".into(), json!(created_at));
    // finish_reason == "length" → incomplete（:14571）
    obj.insert(
        "status".into(),
        Value::String(
            if finish == "length" {
                "incomplete"
            } else {
                "completed"
            }
            .into(),
        ),
    );
    obj.insert("model".into(), Value::String(model.to_string()));
    obj.insert("output".into(), Value::Array(output));
    obj.insert("output_text".into(), Value::String(text.to_string()));
    obj.insert("metadata".into(), json!({}));
    // 请求能力回显，默认 parallel_tool_calls=true / tool_choice="auto" / tools=[]（:14580-14583）
    obj.insert(
        "parallel_tool_calls".into(),
        request_meta
            .parallel_tool_calls
            .clone()
            .unwrap_or(json!(true)),
    );
    obj.insert(
        "tool_choice".into(),
        request_meta.tool_choice.clone().unwrap_or(json!("auto")),
    );
    obj.insert(
        "tools".into(),
        request_meta.tools.clone().unwrap_or(json!([])),
    );
    if let Some(u) = responses_usage(chat_obj.get("usage")) {
        obj.insert("usage".into(), u);
    }
    if finish == "length" {
        obj.insert(
            "incomplete_details".into(),
            json!({"reason": "max_output_tokens"}),
        );
    }
    Value::Object(obj)
}

/// `_unwrap_custom_input`（:13312-13328）：从 `{"input": "..."}` 参数里取回原始串。
pub fn unwrap_custom_input(args: &str) -> String {
    if args.is_empty() {
        // Python 对空串走 json.loads 失败路径 → 原样返回 ""；此处等价
        return String::new();
    }
    let Ok(parsed) = serde_json::from_str::<Value>(args) else {
        return args.to_string();
    };
    match parsed {
        Value::Object(o) => match o.get("input") {
            Some(Value::String(s)) => s.clone(),
            Some(v) if !v.is_null() => anthropic_json_text(Some(v)),
            _ => args.to_string(),
        },
        Value::String(s) => s,
        _ => args.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- flatten_content ----

    #[test]
    fn flatten_content_string_passthrough() {
        assert_eq!(flatten_content(Some(&json!("hi"))), json!("hi"));
        assert_eq!(flatten_content(None), json!(""));
    }

    #[test]
    fn flatten_content_joins_text_and_drops_empty_segments() {
        // :13057 空文本段被过滤，且用 \n 拼接
        let content = json!([
            {"type": "input_text", "text": "a"},
            {"type": "input_text", "text": ""},
            {"type": "output_text", "text": "b"},
        ]);
        assert_eq!(flatten_content(Some(&content)), json!("a\nb"));
    }

    #[test]
    fn flatten_content_keeps_parts_when_image_present() {
        let content = json!([
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "image_url": "http://x/i.png"},
        ]);
        let out = flatten_content(Some(&content));
        let arr = out.as_array().expect("multimodal keeps array");
        assert_eq!(arr[0], json!({"type": "text", "text": "look"}));
        assert_eq!(
            arr[1],
            json!({"type": "image_url", "image_url": {"url": "http://x/i.png"}})
        );
    }

    #[test]
    fn flatten_content_image_data_uri_fallback() {
        // :13050-13052 data 字段拼 data URI，mime 默认 image/png
        let content = json!([{"type": "input_image", "data": "QUJD"}]);
        let out = flatten_content(Some(&content));
        assert_eq!(
            out[0]["image_url"]["url"],
            json!("data:image/png;base64,QUJD")
        );
    }

    #[test]
    fn flatten_content_image_url_dict_form() {
        // :13048-13049 image_url 为 dict 时取其 url
        let content = json!([{"type": "image_url", "image_url": {"url": "http://y/a.png"}}]);
        let out = flatten_content(Some(&content));
        assert_eq!(out[0]["image_url"]["url"], json!("http://y/a.png"));
    }

    // ---- responses_input_to_messages ----

    #[test]
    fn input_string_becomes_user_message_after_system() {
        let payload = json!({"instructions": "be nice", "input": "hi"});
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs[0], json!({"role": "system", "content": "be nice"}));
        assert_eq!(msgs[1], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn blank_instructions_skipped() {
        // :13529 instructions.strip() 判空
        let payload = json!({"instructions": "   ", "input": "hi"});
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
    }

    #[test]
    fn developer_role_normalised_to_system() {
        // :13547-13548
        let payload = json!({
            "input": [{"type": "message", "role": "developer", "content": "d"}]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs[0], json!({"role": "system", "content": "d"}));
    }

    #[test]
    fn adjacent_assistant_text_merges_with_newline() {
        // :13549-13557 紧邻 assistant 文本合并，分隔符为 \n
        let payload = json!({
            "input": [
                {"type": "message", "role": "assistant", "content": "a"},
                {"type": "message", "role": "assistant", "content": "b"},
            ]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], json!("a\nb"));
    }

    #[test]
    fn reasoning_before_assistant_is_pending_then_attached() {
        // :13563-13565 pending_reasoning 附到随后的 assistant 消息
        let payload = json!({
            "input": [
                {"type": "reasoning", "summary": [{"text": "think"}]},
                {"type": "message", "role": "assistant", "content": "ans"},
            ]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["reasoning_content"], json!("think"));
        assert_eq!(msgs[0]["content"], json!("ans"));
    }

    #[test]
    fn function_call_merges_into_adjacent_assistant() {
        // :13648-13667 紧邻 assistant 并入 tool_calls
        let payload = json!({
            "input": [
                {"type": "message", "role": "assistant", "content": "ok"},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"x\":1}"},
                {"type": "function_call", "call_id": "c2", "name": "g", "arguments": "{}"},
            ]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs.len(), 1);
        let calls = msgs[0]["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], json!("c1"));
        assert_eq!(calls[0]["function"]["name"], json!("f"));
        assert_eq!(calls[1]["id"], json!("c2"));
    }

    #[test]
    fn function_call_without_adjacent_assistant_starts_new_message() {
        let payload = json!({
            "input": [
                {"type": "message", "role": "user", "content": "u"},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            ]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], json!("assistant"));
        assert_eq!(msgs[1]["content"], json!(""));
    }

    #[test]
    fn custom_tool_call_wraps_input_as_json() {
        // :13675-13682 custom 的 input 包成 {"input": "..."}
        let payload = json!({
            "input": [{"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch", "input": "raw"}]
        });
        let msgs = responses_input_to_messages(&payload);
        let args = msgs[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments is string");
        let parsed: Value = serde_json::from_str(args).expect("valid json");
        assert_eq!(parsed, json!({"input": "raw"}));
    }

    #[test]
    fn function_call_output_with_call_id_becomes_tool_message() {
        // :13634-13638
        let payload = json!({
            "input": [{"type": "function_call_output", "call_id": "c1", "output": "res"}]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(
            msgs[0],
            json!({"role": "tool", "tool_call_id": "c1", "content": "res"})
        );
    }

    #[test]
    fn orphan_function_call_output_wrapped_as_user_instruction() {
        // :13592-13622 无 call_id 时包装成用户指令
        let payload = json!({
            "input": [{"type": "function_call_output", "output": "orphan"}]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
        let content = msgs[0]["content"].as_str().expect("string content");
        assert!(content.starts_with(ORPHAN_HINT));
        assert!(content.contains("orphan"));
    }

    #[test]
    fn agent_message_wrapped_as_user_instruction() {
        // :13719-13734
        let payload = json!({
            "input": [{"type": "agent_message", "content": [{"text": "ping"}]}]
        });
        let msgs = responses_input_to_messages(&payload);
        assert_eq!(msgs[0]["role"], json!("user"));
        assert!(msgs[0]["content"].as_str().unwrap().contains("ping"));
    }

    #[test]
    fn unhandled_item_type_is_dropped() {
        let payload = json!({"input": [{"type": "totally_unknown", "x": 1}]});
        assert!(responses_input_to_messages(&payload).is_empty());
    }

    // ---- responses_to_chat ----

    #[test]
    fn responses_to_chat_maps_scalar_fields() {
        let payload = json!({
            "model": "m",
            "input": "hi",
            "temperature": 0.3,
            "top_p": 0.9,
            "seed": 7,
            "max_output_tokens": 128,
        });
        let chat = responses_to_chat(&payload, None);
        assert_eq!(chat["model"], json!("m"));
        assert_eq!(chat["temperature"], json!(0.3));
        assert_eq!(chat["top_p"], json!(0.9));
        assert_eq!(chat["seed"], json!(7));
        assert_eq!(chat["max_tokens"], json!(128));
    }

    #[test]
    fn responses_to_chat_uses_catalog_default_when_max_absent() {
        // :13750-13753 缺省时取目录默认值
        let payload = json!({"model": "m", "input": "hi"});
        let chat = responses_to_chat(&payload, Some(4096));
        assert_eq!(chat["max_tokens"], json!(4096));
        let chat_none = responses_to_chat(&payload, None);
        assert!(chat_none.get("max_tokens").is_none());
    }

    #[test]
    fn responses_to_chat_reasoning_nested_wins_over_toplevel() {
        // :13754-13761 reasoning.effort 优先
        let payload = json!({
            "input": "hi",
            "reasoning": {"effort": "high"},
            "reasoning_effort": "low",
        });
        let chat = responses_to_chat(&payload, None);
        assert_eq!(chat["reasoning_effort"], json!("high"));
    }

    #[test]
    fn responses_to_chat_reasoning_toplevel_fallback() {
        let payload = json!({"input": "hi", "reasoning_effort": "low"});
        let chat = responses_to_chat(&payload, None);
        assert_eq!(chat["reasoning_effort"], json!("low"));
    }

    #[test]
    fn responses_to_chat_downgrades_custom_tools_only() {
        // :13762-13765 tools 降级 custom；普通 function 原样
        let payload = json!({
            "input": "hi",
            "tools": [
                {"type": "function", "name": "keep"},
                {"type": "custom", "name": "apply_patch", "description": "d"},
            ],
        });
        let chat = responses_to_chat(&payload, None);
        let tools = chat["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0], json!({"type": "function", "name": "keep"}));
        assert_eq!(tools[1]["type"], json!("function"));
        assert_eq!(tools[1]["parameters"]["required"], json!(["input"]));
        assert!(tools[1]["description"]
            .as_str()
            .unwrap()
            .contains("freeform tool"));
    }

    #[test]
    fn responses_to_chat_passes_tool_choice_and_parallel() {
        let payload = json!({
            "input": "hi",
            "tool_choice": "auto",
            "parallel_tool_calls": false,
        });
        let chat = responses_to_chat(&payload, None);
        assert_eq!(chat["tool_choice"], json!("auto"));
        assert_eq!(chat["parallel_tool_calls"], json!(false));
    }

    #[test]
    fn custom_tool_names_collects_declared_names() {
        let tools = json!([
            {"type": "custom", "name": "a"},
            {"type": "function", "name": "b"},
            {"type": "Custom", "name": "c"},
        ]);
        let mut names = custom_tool_names(Some(&tools));
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
    }

    // ---- responses_usage ----

    #[test]
    fn responses_usage_maps_fields_and_cache_fallback_chain() {
        // :14489-14490 prompt_cache_hit_tokens 优先
        let u = json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "prompt_cache_hit_tokens": 3,
            "completion_tokens_details": {"reasoning_tokens": 2, "cached_tokens": 9},
        });
        let out = responses_usage(Some(&u)).expect("usage");
        assert_eq!(out["input_tokens"], json!(10));
        assert_eq!(out["input_tokens_details"]["cached_tokens"], json!(3));
        assert_eq!(out["output_tokens"], json!(5));
        assert_eq!(out["output_tokens_details"]["reasoning_tokens"], json!(2));
        assert_eq!(out["total_tokens"], json!(15));
    }

    #[test]
    fn responses_usage_falls_back_to_nested_cached() {
        let u = json!({
            "prompt_tokens": 1,
            "completion_tokens_details": {"cached_tokens": 4},
        });
        let out = responses_usage(Some(&u)).expect("usage");
        assert_eq!(out["input_tokens_details"]["cached_tokens"], json!(4));
    }

    #[test]
    fn responses_usage_none_when_absent_or_falsy() {
        assert!(responses_usage(None).is_none());
        assert!(responses_usage(Some(&json!({}))).is_none());
        assert!(responses_usage(Some(&Value::Null)).is_none());
    }

    // ---- chat_to_response ----

    #[test]
    fn chat_to_response_text_only_message() {
        let chat = json!({
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
        });
        let obj = chat_to_response(&chat, "m", &[], &RequestMeta::default(), 100);
        assert_eq!(obj["object"], json!("response"));
        assert_eq!(obj["status"], json!("completed"));
        assert_eq!(obj["model"], json!("m"));
        assert_eq!(obj["created_at"], json!(100));
        assert_eq!(obj["output_text"], json!("hello"));
        let out = obj["output"].as_array().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], json!("message"));
        assert_eq!(out[0]["content"][0]["type"], json!("output_text"));
        assert_eq!(out[0]["content"][0]["annotations"], json!([]));
        // 默认回显（:14581-14583）
        assert_eq!(obj["parallel_tool_calls"], json!(true));
        assert_eq!(obj["tool_choice"], json!("auto"));
        assert_eq!(obj["tools"], json!([]));
    }

    #[test]
    fn chat_to_response_emits_reasoning_item_first() {
        // :14514-14520 reasoning 先于其它 output 项
        let chat = json!({
            "choices": [{"message": {"content": "a", "reasoning_content": "r"}, "finish_reason": "stop"}],
        });
        let obj = chat_to_response(&chat, "m", &[], &RequestMeta::default(), 0);
        let out = obj["output"].as_array().unwrap();
        assert_eq!(out[0]["type"], json!("reasoning"));
        assert_eq!(out[0]["summary"][0]["text"], json!("r"));
        assert_eq!(out[1]["type"], json!("message"));
    }

    #[test]
    fn chat_to_response_custom_name_inflates_to_custom_tool_call() {
        // :14525-14533 声明为 custom 的工具名重膨胀
        let chat = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{"id": "c1", "function": {"name": "apply_patch",
                        "arguments": "{\"input\":\"raw\"}"}}],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let names = vec!["apply_patch".to_string()];
        let obj = chat_to_response(&chat, "m", &names, &RequestMeta::default(), 0);
        let out = obj["output"].as_array().unwrap();
        assert_eq!(out[0]["type"], json!("custom_tool_call"));
        assert_eq!(out[0]["input"], json!("raw"));
        assert_eq!(out[0]["call_id"], json!("c1"));
    }

    #[test]
    fn chat_to_response_plain_tool_call_stays_function_call() {
        let chat = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{\"x\":1}"}}],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let obj = chat_to_response(&chat, "m", &[], &RequestMeta::default(), 0);
        let out = obj["output"].as_array().unwrap();
        assert_eq!(out[0]["type"], json!("function_call"));
        assert_eq!(out[0]["arguments"], json!("{\"x\":1}"));
        // 有 tool_calls 时不补 message 项（:14557 条件）
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn chat_to_response_empty_arguments_defaults_to_braces() {
        // :14541 缺省 "{}"
        let chat = json!({
            "choices": [{"message": {"tool_calls": [{"id": "c1", "function": {"name": "f"}}]},
                         "finish_reason": "tool_calls"}],
        });
        let obj = chat_to_response(&chat, "m", &[], &RequestMeta::default(), 0);
        assert_eq!(obj["output"][0]["arguments"], json!("{}"));
    }

    #[test]
    fn chat_to_response_length_finish_marks_incomplete() {
        // :14571 + :14587-14588
        let chat = json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "length"}],
        });
        let obj = chat_to_response(&chat, "m", &[], &RequestMeta::default(), 0);
        assert_eq!(obj["status"], json!("incomplete"));
        assert_eq!(
            obj["incomplete_details"]["reason"],
            json!("max_output_tokens")
        );
    }

    #[test]
    fn chat_to_response_empty_output_still_yields_message_item() {
        // :14557 `text or not output` → 空 content 的 message 项
        let chat = json!({"choices": [{"message": {}}]});
        let obj = chat_to_response(&chat, "m", &[], &RequestMeta::default(), 0);
        let out = obj["output"].as_array().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], json!("message"));
        assert_eq!(out[0]["content"], json!([]));
    }

    #[test]
    fn chat_to_response_echoes_request_meta() {
        let chat = json!({"choices": [{"message": {"content": "x"}}]});
        let meta = RequestMeta {
            parallel_tool_calls: Some(json!(false)),
            tool_choice: Some(json!("required")),
            tools: Some(json!([{"type": "function", "name": "t"}])),
        };
        let obj = chat_to_response(&chat, "m", &[], &meta, 0);
        assert_eq!(obj["parallel_tool_calls"], json!(false));
        assert_eq!(obj["tool_choice"], json!("required"));
        assert_eq!(obj["tools"][0]["name"], json!("t"));
    }

    // ---- unwrap_custom_input ----

    #[test]
    fn unwrap_custom_input_pulls_string_field() {
        assert_eq!(unwrap_custom_input("{\"input\":\"raw\"}"), "raw");
    }

    #[test]
    fn unwrap_custom_input_non_json_returns_as_is() {
        assert_eq!(unwrap_custom_input("not json"), "not json");
    }

    #[test]
    fn unwrap_custom_input_json_string_returns_inner() {
        // :13326-13327 parsed 为字符串时返回其本身
        assert_eq!(unwrap_custom_input("\"inner\""), "inner");
    }

    #[test]
    fn unwrap_custom_input_empty_returns_empty() {
        assert_eq!(unwrap_custom_input(""), "");
    }
}
