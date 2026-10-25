//! Responses API（`/v1/responses`、`/responses`）↔ Chat Completions 的结构翻译层。
//!
//! 移植自 wb_proxy.py：
//! - `_flatten_content`（:13025-13057）
//! - `_responses_input_to_messages`（:13525-13738）
//! - `responses_to_chat`（:13741-13778）
//! - `_responses_usage`（:14481-14495）
//! - `chat_to_response`（:14496-14589）
//!
//! M3 范围（用户 2026-10-16 裁决「结构翻译进 M3，流式推 M3-b」）：结构翻译 +
//! 流式事件翻译。以下支线推迟，本模块不实现、也不假装实现：
//! - namespace 工具展开/还原（:13175-13300 的 `expand_namespace_tools` /
//!   `apply_namespace_to_calls`）——需 `_namespace_map` 贯穿请求与响应两侧；
//! - web-tools 注入与代跑（`wb_webtools.install_tool_defs`、`web_tools_active`）；
//! - DSML tool calls 兜底（:11205 `parse_dsml_tool_calls`）；
//! - 引用注释（`build_citations`）。
//!
//! custom（freeform）工具的降级与重膨胀**在 M3 范围内**：它只依赖请求自身的
//! `tools` 声明，无外部状态。
//!
//! **关卡 4.21（线 B）补齐的三条差距**（与 Python `wb_proxy.py` 逐条对齐）：
//! 1. `previous_response_id` 不再被静默丢弃 —— 路由层判 400（见 api 层）；
//! 2. 回显元数据（`tools` / `tool_choice` / `parallel_tool_calls`）从**原始**
//!    Responses 请求体取（`request_meta_from_payload`），不再是空默认值；
//! 3. 支持 `stream: true` —— 上游 chat SSE → Responses 事件帧
//!    （`ResponsesStreamTranslator` / `stream_responses_events`）。

use std::collections::{BTreeMap, HashMap};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::anthropic::{anthropic_json_text, new_id};
use crate::stream::{estimate_tokens, is_truncated_arguments, strip_data_prefix};

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
///
/// pub：api 层判 `previous_response_id` / `stream` 标志时要用同一套口径。
pub fn is_truthy(v: &Value) -> bool {
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
// namespace 工具展开 / 还原（Python expand_namespace_tools / apply_namespace_to_calls）
// ---------------------------------------------------------------------------

/// `NAMESPACE_MAX_DEPTH`：namespace 最大展开深度。
const NAMESPACE_MAX_DEPTH: usize = 4;

/// `_NS_SEP`：namespace 与子工具名之间的分隔符。
const NS_SEP: &str = "__";

/// `expand_namespace_tools`：把 namespace 展开成 flat function 清单，其余工具原样保留。
///
/// 返回 `(flat_tools, name_to_namespace)`：
/// - `flat_tools`：展开后的扁平工具列表（namespace 被拆成其子 function）；
/// - `name_to_namespace`：子工具名 → 所属 namespace 名的映射，供回程还原使用。
///
/// 规则（与 Python `expand_namespace_tools` 逐条对齐）：
/// - 子工具可能在 `tools` / `children` / `functions` 任一栏位；
/// - namespace 子工具常常没有 `type` 栏位，展开时补成上游认得的 flat function；
/// - custom / web_search 等非 function 项目原样留下；
/// - 同名只留第一个。
pub fn expand_namespace_tools(
    tools: impl AsRef<Value>,
    depth: impl Into<i64>,
) -> (Vec<Value>, HashMap<String, String>) {
    let tools = tools.as_ref();
    let depth = depth.into();
    let mut flat: Vec<Value> = Vec::new();
    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let max_depth = (depth.max(0) as usize) + NAMESPACE_MAX_DEPTH;
    // max(0, ...) 已由 depth.max(0) 保证非负

    fn collect(
        entry: &Value,
        depth: usize,
        ns_name: &str,
        max_depth: usize,
        flat: &mut Vec<Value>,
        mapping: &mut HashMap<String, String>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        if !entry.is_object() || depth > max_depth {
            return;
        }
        let obj = entry.as_object().unwrap();
        let etype = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        // namespace：递归展开子工具
        if etype == "namespace" {
            let subs = obj
                .get("tools")
                .or_else(|| obj.get("children"))
                .or_else(|| obj.get("functions"));
            let subs = subs.and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let child_ns = obj
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| ns_name.to_string());
            for sub in &subs {
                collect(sub, depth + 1, &child_ns, max_depth, flat, mapping, seen);
            }
            return;
        }

        // 在 namespace 内的 function 形子工具：展开成 flat function
        if !ns_name.is_empty() && (etype.is_empty() || etype == "function") {
            let fn_obj = obj.get("function").and_then(|v| v.as_object());
            let fn_map: Map<String, Value> = if let Some(f) = fn_obj {
                f.clone()
            } else {
                let mut m = Map::new();
                m.insert(
                    "name".into(),
                    obj.get("name").cloned().unwrap_or(Value::Null),
                );
                m.insert(
                    "description".into(),
                    obj.get("description").cloned().unwrap_or(json!("")),
                );
                let params = obj
                    .get("parameters")
                    .or_else(|| obj.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                m.insert("parameters".into(), params);
                m
            };
            let name = fn_map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() || seen.contains(&name) {
                return;
            }
            seen.insert(name.clone());
            mapping.insert(name.clone(), ns_name.to_string());
            let description = fn_map
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parameters = fn_map
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let mut flat_fn = Map::new();
            flat_fn.insert("type".into(), json!("function"));
            flat_fn.insert("name".into(), json!(name));
            flat_fn.insert("description".into(), json!(description));
            flat_fn.insert("parameters".into(), parameters);
            if fn_map.contains_key("strict") {
                flat_fn.insert(
                    "strict".into(),
                    fn_map.get("strict").cloned().unwrap_or(Value::Null),
                );
            }
            flat.push(Value::Object(flat_fn));
            return;
        }

        // 其余：保留原样，仅在 namespace 内登记映射
        let fn_obj = obj
            .get("function")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                fn_obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        if !name.is_empty() {
            if seen.contains(&name) {
                return;
            }
            seen.insert(name.clone());
            if !ns_name.is_empty() {
                mapping.insert(name, ns_name.to_string());
            }
        }
        flat.push(entry.clone());
    }

    let entries = tools.as_array().cloned().unwrap_or_default();
    for entry in &entries {
        collect(entry, 0, "", max_depth, &mut flat, &mut mapping, &mut seen);
    }
    // tools 可能不是数组（None/非数组）→ 视为空
    let _ = tools;
    (flat, mapping)
}

/// `resolve_namespaced_name`：把模型回传的名字解析回 (bare_name, namespace)。
pub fn resolve_namespaced_name(
    name: Option<&Value>,
    mapping: &HashMap<String, String>,
) -> (String, String) {
    let name = name.and_then(|v| v.as_str()).unwrap_or("").to_string();
    if name.is_empty() {
        return (name, String::new());
    }
    if let Some(ns) = mapping.get(&name) {
        return (name.clone(), ns.clone());
    }
    if name.contains("::") {
        let idx = name.find("::").unwrap();
        if idx > 0 {
            let tail = &name[idx + 2..];
            let head = &name[..idx];
            if let Some(ns) = mapping.get(tail) {
                return (tail.to_string(), ns.clone());
            }
            return (tail.to_string(), head.to_string());
        }
    }
    // ns__tool 精确比对
    for (tool, ns) in mapping.iter() {
        if name == format!("{ns}{NS_SEP}{tool}") {
            return (tool.clone(), ns.clone());
        }
    }
    (name, String::new())
}

/// `apply_namespace_to_calls`：替 Responses 的 function_call / custom_tool_call 补上 namespace。
///
/// 返回还原后的 calls 列表。
pub fn apply_namespace_to_calls(
    calls: impl Into<Vec<Value>>,
    name_to_namespace: impl AsRef<HashMap<String, String>>,
) -> Vec<Value> {
    let name_to_namespace = name_to_namespace.as_ref();
    let mut calls: Vec<Value> = calls.into();
    if name_to_namespace.is_empty() {
        return calls;
    }
    for item in calls.iter_mut() {
        let obj = match item.as_object_mut() {
            Some(o) => o,
            None => continue,
        };
        let typ = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ != "function_call" && typ != "custom_tool_call" {
            continue;
        }
        if obj
            .get("namespace")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        {
            continue;
        }
        let name_val = obj.get("name").cloned();
        let (bare, ns) = resolve_namespaced_name(name_val.as_ref(), name_to_namespace);
        if !ns.is_empty() {
            obj.insert("name".into(), Value::String(bare));
            obj.insert("namespace".into(), Value::String(ns));
        }
    }
    calls
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

/// `request_meta`（Python :17904-17908）：从**原始** Responses 请求体取客户端
/// 宣告的能力，原样回显在响应对象上。
///
/// ⚠️ 必须从**原始 payload** 取，不能从翻译后的 chat body 取：翻译会把
/// `tools` 降级（custom → 单参 `input` 的函数工具）、会丢掉 `stream`，回显
/// 出来就不是客户端发的东西了。
///
/// 口径与 Python 逐字对齐：
/// - `tools`：`payload.get("tools") or []` —— 缺省/空 ⇒ `[]`，否则**原样**
///   （custom 工具也保持 Responses 形状，不降级）；
/// - `tool_choice`：`payload.get("tool_choice", "auto")` —— 键缺省 ⇒ `None`
///   （`chat_to_response` 落成 `"auto"`），键存在但为 null ⇒ `Some(null)`
///   （回显 null，与 Python 一致）；
/// - `parallel_tool_calls`：`payload.get("parallel_tool_calls", True)` —— 同上口径。
pub fn request_meta_from_payload(payload: &Value) -> RequestMeta {
    RequestMeta {
        parallel_tool_calls: payload.get("parallel_tool_calls").cloned(),
        tool_choice: payload.get("tool_choice").cloned(),
        tools: Some(match payload.get("tools") {
            Some(tools) if is_truthy(tools) => tools.clone(),
            _ => json!([]),
        }),
    }
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
            json!([{"type": "output_text", "text": text, "annotations": build_citations(text, &[])}])
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

// ---------------------------------------------------------------------------
// build_citations（Python build_citations，:13388-13441）
// ---------------------------------------------------------------------------

/// `build_citations`：把模型实际引用到的来源转成 `url_citation` annotations。
///
/// 只标注真的有出现在工具输出里的网址 —— 模型自己编的连结不会被当成引用。
pub fn build_citations(response_text: &str, sources: &[Value]) -> Vec<Value> {
    let text = response_text;
    if text.is_empty() || sources.is_empty() {
        return Vec::new();
    }
    // by_url：url → source（setdefault 语义，先到先得）
    let mut by_url: HashMap<String, Value> = HashMap::new();
    for s in sources {
        if !s.is_object() {
            continue;
        }
        let url = s
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            continue;
        }
        by_url.entry(url.clone()).or_insert_with(|| s.clone());
        by_url
            .entry(url.trim_end_matches('/').to_string())
            .or_insert_with(|| s.clone());
    }
    if by_url.is_empty() {
        return Vec::new();
    }

    // 字节偏移 → 字符（code point）偏移，对齐 Python 的 str 索引
    let byte_to_char = |byte: usize| -> usize { text[..byte].chars().count() };

    let md_re = Regex::new(r"\[([^\]\n]{1,200})\]\((https?://[^)\s]+)\)").unwrap();
    let url_re = Regex::new(r"https?://[^\s<>()\[\]]+").unwrap();

    let mut anns: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<(String, usize, usize)> =
        std::collections::HashSet::new();
    let mut md_spans: Vec<(usize, usize)> = Vec::new();

    let mut add = |url: String, title: Option<String>, start: usize, end: usize| {
        if end <= start {
            return;
        }
        let key = (url.clone(), start, end);
        if seen.contains(&key) {
            return;
        }
        seen.insert(key);
        let title = match title {
            Some(t) if !t.is_empty() => t,
            _ => url.clone(),
        };
        anns.push(json!({
            "type": "url_citation",
            "url": url,
            "title": title,
            "start_index": start,
            "end_index": end,
        }));
    };

    // 1. Markdown 链接 [title](url)
    for caps in md_re.captures_iter(text) {
        let m = caps.get(0).unwrap();
        let group1 = caps.get(1).map(|g| g.as_str()).unwrap_or("");
        let url_raw = caps.get(2).map(|g| g.as_str()).unwrap_or("");
        let src = by_url
            .get(url_raw)
            .or_else(|| by_url.get(url_raw.trim_end_matches('/')))
            .cloned();
        if src.is_none() {
            continue;
        }
        let src = src.unwrap();
        let start = byte_to_char(m.start());
        let end = byte_to_char(m.end());
        md_spans.push((start, end));
        let src_title = src
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Python: src.get("title") or group1
        let title = src_title
            .filter(|t| !t.is_empty())
            .or_else(|| Some(group1.to_string()));
        add(url_raw.to_string(), title, start, end);
    }

    // 2. 裸 URL
    for m in url_re.find_iter(text) {
        let start = byte_to_char(m.start());
        let end_full = byte_to_char(m.end());
        // 是否落在某个 md span 内
        let in_md = md_spans.iter().any(|(s, e)| start >= *s && end_full <= *e);
        if in_md {
            continue;
        }
        let raw = m.as_str();
        // rstrip(".,;:!?")
        let url = raw
            .trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'))
            .to_string();

        let src = by_url
            .get(&url)
            .or_else(|| by_url.get(url.trim_end_matches('/')))
            .cloned();
        if src.is_none() {
            continue;
        }
        let src = src.unwrap();
        let src_title = src
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let end = start + url.chars().count();
        // start 已对应 raw 起点；url 可能比 raw 短，end 按 url 长度
        add(url, src_title, start, end);
    }

    anns.sort_by(|a, b| {
        let sa = a.get("start_index").and_then(|v| v.as_i64()).unwrap_or(0);
        let sb = b.get("start_index").and_then(|v| v.as_i64()).unwrap_or(0);
        if sa != sb {
            return sa.cmp(&sb);
        }
        let ea = a.get("end_index").and_then(|v| v.as_i64()).unwrap_or(0);
        let eb = b.get("end_index").and_then(|v| v.as_i64()).unwrap_or(0);
        ea.cmp(&eb)
    });
    anns
}

// ---------------------------------------------------------------------------
// 流式：stream_responses_events（:14590-15085）
// ---------------------------------------------------------------------------

/// 一次流式工具调用（Python `tool_calls_map` 的 entry）。
#[derive(Debug, Clone)]
struct ToolEntry {
    /// 在 `outputs` 里的下标（= Responses 的 `output_index`）。
    output_index: usize,
    /// 上游给的 tool call id（回显为 `call_id`）。
    call_id: String,
    /// 工具名。
    name: String,
    /// 累积的 arguments（上游分片，字符串拼接）。
    arguments: String,
    /// 是否是本请求里声明为 custom（freeform）的工具。
    custom: bool,
    /// 本地生成的 item id（`fc_` / `ctc_` 前缀）。
    item_id: String,
}

/// `stream_responses_events`（:14590-15085）的状态机。
///
/// 上游 chat SSE 逐行喂进 [`ResponsesStreamTranslator::feed`]，产出 Responses
/// 事件（`Value`，含 `type` + `sequence_number`）；流结束时调
/// [`ResponsesStreamTranslator::finish`] 取收尾帧（含 `response.completed`）。
///
/// **本移植覆盖的事件**（与 Python 逐字对齐的字段名与顺序）：
/// - 生命周期：`response.created` / `response.in_progress` / `response.completed`；
/// - 文本：`response.output_item.added` / `response.content_part.added` /
///   `response.output_text.delta`，收尾 `response.output_text.done` /
///   `response.content_part.done` / `response.output_item.done`；
/// - 推理：`response.reasoning_summary_part.added` /
///   `response.reasoning_summary_text.delta`，收尾
///   `response.reasoning_summary_text.done` /
///   `response.reasoning_summary_part.done` / `response.output_item.done`；
/// - 工具调用：`response.output_item.added` +
///   `response.function_call_arguments.delta`（custom 工具则是
///   `response.custom_tool_call_input.delta`），收尾
///   `response.function_call_arguments.done`（custom 则是
///   `response.custom_tool_call_input.done`）+ `response.output_item.done`。
///
/// **不在本移植范围**（与本关卡的账本一致，**不假装实现**）：
/// - namespace 工具展开/还原（`expand_namespace_tools` / `apply_namespace_to_calls`）；
/// - DSML tool calls 兜底（`parse_dsml_tool_calls`）⇒ 文本**原样**下发，
///   模型把调用打印在正文里时不会被还原成 `function_call`；
/// - 引用注释（`build_citations`）⇒ `annotations` 恒为 `[]`；
/// - web-tools 代跑（`wb_webtools`）⇒ 上游的 web_search / web_fetch 呼叫
///   **原样**转成 `function_call` 交给客户端，网关不主动出网。
pub struct ResponsesStreamTranslator {
    resp_id: String,
    msg_id: String,
    rs_id: String,
    created_at: i64,
    seq: i64,
    model: String,
    meta: RequestMeta,
    custom_names: Vec<String>,
    text_parts: Vec<String>,
    reason_parts: Vec<String>,
    /// 输出项列表；`None` = 占位（还没填内容）。
    outputs: Vec<Option<Value>>,
    reason_index: Option<usize>,
    msg_index: Option<usize>,
    finish: String,
    usage: Option<Value>,
    saw_done: bool,
    tool_calls: BTreeMap<i64, ToolEntry>,
    lifecycle_sent: bool,
}

impl ResponsesStreamTranslator {
    /// 建一个翻译器。`created_at` 是秒级时间戳（由调用方注入，便于测试固定）。
    pub fn new(
        model: &str,
        request_meta: &RequestMeta,
        custom_names: &[String],
        created_at: i64,
    ) -> Self {
        Self {
            resp_id: new_id("resp_"),
            msg_id: new_id("msg_"),
            rs_id: new_id("rs_"),
            created_at,
            seq: 0,
            model: model.to_string(),
            meta: request_meta.clone(),
            custom_names: custom_names.to_vec(),
            text_parts: Vec::new(),
            reason_parts: Vec::new(),
            outputs: Vec::new(),
            reason_index: None,
            msg_index: None,
            finish: "stop".into(),
            usage: None,
            saw_done: false,
            tool_calls: BTreeMap::new(),
            lifecycle_sent: false,
        }
    }

    /// 上游 usage（chat 形状），供 api 层记账。
    pub fn usage(&self) -> Option<Value> {
        self.usage.clone()
    }

    /// 造一个事件（Python `ev()` :14637-14641）：`type` + `sequence_number` +
    /// 负载。序号从 1 开始（Python 是 `seq += 1` 后再用）。
    fn ev(&mut self, etype: &str, payload: Value) -> Value {
        self.seq += 1;
        let mut data = Map::new();
        data.insert("type".into(), Value::String(etype.to_string()));
        data.insert("sequence_number".into(), Value::Number(self.seq.into()));
        if let Value::Object(p) = payload {
            for (k, v) in p {
                data.insert(k, v);
            }
        }
        Value::Object(data)
    }

    /// 响应对象（Python `resp_obj()` :14612-14633）。
    fn resp_obj(&self, status: &str) -> Value {
        let mut obj = Map::new();
        obj.insert("id".into(), Value::String(self.resp_id.clone()));
        obj.insert("object".into(), Value::String("response".into()));
        obj.insert("created_at".into(), json!(self.created_at));
        obj.insert("status".into(), Value::String(status.to_string()));
        obj.insert("model".into(), Value::String(self.model.clone()));
        obj.insert(
            "output".into(),
            Value::Array(self.outputs.iter().filter_map(|o| o.clone()).collect()),
        );
        obj.insert(
            "output_text".into(),
            Value::String(self.text_parts.join("")),
        );
        obj.insert(
            "parallel_tool_calls".into(),
            self.meta.parallel_tool_calls.clone().unwrap_or(json!(true)),
        );
        obj.insert(
            "tool_choice".into(),
            self.meta.tool_choice.clone().unwrap_or(json!("auto")),
        );
        obj.insert("tools".into(), self.meta.tools.clone().unwrap_or(json!([])));
        obj.insert("metadata".into(), json!({}));
        if let Some(u) = responses_usage(self.usage.as_ref()) {
            obj.insert("usage".into(), u);
        }
        Value::Object(obj)
    }

    /// 推理项（Python `reason_item()` :14644-14649）。
    fn reason_item(&self, status: &str) -> Value {
        json!({
            "id": self.rs_id,
            "type": "reasoning",
            "status": status,
            "summary": [{"type": "summary_text", "text": self.reason_parts.join("")}],
        })
    }

    /// 消息项（Python `msg_item()` :14662-14668）。
    fn msg_item(&self, status: &str) -> Value {
        let content = if self.text_parts.is_empty() {
            json!([])
        } else {
            json!([{
                "type": "output_text",
                "text": self.text_parts.join(""),
                "annotations": [],
            }])
        };
        json!({
            "id": self.msg_id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": content,
        })
    }

    /// 输出项槽位是否还是空的（Python `outputs[i] is None` 的占位语义）。
    fn slot_empty(&self, idx: usize) -> bool {
        matches!(self.outputs.get(idx), Some(None))
    }

    /// 生命周期开场（Python :14925-14927）：只在**第一轮**送一次。
    fn ensure_lifecycle(&mut self, events: &mut Vec<Value>) {
        if self.lifecycle_sent {
            return;
        }
        self.lifecycle_sent = true;
        let created = self.resp_obj("in_progress");
        events.push(self.ev("response.created", json!({"response": created})));
        let in_progress = self.resp_obj("in_progress");
        events.push(self.ev("response.in_progress", json!({"response": in_progress})));
    }

    /// 喂一行上游 SSE（`data: {...}` 或 `[DONE]`），返回本行产出的事件。
    pub fn feed(&mut self, raw_line: &str) -> Vec<Value> {
        let data = strip_data_prefix(raw_line);
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            self.saw_done = true;
            return Vec::new();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        self.ensure_lifecycle(&mut events);
        // usage 在 choices 守卫之前捕获（Python :14938-14943）
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            let take = match &self.usage {
                None => true,
                Some(prev) => {
                    u.get("total_tokens").and_then(|t| t.as_i64()).unwrap_or(0)
                        >= prev
                            .get("total_tokens")
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0)
                }
            };
            if take {
                self.usage = Some(u.clone());
            }
        }
        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return events;
        };
        for choice in choices {
            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            self.feed_reasoning(&delta, &mut events);
            self.feed_tool_calls(&delta, &mut events);
            self.feed_content(&delta, &mut events);
            if let Some(fr) = choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                self.finish = fr.to_string();
            }
        }
        events
    }

    /// 推理增量（Python :14946-14962）。
    fn feed_reasoning(&mut self, delta: &Value, events: &mut Vec<Value>) {
        let Some(piece) = delta
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let rs_id = self.rs_id.clone();
        if self.reason_index.is_none() {
            let idx = self.outputs.len();
            self.outputs.push(None);
            self.reason_index = Some(idx);
            let item = self.reason_item("in_progress");
            events.push(self.ev(
                "response.output_item.added",
                json!({"output_index": idx, "item": item}),
            ));
            events.push(self.ev(
                "response.reasoning_summary_part.added",
                json!({
                    "item_id": rs_id,
                    "output_index": idx,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": ""},
                }),
            ));
        }
        self.reason_parts.push(piece.clone());
        let idx = self.reason_index.unwrap_or(0);
        events.push(self.ev(
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": rs_id,
                "output_index": idx,
                "summary_index": 0,
                "delta": piece,
            }),
        ));
    }

    /// 工具调用增量（Python :14963-15012）。
    fn feed_tool_calls(&mut self, delta: &Value, events: &mut Vec<Value>) {
        let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
            return;
        };
        for tc in calls {
            let idx = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
            let fn_obj = tc.get("function").cloned().unwrap_or(Value::Null);
            let fn_name = fn_obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let fn_args = fn_obj
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let call_id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !self.tool_calls.contains_key(&idx) {
                // 首片：开一个新输出项（Python :14970-14998）
                let out_idx = self.outputs.len();
                self.outputs.push(None);
                let c_id = if call_id.is_empty() {
                    new_id("call_")
                } else {
                    call_id.clone()
                };
                let is_custom =
                    !fn_name.is_empty() && self.custom_names.iter().any(|n| n == &fn_name);
                let item_id = new_id(if is_custom { "ctc_" } else { "fc_" });
                self.tool_calls.insert(
                    idx,
                    ToolEntry {
                        output_index: out_idx,
                        call_id: c_id.clone(),
                        name: fn_name.clone(),
                        arguments: fn_args.clone(),
                        custom: is_custom,
                        item_id: item_id.clone(),
                    },
                );
                let mut item = Map::new();
                item.insert("id".into(), Value::String(item_id));
                item.insert("status".into(), Value::String("in_progress".into()));
                item.insert("call_id".into(), Value::String(c_id));
                item.insert("name".into(), Value::String(fn_name));
                if is_custom {
                    item.insert("type".into(), Value::String("custom_tool_call".into()));
                    item.insert("input".into(), Value::String(String::new()));
                } else {
                    item.insert("type".into(), Value::String("function_call".into()));
                    item.insert("arguments".into(), Value::String(String::new()));
                }
                events.push(self.ev(
                    "response.output_item.added",
                    json!({"output_index": out_idx, "item": Value::Object(item)}),
                ));
                // ⚠️ Python 首片的 arguments **不**发 delta 事件（直接进 entry），
                // 只有后续分片才发 —— 这里照抄（:14970-14998）。
                continue;
            }
            let is_custom_name = self.custom_names.iter().any(|n| n == &fn_name);
            // 借用 `self.tool_calls` 的可变引用期间不能调 `self.ev`（要 &mut self），
            // 故先把事件载荷攒出来，出了借用再发。
            let mut delta_event: Option<(String, Value)> = None;
            {
                let Some(entry) = self.tool_calls.get_mut(&idx) else {
                    continue;
                };
                if !fn_name.is_empty() && entry.name.is_empty() {
                    entry.name = fn_name.clone();
                    if is_custom_name {
                        entry.custom = true;
                    }
                }
                if !fn_args.is_empty() {
                    entry.arguments.push_str(&fn_args);
                    let etype = if entry.custom {
                        "response.custom_tool_call_input.delta"
                    } else {
                        "response.function_call_arguments.delta"
                    };
                    delta_event = Some((
                        etype.to_string(),
                        json!({
                            "output_index": entry.output_index,
                            "item_id": entry.item_id,
                            "call_id": entry.call_id,
                            "delta": fn_args,
                        }),
                    ));
                }
            }
            if let Some((etype, payload)) = delta_event {
                events.push(self.ev(&etype, payload));
            }
        }
    }

    /// 文本增量（Python :15013-15075）。
    ///
    /// ⚠️ Python 这里有一段 DSML 缓冲（`text_buffer`，:15035-15071）用来从正文里
    /// 捞工具调用；DSML 兜底不在本关卡范围，故文本**原样**逐片下发。
    fn feed_content(&mut self, delta: &Value, events: &mut Vec<Value>) {
        let Some(piece) = delta
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let msg_id = self.msg_id.clone();
        if self.msg_index.is_none() {
            // 推理项还开着就先收尾（Python :15015-15026）
            if let Some(ridx) = self.reason_index {
                if self.slot_empty(ridx) {
                    self.close_reasoning(ridx, events);
                }
            }
            let idx = self.outputs.len();
            self.outputs.push(None);
            self.msg_index = Some(idx);
            events.push(self.ev(
                "response.output_item.added",
                json!({
                    "output_index": idx,
                    "item": {
                        "id": msg_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    },
                }),
            ));
            events.push(self.ev(
                "response.content_part.added",
                json!({
                    "item_id": msg_id,
                    "output_index": idx,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            ));
        }
        self.text_parts.push(piece.clone());
        let idx = self.msg_index.unwrap_or(0);
        events.push(self.ev(
            "response.output_text.delta",
            json!({
                "item_id": msg_id,
                "output_index": idx,
                "content_index": 0,
                "delta": piece,
            }),
        ));
    }

    /// 推理项收尾（Python :14686-14697 与 :15018-15026 两处同款）。
    fn close_reasoning(&mut self, ridx: usize, events: &mut Vec<Value>) {
        let rs_id = self.rs_id.clone();
        let full_r = self.reason_parts.join("");
        events.push(self.ev(
            "response.reasoning_summary_text.done",
            json!({
                "item_id": rs_id,
                "output_index": ridx,
                "summary_index": 0,
                "text": full_r,
            }),
        ));
        events.push(self.ev(
            "response.reasoning_summary_part.done",
            json!({
                "item_id": rs_id,
                "output_index": ridx,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": full_r},
            }),
        ));
        let item = self.reason_item("completed");
        if let Some(slot) = self.outputs.get_mut(ridx) {
            *slot = Some(item.clone());
        }
        events.push(self.ev(
            "response.output_item.done",
            json!({"output_index": ridx, "item": item}),
        ));
    }

    /// 流结束：收尾帧 + `response.completed`（Python `_finalize()` :14678-14905）。
    pub fn finish(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        self.ensure_lifecycle(&mut events);
        // 截断的工具调用不收尾（Python :15045-15052）：arguments 是半截 JSON，
        // 收尾会让客户端拿到一个永远解析不了的调用。
        let mut dropped_truncated = false;
        if !self.tool_calls.is_empty() && (self.finish == "length" || !self.saw_done) {
            let keys: Vec<i64> = self.tool_calls.keys().copied().collect();
            for idx in keys {
                let truncated = self
                    .tool_calls
                    .get(&idx)
                    .is_some_and(|e| is_truncated_arguments(&e.arguments));
                if truncated {
                    self.tool_calls.remove(&idx);
                    dropped_truncated = true;
                }
            }
        }
        // 1. 推理项收尾（Python :14680-14697）
        if let Some(ridx) = self.reason_index {
            if self.slot_empty(ridx) {
                self.close_reasoning(ridx, &mut events);
            }
        }
        // 2. 工具调用收尾（Python :14698-14720）
        let keys: Vec<i64> = self.tool_calls.keys().copied().collect();
        for idx in keys {
            let Some(entry) = self.tool_calls.get(&idx).cloned() else {
                continue;
            };
            let mut item = Map::new();
            item.insert("id".into(), Value::String(entry.item_id.clone()));
            item.insert(
                "type".into(),
                Value::String(
                    if entry.custom {
                        "custom_tool_call"
                    } else {
                        "function_call"
                    }
                    .to_string(),
                ),
            );
            item.insert("status".into(), Value::String("completed".into()));
            item.insert("call_id".into(), Value::String(entry.call_id.clone()));
            item.insert("name".into(), Value::String(entry.name.clone()));
            if entry.custom {
                let input = unwrap_custom_input(&entry.arguments);
                events.push(self.ev(
                    "response.custom_tool_call_input.done",
                    json!({
                        "output_index": entry.output_index,
                        "item_id": entry.item_id,
                        "call_id": entry.call_id,
                        "input": input,
                    }),
                ));
                item.insert("input".into(), Value::String(input));
            } else {
                events.push(self.ev(
                    "response.function_call_arguments.done",
                    json!({
                        "output_index": entry.output_index,
                        "item_id": entry.item_id,
                        "call_id": entry.call_id,
                        "arguments": entry.arguments,
                    }),
                ));
                item.insert("arguments".into(), Value::String(entry.arguments.clone()));
            }
            let item = Value::Object(item);
            if let Some(slot) = self.outputs.get_mut(entry.output_index) {
                *slot = Some(item.clone());
            }
            events.push(self.ev(
                "response.output_item.done",
                json!({"output_index": entry.output_index, "item": item}),
            ));
        }
        // 3. 消息项收尾（Python :14818-14845）
        let full_text = self.text_parts.join("");
        let has_other_items = self.outputs.iter().any(|o| o.is_some());
        if self.msg_index.is_some() || !full_text.is_empty() || !has_other_items {
            let msg_id = self.msg_id.clone();
            let idx = match self.msg_index {
                Some(i) => i,
                None => {
                    let i = self.outputs.len();
                    self.outputs.push(None);
                    self.msg_index = Some(i);
                    events.push(self.ev(
                        "response.output_item.added",
                        json!({
                            "output_index": i,
                            "item": {
                                "id": msg_id,
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": [],
                            },
                        }),
                    ));
                    events.push(self.ev(
                        "response.content_part.added",
                        json!({
                            "item_id": msg_id,
                            "output_index": i,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        }),
                    ));
                    i
                }
            };
            events.push(self.ev(
                "response.output_text.done",
                json!({
                    "item_id": msg_id,
                    "output_index": idx,
                    "content_index": 0,
                    "text": full_text,
                }),
            ));
            events.push(self.ev(
                "response.content_part.done",
                json!({
                    "item_id": msg_id,
                    "output_index": idx,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": full_text, "annotations": []},
                }),
            ));
            let item = self.msg_item("completed");
            if let Some(slot) = self.outputs.get_mut(idx) {
                *slot = Some(item.clone());
            }
            events.push(self.ev(
                "response.output_item.done",
                json!({"output_index": idx, "item": item}),
            ));
        }
        // 4. usage 兜底估算（Python :14848-14864）
        self.estimate_usage();
        // 5. 收口（Python :14865-14876）
        let status = if self.finish != "length" && !dropped_truncated {
            "completed"
        } else {
            "incomplete"
        };
        let mut final_obj = self.resp_obj(status);
        if self.finish == "length" {
            final_obj["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        events.push(self.ev("response.completed", json!({"response": final_obj})));
        events
    }

    /// usage 兜底估算（Python :14848-14864）。
    ///
    /// ⚠️ Python 的 `prompt_est` 是 `max(1, estimate_tokens(str(meta.get("input") or "")))`
    /// —— `request_meta` 里**没有** `input` 键，故 `str(None)` = `"None"`，
    /// `estimate_tokens("None")` = 1 ⇒ 恒为 1。这里照抄该口径。
    fn estimate_usage(&mut self) {
        let total = self
            .usage
            .as_ref()
            .and_then(|u| u.get("total_tokens").and_then(|t| t.as_i64()))
            .unwrap_or(0);
        if self.usage.is_some() && total != 0 {
            return;
        }
        let out_txt = self.text_parts.join("");
        let rs_txt = self.reason_parts.join("");
        if out_txt.is_empty() && rs_txt.is_empty() {
            return;
        }
        let comp = estimate_tokens(&out_txt) + estimate_tokens(&rs_txt);
        let prompt_est = 1;
        self.usage = Some(json!({
            "prompt_tokens": prompt_est,
            "completion_tokens": comp,
            "total_tokens": prompt_est + comp,
            "completion_tokens_details": {"reasoning_tokens": estimate_tokens(&rs_txt)},
            "prompt_tokens_details": {"cached_tokens": 0},
        }));
    }
}

/// `stream_responses_events`（Python :14590-15085）：把上游 chat SSE 分片翻译成
/// Responses 事件序列。**纯函数**：输入分片 ⇒ 输出事件序列（便于单测）。
///
/// `lines` 是上游 SSE 的原始行（`data: {...}` / `[DONE]`）；每个事件是
/// `{"type": ..., "sequence_number": ..., ...负载}`。
pub fn stream_responses_events(
    lines: &[&str],
    model: &str,
    request_meta: &RequestMeta,
    custom_names: &[String],
    created_at: i64,
) -> Vec<Value> {
    let mut translator =
        ResponsesStreamTranslator::new(model, request_meta, custom_names, created_at);
    let mut events = Vec::new();
    for line in lines {
        events.extend(translator.feed(line));
    }
    events.extend(translator.finish());
    events
}

/// Responses SSE 帧的字节渲染（Python `ev()` :14637-14641）：
/// `event: <type>\ndata: <json>\n\n`。
pub fn responses_sse_frame(event: &Value) -> Vec<u8> {
    let etype = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let raw = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    format!("event: {etype}\ndata: {raw}\n\n").into_bytes()
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

    // ---- request_meta_from_payload ----

    #[test]
    fn request_meta_takes_tools_choice_and_parallel_from_payload() {
        // :17904-17908 —— 从**原始** payload 取，原样回显
        let payload = json!({
            "input": "hi",
            "tools": [{"type": "custom", "name": "apply_patch"}],
            "tool_choice": "required",
            "parallel_tool_calls": false,
        });
        let meta = request_meta_from_payload(&payload);
        // tools 保持 Responses 原样（不降级成单参 input 的函数工具）
        assert_eq!(
            meta.tools,
            Some(json!([{"type": "custom", "name": "apply_patch"}]))
        );
        assert_eq!(meta.tool_choice, Some(json!("required")));
        assert_eq!(meta.parallel_tool_calls, Some(json!(false)));
    }

    #[test]
    fn request_meta_defaults_when_fields_absent() {
        // 键缺省 ⇒ None ⇒ chat_to_response 落成 auto / true / []
        let meta = request_meta_from_payload(&json!({"input": "hi"}));
        assert_eq!(meta.tools, Some(json!([])));
        assert_eq!(meta.tool_choice, None);
        assert_eq!(meta.parallel_tool_calls, None);
    }

    #[test]
    fn request_meta_keeps_explicit_null_tool_choice() {
        // Python `payload.get("tool_choice", "auto")`：键存在但为 null ⇒ 回显 null
        let meta = request_meta_from_payload(&json!({"tool_choice": null}));
        assert_eq!(meta.tool_choice, Some(Value::Null));
    }

    // ---- stream_responses_events ----

    /// 上游 chat SSE 的一行（`data: ` 前缀 + JSON）。
    fn sse_line(body: &str) -> String {
        format!("data: {body}")
    }

    fn text_stream_lines() -> Vec<String> {
        vec![
            sse_line(
                r#"{"id":"c1","model":"m","choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#,
            ),
            sse_line(
                r#"{"id":"c1","model":"m","choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#,
            ),
            sse_line(r#"{"id":"c1","model":"m","choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            "data: [DONE]".to_string(),
        ]
    }

    fn event_types(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn stream_text_emits_expected_event_order() {
        let lines = text_stream_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        assert_eq!(
            event_types(&events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // sequence_number 从 1 开始且逐个递增（Python `ev()` :14637-14641）
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev["sequence_number"], json!(i + 1));
        }
    }

    #[test]
    fn stream_text_deltas_concatenate_to_upstream_text() {
        let lines = text_stream_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        let text: String = events
            .iter()
            .filter(|e| e["type"] == json!("response.output_text.delta"))
            .map(|e| e["delta"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(text, "Hello");
        // 收尾帧的 text 是全文
        let done = events
            .iter()
            .find(|e| e["type"] == json!("response.output_text.done"))
            .expect("output_text.done");
        assert_eq!(done["text"], json!("Hello"));
        assert_eq!(done["content_index"], json!(0));
    }

    #[test]
    fn stream_completed_carries_response_object() {
        let lines = text_stream_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        let completed = events
            .iter()
            .find(|e| e["type"] == json!("response.completed"))
            .expect("response.completed");
        let resp = &completed["response"];
        assert_eq!(resp["object"], json!("response"));
        assert_eq!(resp["status"], json!("completed"));
        assert_eq!(resp["output_text"], json!("Hello"));
        assert_eq!(resp["created_at"], json!(1000));
        // output[] 里有一条 message 项
        let out = resp["output"].as_array().expect("output[]");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], json!("message"));
        assert_eq!(out[0]["content"][0]["text"], json!("Hello"));
    }

    #[test]
    fn stream_created_and_in_progress_carry_empty_response() {
        let lines = text_stream_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        for etype in ["response.created", "response.in_progress"] {
            let ev = events
                .iter()
                .find(|e| e["type"] == json!(etype))
                .expect(etype);
            assert_eq!(ev["response"]["status"], json!("in_progress"));
            assert_eq!(ev["response"]["output"], json!([]));
            assert_eq!(ev["response"]["output_text"], json!(""));
        }
    }

    #[test]
    fn stream_echoes_request_meta_on_response_object() {
        // 回显元数据必须来自原始 payload（差距②）
        let meta = request_meta_from_payload(&json!({
            "tools": [{"type": "function", "name": "t"}],
            "tool_choice": "required",
            "parallel_tool_calls": false,
        }));
        let lines = text_stream_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &meta, &[], 1000);
        let completed = events
            .iter()
            .find(|e| e["type"] == json!("response.completed"))
            .expect("response.completed");
        assert_eq!(completed["response"]["tools"][0]["name"], json!("t"));
        assert_eq!(completed["response"]["tool_choice"], json!("required"));
        assert_eq!(completed["response"]["parallel_tool_calls"], json!(false));
    }

    #[test]
    fn stream_tool_call_emits_function_call_lifecycle() {
        let lines = [
            sse_line(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"run","arguments":"{\"a\":"}}]},"finish_reason":null}]}"#,
            ),
            sse_line(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}"#,
            ),
            "data: [DONE]".to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        let types = event_types(&events);
        assert!(types.contains(&"response.output_item.added".to_string()));
        assert!(types.contains(&"response.function_call_arguments.delta".to_string()));
        assert!(types.contains(&"response.function_call_arguments.done".to_string()));
        // 收尾的 function_call 项：arguments 是拼好的完整 JSON
        let done = events
            .iter()
            .find(|e| e["type"] == json!("response.function_call_arguments.done"))
            .expect("arguments.done");
        assert_eq!(done["arguments"], json!("{\"a\":1}"));
        assert_eq!(done["call_id"], json!("call_1"));
        // output_item.done 带完整的 function_call 项
        let item_done = events
            .iter()
            .rev()
            .find(|e| e["type"] == json!("response.output_item.done"))
            .expect("output_item.done");
        assert_eq!(item_done["item"]["type"], json!("function_call"));
        assert_eq!(item_done["item"]["name"], json!("run"));
        assert_eq!(item_done["item"]["status"], json!("completed"));
    }

    #[test]
    fn stream_custom_tool_call_uses_custom_events() {
        // 请求里声明为 custom 的工具 ⇒ custom_tool_call 事件族
        let lines = [
            sse_line(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"raw\"}"}}]},"finish_reason":null}]}"#,
            ),
            "data: [DONE]".to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let names = vec!["apply_patch".to_string()];
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &names, 1000);
        let types = event_types(&events);
        assert!(types.contains(&"response.custom_tool_call_input.done".to_string()));
        assert!(!types.contains(&"response.function_call_arguments.done".to_string()));
        let done = events
            .iter()
            .find(|e| e["type"] == json!("response.custom_tool_call_input.done"))
            .expect("custom input done");
        // custom 的参数被解包回原始 input 串（:14578-14582）
        assert_eq!(done["input"], json!("raw"));
    }

    #[test]
    fn stream_reasoning_emits_summary_events() {
        let lines = [
            sse_line(
                r#"{"choices":[{"delta":{"reasoning_content":"think"},"finish_reason":null}]}"#,
            ),
            sse_line(r#"{"choices":[{"delta":{"content":"ans"},"finish_reason":"stop"}]}"#),
            "data: [DONE]".to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        let types = event_types(&events);
        assert!(types.contains(&"response.reasoning_summary_part.added".to_string()));
        assert!(types.contains(&"response.reasoning_summary_text.delta".to_string()));
        assert!(types.contains(&"response.reasoning_summary_text.done".to_string()));
        // 推理项排在 message 项前面（reasoning 先出现 ⇒ output_index 0）
        let added = events
            .iter()
            .find(|e| e["type"] == json!("response.output_item.added"))
            .expect("output_item.added");
        assert_eq!(added["output_index"], json!(0));
        assert_eq!(added["item"]["type"], json!("reasoning"));
    }

    #[test]
    fn stream_length_finish_marks_response_incomplete() {
        let lines = [
            sse_line(r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#),
            "data: [DONE]".to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let events = stream_responses_events(&refs, "m", &RequestMeta::default(), &[], 1000);
        let completed = events
            .iter()
            .find(|e| e["type"] == json!("response.completed"))
            .expect("response.completed");
        assert_eq!(completed["response"]["status"], json!("incomplete"));
        assert_eq!(
            completed["response"]["incomplete_details"]["reason"],
            json!("max_output_tokens")
        );
    }

    #[test]
    fn stream_empty_upstream_still_emits_lifecycle_and_completed() {
        let events = stream_responses_events(&[], "m", &RequestMeta::default(), &[], 1000);
        assert_eq!(
            event_types(&events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }

    #[test]
    fn responses_sse_frame_has_event_and_data_lines() {
        let event = json!({"type": "response.created", "sequence_number": 1});
        let frame = String::from_utf8(responses_sse_frame(&event)).expect("utf8");
        assert!(
            frame.starts_with("event: response.created\ndata: "),
            "got {frame}"
        );
        assert!(frame.ends_with("\n\n"), "got {frame}");
    }
}

// ---------------------------------------------------------------------------
// web-tools（wb_webtools）最小对齐：tool defs 注入 / 检测 / 代跑辅助
// ---------------------------------------------------------------------------

/// `WEB_SEARCH_NAME` / `WEB_FETCH_NAME`：反代代跑的两个工具名。
pub const WEB_SEARCH_NAME: &str = "web_search";
pub const WEB_FETCH_NAME: &str = "web_fetch";
pub const MAX_WEB_ROUNDS: usize = 3;

const SEARCH_DECL_TYPES: &[&str] = &[
    "web_search",
    "web_search_preview",
    "web_search_preview_2025_03_11",
    "web_search",
    "web_fetch",
];

/// `client_wants_web`：客户端宣告了哪些网路工具（伺服器端或 function 形状都算）。
pub fn client_wants_web(tools: &Value) -> Value {
    let mut search = false;
    let mut fetch = false;
    if let Some(arr) = tools.as_array() {
        for t in arr {
            if !t.is_object() {
                continue;
            }
            let ty = t
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let name = {
                let direct = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let nested = t
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !direct.is_empty() {
                    direct
                } else {
                    nested
                }
            }
            .trim()
            .to_lowercase();
            if [
                "web_search",
                "web_search_preview",
                "web_search_preview_2025_03_11",
                "web_search",
            ]
            .contains(&ty.as_str())
                || name == "web_search"
            {
                search = true;
            }
            if ty == "web_fetch" || name == "web_fetch" {
                fetch = true;
            }
        }
    }
    json!({"search": search, "fetch": fetch})
}

/// `web_search_tool_def`：给上游看的 function 形状。
pub fn web_search_tool_def() -> Value {
    json!({
        "type": "function",
        "name": WEB_SEARCH_NAME,
        "description": "Searches the web for real-time information and returns ranked results with titles, URLs and snippets.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query (at least 2 characters)."},
                "numResults": {"type": "number", "description": "How many results to return (1-10, default 5)."}
            },
            "required": ["query"]
        }
    })
}

/// `web_fetch_tool_def`。
pub fn web_fetch_tool_def() -> Value {
    json!({
        "type": "function",
        "name": WEB_FETCH_NAME,
        "description": "Fetches a URL and returns its readable text. Use startIndex to page through a long page.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Absolute http:// or https:// URL to fetch."},
                "startIndex": {"type": "number", "description": "Character offset to continue reading a long page."}
            },
            "required": ["url"]
        }
    })
}

/// `install_tool_defs`：把客户端的网路工具宣告换成我们的 function。
pub fn install_tool_defs(chat_tools: &Value, wants: &Value) -> Vec<Value> {
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    if wants.get("search").and_then(|v| v.as_bool()) == Some(true) {
        names.insert(WEB_SEARCH_NAME.to_string());
    }
    if wants.get("fetch").and_then(|v| v.as_bool()) == Some(true) {
        names.insert(WEB_FETCH_NAME.to_string());
    }

    let mut kept: Vec<Value> = Vec::new();
    if let Some(arr) = chat_tools.as_array() {
        for t in arr {
            if !t.is_object() {
                kept.push(t.clone());
                continue;
            }
            let type_name = t
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let mut name = t
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if name.is_empty() {
                if let Some(f) = t.get("function") {
                    name = f
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_lowercase();
                }
            }
            if names.contains(&name) || names.contains(&type_name) {
                continue;
            }
            kept.push(t.clone());
        }
    }

    if wants.get("search").and_then(|v| v.as_bool()) == Some(true) {
        kept.push(web_search_tool_def());
    }
    if wants.get("fetch").and_then(|v| v.as_bool()) == Some(true) {
        kept.push(web_fetch_tool_def());
    }
    kept
}

/// `is_internal_tool`。
pub fn is_internal_tool(name: &str) -> bool {
    matches!(name.trim(), "web_search" | "web_fetch")
}

/// `query_args`：从工具参数取出查询字串。
pub fn query_args(args: &Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let raw = obj
        .get("query")
        .or_else(|| obj.get("queries"))
        .or_else(|| obj.get("q"));
    match raw {
        None => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" or "),
        Some(other) => other.to_string(),
    }
}

/// `url_arg`。
pub fn url_arg(args: &Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    if let Some(Value::String(s)) = obj.get("url") {
        return s.trim().to_string();
    }
    if let Some(Value::Array(arr)) = obj.get("urls") {
        if let Some(Value::String(s)) = arr.first() {
            return s.trim().to_string();
        }
    }
    String::new()
}

/// `web_tools_active`：body 是否被注入网路工具。
pub fn web_tools_active(body: &Value) -> bool {
    body.get("_web_tools").and_then(|v| v.as_bool()) == Some(true)
}

/// `tool_name_of`。
pub fn tool_name_of(tool: &Value) -> String {
    if !tool.is_object() {
        return String::new();
    }
    if let Some(f) = tool.get("function") {
        if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
            return n.to_string();
        }
    }
    tool.get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// `internal_calls_from_chat`。
pub fn internal_calls_from_chat(chat_obj: &Value, web_tools: bool) -> Vec<Value> {
    if !web_tools {
        return Vec::new();
    }
    let message = chat_obj
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"));
    let mut out = Vec::new();
    if let Some(msg) = message {
        if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in calls {
                let f = match tc.get("function") {
                    Some(f) => f,
                    None => continue,
                };
                let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if is_internal_tool(name) {
                    let args = f.get("arguments").cloned().unwrap_or_else(|| json!("{}"));
                    out.push(json!({"name": name, "arguments": args}));
                }
            }
        }
    }
    out
}

/// 简易 URL 守卫：回 Some(error) 表示拒绝。
fn guard_url(url: &str) -> Option<String> {
    let lower = url.trim().to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Some("Error: web_fetch needs an http:// or https:// URL.".to_string());
    }
    // 提取 host
    let rest = lower
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if host.is_empty() || !host.contains('.') {
        return Some("Error: host not allowed.".to_string());
    }
    let h = host.trim_end_matches('.');
    if h == "localhost" || h == "127.0.0.1" || h.starts_with("10.") || h.starts_with("192.168.") {
        return Some("Error: host not allowed.".to_string());
    }
    None
}

/// `execute`：永不抛例外，永远回一句能喂回模型的字串。
pub fn execute(name: &str, args_raw: &Value) -> String {
    let args = match args_raw {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        _ => args_raw.clone(),
    };
    if !args.is_object() {
        // non-dict argument blob
        // still try query
        if name.trim() == WEB_SEARCH_NAME {
            // treat as empty
        }
        // fall through to handling below with empty object
    }
    let obj = args.as_object().cloned().unwrap_or_default();
    let args_val = Value::Object(obj);

    let name = name.trim();
    if name == WEB_SEARCH_NAME {
        let q = query_args(&args_val);
        if q.chars().count() < 2 {
            return format!(
                "Error: web_search needs a query of at least 2 characters; got {:?}.",
                q
            );
        }
        return "Error: web search is temporarily unavailable.".to_string();
    }
    if name == WEB_FETCH_NAME {
        let url = url_arg(&args_val);
        if let Some(err) = guard_url(&url) {
            return err;
        }
        return "Error: web fetch is temporarily unavailable.".to_string();
    }
    format!("Error: {} is not a tool this gateway runs.", name)
}

/// `search` / `fetch` 占位。
pub fn search(_query: &str, _num_results: Option<i64>) -> String {
    "Error: web search is temporarily unavailable.".to_string()
}

pub fn fetch(url: &str, _start_index: Option<i64>) -> String {
    if let Some(err) = guard_url(url) {
        return err;
    }
    "Error: web fetch is temporarily unavailable.".to_string()
}

pub fn sources_from_result(_result: &str) -> Vec<Value> {
    Vec::new()
}

pub fn max_web_rounds() -> usize {
    MAX_WEB_ROUNDS
}
