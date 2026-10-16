//! build_upstream_body 入站请求体变形主管线，移植自 wb_proxy.py
//! （:11803-11878）及其子函数。顺序不可颠倒（后步依赖前步产物）。
//!
//! 管线：① model 提取 → ② thinking_enabled 预解析 → ③ normalize_roles →
//! ④ apply_prompt_mode → ⑤ sanitize_messages → ⑥ backfill_reasoning_content →
//! ⑦ SYSTEM_PROMPT 注入 → ⑧ 私有键删除 + body 重建 → ⑨ tool-call 配对三连 →
//! ⑩ translate_max_completion_tokens → ⑪ normalize_tool_choice/tools/stop →
//! ⑫ max_tokens 目录默认值 → ⑬ deepseek thinking/effort 注入 →
//! ⑭ 强制 stream=true + stream_options。

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::prompt::{apply_prompt_mode, PromptConfig};
use crate::sanitize::sanitize_messages;

/// 管线输入：目录/配置驱动的可选项。model_default_max_tokens 来自模型目录
/// （M5 前为空则保守不注入）；degraded 为 403 降级窗口（Task 4 注入）。
#[derive(Debug, Clone)]
pub struct BodyOptions<'a> {
    pub model_default_max_tokens: Option<i64>,
    pub model_default_effort: Option<&'a str>,
    pub system_prompt: &'a str,
    pub prompt: PromptConfig,
    pub degraded: bool,
}

impl Default for BodyOptions<'_> {
    fn default() -> Self {
        Self {
            model_default_max_tokens: None,
            model_default_effort: None,
            system_prompt: "You are a helpful assistant.",
            prompt: PromptConfig::default(),
            degraded: false,
        }
    }
}

fn is_deepseek(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("deepseek")
}

/// ② thinking_enabled 预解析（wb_proxy.py:11804-11818）。仅 deepseek 生效。
fn thinking_enabled(payload: &Value, model: &str) -> bool {
    if !is_deepseek(model) {
        return false;
    }
    let thinking_type = match payload.get("thinking") {
        Some(Value::Object(o)) => o
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase(),
        _ => String::new(),
    };
    if thinking_type == "enabled" {
        return true;
    }
    if thinking_type == "disabled" {
        return false;
    }
    let effort = payload
        .get("reasoning_effort")
        .or_else(|| payload.get("reasoningEffort"))
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // deepseek 且无 thinking 字段 → type=""（≠"disabled"）且 effort 缺省（≠"none"）→ True
    effort != "none"
}

/// ③ developer → system（wb_proxy.py:10643-10659）：WorkBuddy 上游只认
/// system/user/assistant/tool，直传 developer 会 400 code 11128。
fn normalize_roles(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            if let Some(obj) = m.as_object() {
                if obj.get("role").and_then(|r| r.as_str()) == Some("developer") {
                    let mut clone = obj.clone();
                    clone.insert("role".into(), Value::String("system".into()));
                    return Value::Object(clone);
                }
            }
            m.clone()
        })
        .collect()
}

/// ⑥ 上游要求 thinking 开启时每条 assistant 都带 reasoning_content（否则
/// code 11155）。非 deepseek 原样；门控 = thinking_enabled || has_trace。
fn has_trace(messages: &[Value]) -> bool {
    messages.iter().any(|m| {
        let Some(obj) = m.as_object() else {
            return false;
        };
        if obj
            .get("reasoning")
            .and_then(|r| r.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        obj.contains_key("reasoning_content")
    })
}

fn backfill_reasoning_content(messages: &[Value], deepseek: bool, thinking_on: bool) -> Vec<Value> {
    if !deepseek {
        return messages.to_vec();
    }
    if !thinking_on && !has_trace(messages) {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|m| {
            let Some(obj) = m.as_object() else {
                return m.clone();
            };
            if obj.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                return m.clone();
            }
            let mut clone = obj.clone();
            let rc = match obj.get("reasoning_content") {
                Some(Value::String(s)) => s.clone(),
                _ => match obj.get("reasoning") {
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                },
            };
            clone.insert("reasoning_content".into(), Value::String(rc.clone()));
            // 镜像：仅当 reasoning 不是非空 str 时写 rc；空 rc 用单个空格占位
            // （上游 len(reasoning)>0 校验，空串不满足）。
            match obj.get("reasoning") {
                Some(Value::String(s)) if !s.is_empty() => {}
                _ => {
                    let val = if rc.is_empty() { " " } else { rc.as_str() };
                    clone.insert("reasoning".into(), Value::String(val.into()));
                }
            }
            Value::Object(clone)
        })
        .collect()
}

/// content 是否为「空」（None/""/[]；wb_proxy.py:10798-10811）。
fn empty_content(content: Option<&Value>) -> bool {
    match content {
        None => true,
        Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// 合并相邻 assistant 的 reasoning_content（wb_proxy.py:10814-10823）。
fn merge_reasoning_content(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    if let Some(Value::String(src_rc)) = src.get("reasoning_content") {
        if !src_rc.is_empty() {
            let merged = match dst.get("reasoning_content") {
                Some(Value::String(d)) if !d.is_empty() => format!("{d}\n{src_rc}"),
                _ => src_rc.clone(),
            };
            dst.insert("reasoning_content".into(), Value::String(merged));
        }
    }
}

fn non_empty_tool_calls(obj: &Map<String, Value>) -> bool {
    obj.get("tool_calls")
        .and_then(|tc| tc.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

/// ⑨a 合并相邻 tool-call 声明（wb_proxy.py:10826-10880）：
/// Form 1 = 后条带 calls 且 content 空 → 并到前条；Form 2 = 后条无 calls 且
/// content 非空 str、前条 content 空 → 内容并入前条。两式均合并 reasoning。
fn merge_adjacent_tool_calls(messages: &mut Vec<Value>) {
    if messages.len() < 2 {
        return;
    }
    let mut i = 1;
    while i < messages.len() {
        let Some(prev) = messages[i - 1].as_object().cloned() else {
            i += 1;
            continue;
        };
        if prev.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            i += 1;
            continue;
        }
        let Some(cur) = messages[i].as_object().cloned() else {
            i += 1;
            continue;
        };
        if cur.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            i += 1;
            continue;
        }
        let mut merged = false;
        if non_empty_tool_calls(&prev)
            && non_empty_tool_calls(&cur)
            && empty_content(cur.get("content"))
        {
            // Form 1：tool_calls 拼接
            let mut new_prev = prev.clone();
            let mut calls = prev["tool_calls"].as_array().unwrap().clone();
            calls.extend(cur["tool_calls"].as_array().unwrap().clone());
            new_prev.insert("tool_calls".into(), Value::Array(calls));
            merge_reasoning_content(&mut new_prev, &cur);
            messages[i - 1] = Value::Object(new_prev);
            messages.remove(i);
            merged = true;
        } else if non_empty_tool_calls(&prev)
            && !cur.contains_key("tool_calls")
            && matches!(cur.get("content"), Some(Value::String(s)) if !s.is_empty())
            && empty_content(prev.get("content"))
        {
            // Form 2：content 并入
            let mut new_prev = prev.clone();
            new_prev.insert("content".into(), cur["content"].clone());
            merge_reasoning_content(&mut new_prev, &cur);
            messages[i - 1] = Value::Object(new_prev);
            messages.remove(i);
            merged = true;
        }
        if !merged {
            i += 1;
        }
    }
}

/// ⑨b 把散落在 tool 结果间的插入者重排到 batch 之后（wb_proxy.py:10883-10949）。
/// 每条约带 tool_calls 的 assistant：want = 其 tool_call id 集合；后续连续的
/// role=tool 且 id∈want 归 results；results 未开始遇非 tool → 停；results 开始
/// 后遇下一条带 tool_calls 的 assistant → 停（留作下个 batch）；其余 → between。
fn repack_tool_result_blocks(messages: &mut Vec<Value>) {
    if messages.len() < 3 {
        return;
    }
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    let n = messages.len();
    while i < n {
        let Some(obj) = messages[i].as_object() else {
            out.push(messages[i].clone());
            i += 1;
            continue;
        };
        let Some(calls) = obj.get("tool_calls").and_then(|tc| tc.as_array()) else {
            out.push(messages[i].clone());
            i += 1;
            continue;
        };
        if calls.is_empty() {
            out.push(messages[i].clone());
            i += 1;
            continue;
        }
        let want: HashSet<String> = calls
            .iter()
            .filter_map(|tc| {
                tc.get("id")
                    .and_then(|id| id.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .collect();
        let mut results: Vec<Value> = Vec::new();
        let mut between: Vec<Value> = Vec::new();
        let mut j = i + 1;
        let mut started = false;
        while j < n {
            let item = &messages[j];
            let is_tool = item.get("role").and_then(|r| r.as_str()) == Some("tool");
            let in_want = item
                .get("tool_call_id")
                .and_then(|t| t.as_str())
                .map(|id| want.contains(id))
                .unwrap_or(false);
            if is_tool && in_want {
                started = true;
                results.push(item.clone());
            } else if !started {
                break;
            } else {
                let next_calls_assistant = item.get("role").and_then(|r| r.as_str())
                    == Some("assistant")
                    && non_empty_tool_calls(item.as_object().unwrap());
                if next_calls_assistant {
                    break;
                }
                between.push(item.clone());
            }
            j += 1;
        }
        out.push(messages[i].clone());
        out.extend(results);
        out.extend(between);
        i = j;
    }
    *messages = out;
}

/// ⑨c 双侧同 id 集裁剪，不留半配对（wb_proxy.py:10952-11012）：
/// assistant.tool_calls 只留 id ∈ (call_ids ∩ result_ids)；role=tool 且
/// tool_call_id ∉ keep 的整条删除。
fn cleanup_orphan_tool_calls(messages: &mut Vec<Value>) {
    let mut call_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for m in messages.iter() {
        let Some(obj) = m.as_object() else {
            continue;
        };
        if let Some(calls) = obj.get("tool_calls").and_then(|tc| tc.as_array()) {
            for tc in calls {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    call_ids.push(id.to_string());
                }
            }
        }
        if let Some(rid) = obj.get("tool_call_id").and_then(|v| v.as_str()) {
            result_ids.push(rid.to_string());
        }
    }
    if call_ids.is_empty() && result_ids.is_empty() {
        return;
    }
    let keep: HashSet<&str> = call_ids
        .iter()
        .filter(|c| result_ids.contains(c))
        .map(|s| s.as_str())
        .collect();
    let mut out: Vec<Value> = Vec::new();
    for m in messages.iter() {
        let Some(obj) = m.as_object() else {
            out.push(m.clone());
            continue;
        };
        let role = obj.get("role").and_then(|r| r.as_str());
        if role == Some("tool") {
            let orphan = obj
                .get("tool_call_id")
                .and_then(|t| t.as_str())
                .map(|id| !keep.contains(id))
                .unwrap_or(true);
            if orphan {
                continue; // 整条删除
            }
        }
        let mut clone = obj.clone();
        if let Some(calls) = clone.get("tool_calls").and_then(|tc| tc.as_array()) {
            let kept: Vec<Value> = calls
                .iter()
                .filter(|tc| {
                    tc.get("id")
                        .and_then(|id| id.as_str())
                        .map(|id| keep.contains(id))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if kept.is_empty() {
                clone.remove("tool_calls");
            } else {
                clone.insert("tool_calls".into(), Value::Array(kept));
            }
        }
        out.push(Value::Object(clone));
    }
    *messages = out;
}

/// ⑩ max_completion_tokens 别名 → max_tokens（wb_proxy.py:11506-11517）：
/// 别名键始终移除；仅当无 max_tokens 且别名数值 >0 时写；无法解析则静默忽略。
fn translate_max_completion_tokens(body: &mut Map<String, Value>) {
    let alias = body.remove("max_completion_tokens");
    if body.contains_key("max_tokens") {
        return;
    }
    let Some(alias) = alias else { return };
    let parsed: Option<i64> = match &alias {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    if let Some(n) = parsed {
        if n > 0 {
            body.insert("max_tokens".into(), json!(n));
        }
    }
}

/// ⑪a tool_choice 归一化（wb_proxy.py:11109-11149）：上游只认字符串，对象会
/// 11101。str "none" 保持（不删 tools——删声明会让 Agent 把调用降级为文本空转）。
fn normalize_tool_choice(body: &mut Map<String, Value>) {
    let Some(choice) = body.get("tool_choice").cloned() else {
        return;
    };
    let new: Option<Value> = match &choice {
        Value::String(s) => {
            if s.trim().eq_ignore_ascii_case("none") {
                Some(Value::String("none".into()))
            } else {
                Some(choice)
            }
        }
        Value::Object(o) => {
            let t = o
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match t.as_str() {
                "none" => Some(Value::String("none".into())),
                "auto" => Some(Value::String("auto".into())),
                "required" => Some(Value::String("required".into())),
                "function" => {
                    let name = o
                        .get("function")
                        .and_then(|f| f.as_object())
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| o.get("name").and_then(|n| n.as_str()))
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| "auto".to_string());
                    Some(Value::String(name))
                }
                _ => None,
            }
        }
        _ => None,
    };
    match new {
        Some(v) => {
            body.insert("tool_choice".into(), v);
        }
        None => {
            body.remove("tool_choice");
        }
    }
}

/// ⑪b tools 归一化（wb_proxy.py:11150-11170）：`{"name","type":"function"}` 扁平
/// 形状包装为嵌套 function；非 dict 元素丢弃；tools 非 list 或空不动。
fn normalize_tools(body: &mut Map<String, Value>) {
    let Some(Value::Array(tools)) = body.get("tools").cloned() else {
        return;
    };
    if tools.is_empty() {
        return;
    }
    let mut out = Vec::new();
    for t in tools {
        let Some(obj) = t.as_object() else {
            continue;
        };
        if obj.get("name").is_some()
            && !obj.contains_key("function")
            && obj.get("type").and_then(|v| v.as_str()) == Some("function")
        {
            let mut fn_obj = Map::new();
            for key in ["name", "description", "parameters"] {
                if let Some(v) = obj.get(key) {
                    fn_obj.insert(key.into(), v.clone());
                }
            }
            if let Some(v) = obj.get("strict") {
                fn_obj.insert("strict".into(), v.clone());
            }
            let mut wrapped = Map::new();
            wrapped.insert("type".into(), Value::String("function".into()));
            wrapped.insert("function".into(), Value::Object(fn_obj));
            out.push(Value::Object(wrapped));
        } else {
            out.push(t.clone());
        }
    }
    body.insert("tools".into(), Value::Array(out));
}

/// ⑪c stop 归一化（wb_proxy.py:11174-11197）：上游 Go 端定死 []string，
/// 字符串直传 400 code 11101。str → [v] 若非空；list → 仅非空 str 元素；
/// 其余 → []；空则删键。
fn normalize_stop(body: &mut Map<String, Value>) {
    let Some(stop) = body.get("stop").cloned() else {
        return;
    };
    let mut vals: Vec<Value> = Vec::new();
    match &stop {
        Value::String(s) => {
            if !s.is_empty() {
                vals.push(Value::String(s.clone()));
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    if !s.is_empty() {
                        vals.push(Value::String(s.into()));
                    }
                }
            }
        }
        _ => {}
    }
    if vals.is_empty() {
        body.remove("stop");
    } else {
        body.insert("stop".into(), Value::Array(vals));
    }
}

/// ⑬ deepseek thinking/effort 注入（wb_proxy.py:11856-11874）：
/// `thinking.type=enabled` 单独不足以开推理轨迹，必须有 effort 值。
fn inject_deepseek(body: &mut Map<String, Value>, model: &str, opts: &BodyOptions) {
    if !is_deepseek(model) {
        return;
    }
    let opted_out = body
        .get("thinking")
        .and_then(|t| t.as_object())
        .map(|o| {
            o.get("type")
                .and_then(|v| v.as_str())
                .map(|t| t.trim().eq_ignore_ascii_case("disabled"))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let effort = body
        .get("reasoning_effort")
        .or_else(|| body.get("reasoningEffort"))
        .and_then(|e| e.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    if opted_out || effort.trim().eq_ignore_ascii_case("none") {
        return;
    }
    if !body.contains_key("thinking") {
        body.insert("thinking".into(), json!({"type": "enabled"}));
    }
    if effort.is_empty() {
        let default = opts
            .model_default_effort
            .filter(|s| !s.is_empty())
            .unwrap_or("high");
        body.insert("reasoning_effort".into(), Value::String(default.into()));
    }
}

/// 主管线。返回新对象（payload 快照 + 各步覆盖；tool_choice/tools 等嵌套
/// 键按契约改写，不保留对入参的别名）。
pub fn build_upstream_body(payload: &Value, opts: &BodyOptions) -> Value {
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let deepseek = is_deepseek(&model);
    let thinking_on = thinking_enabled(payload, &model);

    // ③④⑤⑥
    let mut messages = normalize_roles(
        payload
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]),
    );
    apply_prompt_mode(&mut messages, &opts.prompt, opts.degraded);
    messages = sanitize_messages(&messages);
    messages = backfill_reasoning_content(&messages, deepseek, thinking_on);

    // ⑦ SYSTEM_PROMPT 注入（首条非 system 或空表；发生在 backfill 之后，
    // 该 system 讯息不受 backfill 影响）
    if messages.is_empty() || messages[0].get("role").and_then(|r| r.as_str()) != Some("system") {
        let mut system = Map::new();
        system.insert("role".into(), Value::String("system".into()));
        system.insert("content".into(), Value::String(opts.system_prompt.into()));
        messages.insert(0, Value::Object(system));
    }

    // ⑧ 私有键删除 + body 重建（浅拷贝 payload）
    let mut body: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
    body.retain(|k, _| !k.starts_with('_'));
    body.insert("model".into(), Value::String(model.clone()));
    body.insert("messages".into(), Value::Array(messages.clone()));

    // ⑨ tool-call 配对三连（顺序有注释：先合并相邻声明，repack 才看得到完整 batch）
    merge_adjacent_tool_calls(&mut messages);
    repack_tool_result_blocks(&mut messages);
    cleanup_orphan_tool_calls(&mut messages);
    body.insert("messages".into(), Value::Array(messages));

    // ⑩⑪
    translate_max_completion_tokens(&mut body);
    normalize_tool_choice(&mut body);
    normalize_tools(&mut body);
    normalize_stop(&mut body);

    // ⑫ max_tokens 目录默认值（模型目录缺失则保守不注入）
    if !body.contains_key("max_tokens") {
        if let Some(n) = opts.model_default_max_tokens {
            if n > 0 {
                body.insert("max_tokens".into(), json!(n));
            }
        }
    }

    // ⑬⑭
    inject_deepseek(&mut body, &model, opts);
    body.insert("stream".into(), Value::Bool(true));
    if !body.contains_key("stream_options") {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }

    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BodyOptions<'static> {
        BodyOptions::default()
    }

    #[test]
    fn stream_is_forced_true_even_when_client_asks_false() {
        let payload = json!({"model": "gpt-4o", "stream": false, "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn client_stream_options_preserved() {
        let payload =
            json!({"model": "gpt-4o", "stream_options": {"include_usage": false}, "messages": []});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["stream_options"], json!({"include_usage": false}));
    }

    #[test]
    fn system_prompt_injected_when_first_is_user() {
        let payload = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are a helpful assistant.");
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn not_injected_when_first_is_system() {
        let payload = json!({"model": "m", "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn developer_role_becomes_system() {
        let payload = json!({"model": "m", "messages": [{"role": "developer", "content": "d"}, {"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system"); // developer→system，首条已是 system → 不注入
        assert_eq!(msgs[0]["content"], "d");
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn private_top_level_keys_removed() {
        let payload = json!({"_namespace": "x", "model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert!(!out.as_object().unwrap().contains_key("_namespace"));
        assert_eq!(out["model"], "m");
    }

    #[test]
    fn deepseek_bare_request_gets_thinking_and_effort() {
        let payload = json!({"model": "deepseek-v4.1-flash", "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["thinking"], json!({"type": "enabled"}));
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_backfills_assistant_reasoning() {
        let payload = json!({"model": "deepseek-r1", "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "ok", "reasoning_content": "think"},
        ]});
        let out = build_upstream_body(&payload, &opts());
        // system 注入前插一位：assistant 在 index 2
        let asst = out["messages"].as_array().unwrap()[2].clone();
        assert_eq!(asst["reasoning_content"], "think");
        assert_eq!(asst["reasoning"], "think"); // 镜像回填（原文无非空 reasoning）
    }

    #[test]
    fn deepseek_reasoning_mirror_placeholder() {
        let payload = json!({"model": "deepseek-r1", "messages": [
            {"role": "assistant", "content": "ok"},
        ]});
        let out = build_upstream_body(&payload, &opts());
        let asst = out["messages"].as_array().unwrap()[1].clone();
        assert_eq!(asst["reasoning_content"], "");
        assert_eq!(asst["reasoning"], " "); // 单个空格占位过上游校验
    }

    #[test]
    fn deepseek_disabled_thinking_no_injection() {
        let payload = json!({"model": "deepseek-r1", "thinking": {"type": "disabled"}, "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert!(out.as_object().unwrap().get("thinking").is_some()); // 客户端自带保留
        assert!(!out.as_object().unwrap().contains_key("reasoning_effort"));
    }

    #[test]
    fn max_completion_tokens_aliases_to_max_tokens() {
        let payload = json!({"model": "m", "max_completion_tokens": 128, "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["max_tokens"], 128);
        assert!(!out
            .as_object()
            .unwrap()
            .contains_key("max_completion_tokens"));
        // 已有 max_tokens 时别名被删但不覆盖
        let payload2 =
            json!({"model": "m", "max_completion_tokens": 128, "max_tokens": 64, "messages": []});
        let out2 = build_upstream_body(&payload2, &opts());
        assert_eq!(out2["max_tokens"], 64);
    }

    #[test]
    fn catalog_default_max_tokens_when_absent() {
        let payload = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let opts = BodyOptions {
            model_default_max_tokens: Some(4096),
            ..BodyOptions::default()
        };
        let out = build_upstream_body(&payload, &opts);
        assert_eq!(out["max_tokens"], 4096);
    }

    #[test]
    fn tool_choice_dict_normalized() {
        let payload = json!({"model": "m", "tool_choice": {"type": "none"}, "messages": [{"role": "user", "content": "hi"}]});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["tool_choice"], "none");
        let payload2 = json!({"model": "m", "tool_choice": {"type": "function", "function": {"name": "f"}}, "messages": []});
        let out2 = build_upstream_body(&payload2, &opts());
        assert_eq!(out2["tool_choice"], "f");
    }

    #[test]
    fn flat_tools_wrapped() {
        let payload = json!({"model": "m", "tools": [{"type": "function", "name": "f", "description": "d"}], "messages": []});
        let out = build_upstream_body(&payload, &opts());
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "f");
        assert_eq!(tool["function"]["description"], "d");
    }

    #[test]
    fn stop_normalized_to_array() {
        let payload = json!({"model": "m", "stop": "END", "messages": []});
        let out = build_upstream_body(&payload, &opts());
        assert_eq!(out["stop"], json!(["END"]));
        let payload2 = json!({"model": "m", "stop": ["END", ""], "messages": []});
        let out2 = build_upstream_body(&payload2, &opts());
        assert_eq!(out2["stop"], json!(["END"]));
    }

    #[test]
    fn tool_call_merge_form1() {
        let payload = json!({"model": "m", "messages": [
            {"role": "assistant", "content": null, "tool_calls": [{"id": "t1", "function": {"name": "a"}}]},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "t2", "function": {"name": "b"}}]},
            {"role": "tool", "tool_call_id": "t2", "content": "r2"},
            {"role": "tool", "tool_call_id": "t1", "content": "r1"},
        ]});
        let out = build_upstream_body(&payload, &opts());
        let msgs = out["messages"].as_array().unwrap();
        // system 注入 + 相邻声明合并 → [system, assistant(t1+t2), tool(t2), tool(t1)]
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        let calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1]["id"], "t2");
        // 两个结果均保留（cleanup 见 orphan_tool_calls_cleaned）
        assert_eq!(msgs[2]["tool_call_id"], "t2");
        assert_eq!(msgs[3]["tool_call_id"], "t1");
    }

    #[test]
    fn orphan_tool_calls_cleaned() {
        let payload = json!({"model": "m", "messages": [
            {"role": "assistant", "tool_calls": [{"id": "t1", "function": {"name": "a"}}]},
            {"role": "tool", "tool_call_id": "tX", "content": "orphan"},
        ]});
        let out = build_upstream_body(&payload, &opts());
        let msgs = out["messages"].as_array().unwrap();
        // system 注入 + assistant（孤儿 tool_calls 删键）+ orphan tool 整条删 → 2 条
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].as_object().unwrap().get("tool_calls").is_none());
    }
}
