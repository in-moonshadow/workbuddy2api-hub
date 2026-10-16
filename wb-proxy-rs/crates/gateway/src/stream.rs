//! SSE 行清洗与流聚合，移植自 wb_proxy.py:
//! - :10547-10557 strip_data_prefix
//! - :10558-10596 clean_chunk（NOISE_KEYS 空占位剔除、usage 别名归一）
//! - :247 NOISE_KEYS
//! - :454-473 normalize_usage_cache_aliases / :635-660 _best_cached_tokens
//! - :12841-12848 estimate_tokens（CJK + 其余/3.6）
//! - :11015-11033 is_truncated_arguments
//! - :12850-12996 aggregate_stream（非流式聚合：内容/推理/tool_calls 合并、
//!   finish 判定、usage 兜底估算）
//!
//! 纯函数；行迭代由调用方（reqwest 字节流按 \n 切分）驱动。

use serde_json::Value;

use crate::leaked::repair_leaked_tool_calls;

/// NOISE_KEYS（wb_proxy.py:247）：空值即剔除的 delta 噪音字段。
const NOISE_KEYS: [&str; 3] = ["extra_fields", "refusal", "reasoning_content"];

/// CJK 估算（wb_proxy.py:12840）。
static CJK_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"[一-鿿㐀-䶿]").unwrap());

/// estimate_tokens（wb_proxy.py:12841-12848）：CJK 字符按 1 token，其余
/// 按 3.6 字符折算（最少 1）。空 → 0。
pub fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let cjk = CJK_RE.find_iter(text).count() as i64;
    let total = text.chars().count() as i64;
    let other = total - cjk;
    let other_tokens = ((other as f64) / 3.6) as i64;
    cjk + other_tokens.max(1)
}

/// strip_data_prefix（wb_proxy.py:10547-10557）：SSE 行剥 `data:` 前缀与
/// 注释/心跳；非 data 行 → 空串。
pub fn strip_data_prefix(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return "";
    }
    let mut rest = trimmed;
    while let Some(stripped) = rest.strip_prefix("data:") {
        rest = stripped.trim_start();
    }
    if rest.is_empty() || rest.starts_with(':') {
        return "";
    }
    rest
}

/// 归一化 usage 缓存别名（wb_proxy.py:454-473）：把 best 命中值写入所有别名。
/// 无命中（best<=0）不动。就地修改。
pub fn normalize_usage_cache_aliases(usage: &mut Value) {
    let best = best_cached_tokens(usage);
    if best <= 0 {
        return;
    }
    let Some(obj) = usage.as_object_mut() else {
        return;
    };
    obj.insert("cache_read_input_tokens".into(), Value::Number(best.into()));
    obj.insert("cached_tokens".into(), Value::Number(best.into()));
    obj.insert("prompt_cache_hit_tokens".into(), Value::Number(best.into()));
    if let Some(Value::Object(pd)) = obj.get_mut("prompt_tokens_details") {
        pd.insert("cached_tokens".into(), Value::Number(best.into()));
    } else {
        let mut pd = serde_json::Map::new();
        pd.insert("cached_tokens".into(), Value::Number(best.into()));
        obj.insert("prompt_tokens_details".into(), Value::Object(pd));
    }
    if let Some(Value::Object(ic)) = obj.get_mut("input_tokens_details") {
        ic.insert("cached_tokens".into(), Value::Number(best.into()));
    }
}

/// _best_cached_tokens→整数化（wb_proxy.py:635-660）：按序取首个正命中值。
/// pub：Anthropic 翻译层（anthropic.rs `_anthropic_usage` 缓存别名）复用。
pub fn best_cached_tokens(usage: &Value) -> i64 {
    let pick = |o: &Value, key: &str| -> Option<i64> {
        let v = o.get(key)?;
        let n = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
        (n > 0).then_some(n)
    };
    let prompt_details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let input_details = usage.get("input_tokens_details").unwrap_or(&Value::Null);
    let completion_details = usage
        .get("completion_tokens_details")
        .unwrap_or(&Value::Null);
    for srcc in [
        (prompt_details, "cached_tokens"),
        (usage, "prompt_cache_hit_tokens"),
        (usage, "cache_read_input_tokens"),
        (usage, "cached_tokens"),
        (input_details, "cached_tokens"),
        (completion_details, "cached_tokens"),
    ] {
        if let Some(v) = pick(srcc.0, srcc.1) {
            return v;
        }
    }
    0
}

/// clean_chunk（wb_proxy.py:10558-10596）：剔除上游用空的噪音增量。
/// 返回 None = 该 chunk 应整体丢弃（delta 全空且无 finish）。
pub fn clean_chunk(raw: &str) -> Option<String> {
    let mut obj: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Some(raw.to_string()),
    };
    let mut changed = false;
    if let Some(usage) = obj.get_mut("usage") {
        let before = usage.clone();
        normalize_usage_cache_aliases(usage);
        if usage != &before {
            changed = true;
        }
    }
    let mut drop = false;
    if let Some(choices) = obj.get_mut("choices").and_then(|c| c.as_array_mut()) {
        for choice in choices.iter_mut() {
            let has_finish = choice
                .get("finish_reason")
                .map(|f| !f.is_null())
                .unwrap_or(false);
            let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut()) else {
                continue;
            };
            // function_call 空 name 占位剔除
            if let Some(fc) = delta.get("function_call").cloned() {
                let fc_empty = match &fc {
                    Value::Object(o) => o.get("name").and_then(|n| n.as_str()) == Some(""),
                    Value::Null => true,
                    _ => true,
                };
                if fc_empty {
                    delta.remove("function_call");
                    changed = true;
                }
            }
            if let Some(tc) = delta.get("tool_calls") {
                if matches!(tc, Value::Array(a) if a.is_empty()) {
                    delta.remove("tool_calls");
                    changed = true;
                }
            }
            for key in NOISE_KEYS {
                if let Some(v) = delta.get(key) {
                    let falsy = match v {
                        Value::Null => true,
                        Value::Bool(false) => true,
                        Value::Number(n) => n.as_f64() == Some(0.0),
                        Value::String(s) => s.is_empty(),
                        _ => false,
                    };
                    if falsy {
                        delta.remove(key);
                        changed = true;
                    }
                }
            }
            if delta.is_empty() && !has_finish {
                drop = true;
            }
        }
    }
    if drop {
        return None;
    }
    if changed {
        serde_json::to_string(&obj).ok()
    } else {
        Some(raw.to_string())
    }
}

/// is_truncated_arguments（wb_proxy.py:11015-11033）：非空且不可解析的
/// tool arguments 视为被 max_tokens/断线截断。空/空白是合法无参调用。
pub fn is_truncated_arguments(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    serde_json::from_str::<Value>(trimmed).is_err()
}

/// aggregate_stream 内部暂存的工具调用。
#[derive(Clone)]
struct ToolCall {
    id: String,
    r#type: String,
    name: String,
    arguments: String,
}

/// aggregate_stream（wb_proxy.py:12850-12996）：把 SSE 流折成一个非流式
/// chat.completion。`lines` 为已 strip 的 data 载荷（[DONE] 由 saw_done
/// 捕获）。`now` 注入以便测试固定 created。
pub fn aggregate_stream(lines: &[&str], model: &str, resp_id: &str, now: f64) -> Value {
    let mut content: Vec<String> = Vec::new();
    let mut reasoning: Vec<String> = Vec::new();
    let mut finish: String = "stop".into();
    let mut tool_calls: std::collections::BTreeMap<i64, ToolCall> =
        std::collections::BTreeMap::new();
    let mut usage: Option<Value> = None;
    let mut saw_done = false;
    let mut out_id = resp_id.to_string();
    let mut out_model = model.to_string();
    let mut first_chunk_at: Option<f64> = None;

    for data in lines {
        if data.is_empty() {
            continue;
        }
        if *data == "[DONE]" {
            saw_done = true;
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if first_chunk_at.is_none() {
            first_chunk_at = Some(now);
        }
        if let Some(Value::String(id)) = chunk.get("id") {
            out_id = id.clone();
        }
        if let Some(Value::String(m)) = chunk.get("model") {
            out_model = m.clone();
        }
        if let Some(u) = chunk.get("usage") {
            let take = match &usage {
                None => true,
                Some(prev) => {
                    let prev_total = prev
                        .get("total_tokens")
                        .and_then(|t| t.as_i64())
                        .unwrap_or(0);
                    u.get("total_tokens").and_then(|t| t.as_i64()).unwrap_or(0) >= prev_total
                }
            };
            if take {
                usage = Some(u.clone());
            }
        }
        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        for choice in choices {
            let delta = choice.get("delta").and_then(|d| d.as_object());
            let Some(delta) = delta else {
                continue;
            };
            if let Some(Value::String(c)) = delta.get("content") {
                content.push(c.clone());
            }
            if let Some(Value::String(r)) = delta.get("reasoning_content") {
                reasoning.push(r.clone());
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let Some(obj) = tc.as_object() else {
                        continue;
                    };
                    let idx = obj
                        .get("index")
                        .and_then(|i| i.as_i64())
                        .unwrap_or(tool_calls.len() as i64);
                    let id = obj.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let r#type = obj
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("function");
                    let fn_obj = obj.get("function").and_then(|f| f.as_object());
                    let fn_name = fn_obj
                        .and_then(|f| f.get("name").and_then(|n| n.as_str()))
                        .unwrap_or("");
                    let fn_args = fn_obj
                        .and_then(|f| f.get("arguments").and_then(|a| a.as_str()))
                        .unwrap_or("");
                    match tool_calls.get_mut(&idx) {
                        Some(entry) => {
                            if !id.is_empty() {
                                entry.id = id.to_string();
                            }
                            if !fn_name.is_empty() {
                                entry.name.push_str(fn_name);
                            }
                            if !fn_args.is_empty() {
                                entry.arguments.push_str(fn_args);
                            }
                        }
                        None => {
                            tool_calls.insert(
                                idx,
                                ToolCall {
                                    id: if id.is_empty() {
                                        crate::leaked::new_call_id()
                                    } else {
                                        id.to_string()
                                    },
                                    r#type: r#type.to_string(),
                                    name: fn_name.to_string(),
                                    arguments: fn_args.to_string(),
                                },
                            );
                        }
                    }
                }
            }
            // 流尾 function_call 空占位直接跳过（PATCH2-BY-OPS）
            if let Some(fc) = delta.get("function_call") {
                let fc_nonempty = match fc {
                    Value::Object(o) => o.get("name").and_then(|n| n.as_str()) == Some(""),
                    _ => true,
                };
                if fc_nonempty {
                    let fc_obj = fc.as_object().cloned().unwrap_or_default();
                    let idx = 0i64;
                    let id = fc_obj.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = fc_obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let args = fc_obj
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("");
                    match tool_calls.get_mut(&idx) {
                        Some(entry) => {
                            if !name.is_empty() && entry.name.is_empty() {
                                entry.name = name.to_string();
                            }
                            if !args.is_empty() {
                                entry.arguments.push_str(args);
                            }
                            if !id.is_empty() {
                                entry.id = id.to_string();
                            }
                        }
                        None => {
                            tool_calls.insert(
                                idx,
                                ToolCall {
                                    id: if id.is_empty() {
                                        crate::leaked::new_call_id()
                                    } else {
                                        id.to_string()
                                    },
                                    r#type: "function".into(),
                                    name: name.to_string(),
                                    arguments: args.to_string(),
                                },
                            );
                        }
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason") {
                if let Some(s) = fr.as_str() {
                    finish = s.to_string();
                }
            }
        }
    }

    let mut message = serde_json::json!({
        "role": "assistant",
        "content": content.concat(),
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning.concat());
    }

    // 打印在 content 里的调用恢复
    if tool_calls.is_empty() && repair_leaked_tool_calls(&mut message) {
        finish = "tool_calls".into();
    }

    // 空占位二次防御：去掉无函数名的 tool_call
    tool_calls.retain(|_, tc| !tc.name.is_empty());

    // 截断参数：max_tokens 断流或未见 [DONE] 时丢弃半截 JSON
    if !tool_calls.is_empty() && (finish == "length" || !saw_done) {
        tool_calls.retain(|_, tc| !is_truncated_arguments(&tc.arguments));
    }

    if !tool_calls.is_empty() {
        let ordered: Vec<Value> = tool_calls
            .iter()
            .map(|(&_k, tc)| {
                serde_json::json!({
                    "id": tc.id,
                    "type": tc.r#type,
                    "function": {"name": tc.name, "arguments": tc.arguments},
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(ordered);
        if finish == "stop" {
            finish = "tool_calls".into();
        }
    } else if message.get("tool_calls").is_some() {
        finish = "tool_calls".into();
    } else if finish == "tool_calls" {
        finish = "stop".into();
    }

    // usage 兜底估算（wb_proxy.py:12971-12983）
    if usage.is_none()
        || usage
            .as_ref()
            .and_then(|u| u.get("total_tokens").and_then(|t| t.as_i64()))
            .unwrap_or(0)
            == 0
    {
        let full_c = content.concat();
        let full_r = reasoning.concat();
        if !full_c.is_empty() || !full_r.is_empty() {
            let comp = estimate_tokens(&full_c) + estimate_tokens(&full_r);
            let prompt_est = (comp / 2).max(1);
            usage = Some(serde_json::json!({
                "prompt_tokens": prompt_est,
                "completion_tokens": comp,
                "total_tokens": prompt_est + comp,
                "completion_tokens_details": {"reasoning_tokens": estimate_tokens(&full_r)},
                "prompt_tokens_details": {"cached_tokens": 0},
            }));
        }
    }

    let mut out = serde_json::json!({
        "id": if out_id.is_empty() { "chatcmpl-wb".to_string() } else { out_id },
        "object": "chat.completion",
        "created": now as i64,
        "model": out_model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
    });
    if let Some(mut u) = usage {
        normalize_usage_cache_aliases(&mut u);
        out["usage"] = u;
    }
    out["elapsed_ms"] = Value::Number(serde_json::Number::from(0));
    if let Some(fc) = first_chunk_at {
        out["first_chunk_at"] = Value::Number(serde_json::Number::from_f64(fc).unwrap_or(0.into()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_data_sse_lines() {
        assert_eq!(strip_data_prefix("data: {\\\"a\\\":1}"), "{\\\"a\\\":1}");
        assert_eq!(strip_data_prefix("data: {\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_data_prefix("data:{a}"), "{a}");
        assert_eq!(strip_data_prefix(": comment"), "");
        assert_eq!(strip_data_prefix(""), "");
        assert_eq!(strip_data_prefix("data: "), "");
    }

    #[test]
    fn estimate_tokens_cjk_and_latin() {
        assert_eq!(estimate_tokens(""), 0);
        // Python：cjk + max(1, int(other/3.6))，other=0 时兜底 1
        assert_eq!(estimate_tokens("你好世界"), 5);
        assert_eq!(estimate_tokens("hello"), 1); // int(5/3.6)=1
        assert_eq!(estimate_tokens("你好 hello"), 3); // cjk=2 + int(6/3.6)=1
    }

    #[test]
    fn clean_chunk_drops_empty_delta() {
        // 空 delta 且无 finish → 丢弃
        assert_eq!(
            clean_chunk(r#"{"choices":[{"delta":{},"finish_reason":null}]}"#),
            None
        );
        // 有 finish → 保留
        assert!(clean_chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).is_some());
        // 非 JSON 原样
        assert_eq!(clean_chunk("not json"), Some("not json".into()));
        // 空 reasoning_content 噪音剔除
        let with_noise = r#"{"choices":[{"delta":{"content":"hi","reasoning_content":""},"finish_reason":null}]}"#;
        let out = clean_chunk(with_noise).unwrap();
        assert!(!out.contains("reasoning_content"));
        // function_call 空 name 占位剔除后 delta 全空且无 finish → 整块丢弃
        // （对齐 Python clean_chunk 返回 ""）
        let fc_placeholder = r#"{"choices":[{"delta":{"function_call":{"id":null,"name":"","arguments":""}},"finish_reason":null}]}"#;
        assert_eq!(clean_chunk(fc_placeholder), None);
    }

    #[test]
    fn is_truncated_arguments_rule() {
        assert!(is_truncated_arguments(r#"{"a": "unclosed"#));
        assert!(!is_truncated_arguments(""));
        assert!(!is_truncated_arguments("  "));
        assert!(!is_truncated_arguments(r#"{"a":1}"#));
        assert!(!is_truncated_arguments("null")); // 可解析 scalars 不算截断
    }

    #[test]
    fn normalize_usage_writes_all_aliases() {
        let mut usage = json!({
            "prompt_tokens_details": {"cached_tokens": 42},
            "cache_read_input_tokens": 0,
            "cached_tokens": 0,
        });
        normalize_usage_cache_aliases(&mut usage);
        assert_eq!(usage["cache_read_input_tokens"], 42);
        assert_eq!(usage["cached_tokens"], 42);
        assert_eq!(usage["prompt_cache_hit_tokens"], 42);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 42);
    }

    #[test]
    fn aggregate_content_and_usage() {
        let lines = [
            r#"{"id":"a1","model":"m","choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#,
            r#"{"id":"a1","model":"m","choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
            r#"{"id":"a1","model":"m","usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#,
            "[DONE]",
        ];
        let out = aggregate_stream(&lines, "m", "chatcmpl-wb", 1_000.0);
        assert_eq!(out["id"], "a1");
        assert_eq!(out["choices"][0]["message"]["content"], "Hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["total_tokens"], 12);
        assert_eq!(out["created"], 1000);
    }

    #[test]
    fn aggregate_merges_tool_calls_and_reorders() {
        let lines = [
            r#"{"id":"a1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"run_","arguments":"{\"code\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"a1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":" git status\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"a1","choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"ls","arguments":"{}"}}]},"finish_reason":null}]}"#,
            "[DONE]",
        ];
        let out = aggregate_stream(&lines, "m", "chatcmpl-wb", 1_000.0);
        let tcs = out["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0]["id"], "call_1");
        assert_eq!(tcs[0]["function"]["name"], "run_");
        assert!(tcs[0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("git status"));
        assert_eq!(tcs[1]["id"], "call_2");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn aggregate_usage_fallback_when_empty() {
        let lines = [r#"{"choices":[{"delta":{"content":"你好"},"finish_reason":"stop"}]}"#];
        let out = aggregate_stream(&lines, "m", "chatcmpl-wb", 1_000.0);
        let usage = out["usage"].as_object().unwrap();
        // 估算：estimate_tokens("你好") = 2 + max(1, 0/3) = 3 → comp=3
        assert_eq!(usage["completion_tokens"], 3);
        assert_eq!(usage["prompt_tokens"], 1);
        assert_eq!(usage["total_tokens"], 4);
    }

    #[test]
    fn aggregate_dropped_truncated_tool_calls_without_done() {
        // 流无 [DONE] + 非 JSON arguments 截断 → 丢弃该 call
        let lines = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{\"bad\" :"}}]},"finish_reason":"length"}]}"#,
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#,
        ];
        let out = aggregate_stream(&lines, "m", "chatcmpl-wb", 1_000.0);
        let msg = &out["choices"][0]["message"];
        assert!(msg.get("tool_calls").is_none());
        assert_eq!(out["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn aggregate_recovers_leaked_call_from_content() {
        // 模型打印调用而非结构化 → 恢复到 tool_calls，finish 改 tool_calls
        let lines = [
            r#"{"choices":[{"delta":{"content":"Let me check: <run_code><parameter name=\"code\">git status</parameter></run_code></invoke>"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ];
        let out = aggregate_stream(&lines, "m", "chatcmpl-wb", 1_000.0);
        let msg = &out["choices"][0]["message"];
        assert_eq!(msg["content"], "Let me check:");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "run_code");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }
}
