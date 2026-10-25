//! 关卡 4.25：面板日志缓冲（对齐 Python `wb_proxy.py` 的 LOG_BUFFER）。
//!
//! - `add_log_entry`（:9845-9887）：自动定级（ERROR/WARN/INFO）+ 自动归类
//!   （chat/scheduler/tasks/accounts/catalog/auth/settings/system）+ 追加到
//!   `LOG_BUFFER`（deque maxlen=2000）。
//! - `get_logs`（:9899-9916）：limit/level/tag/search/since_id 过滤 →
//!   `{total, logs, max_id}`。
//! - `logs_export`：导出为纯文本行（`[{ts}] [{level}] [{tag}] {msg}`）。
//! - `clear_logs`：清空缓冲。
//!
//! `ApiPanelSink::add_log_entry`（api crate）接入本模块的 `append`，后台线程
//! 的日志由此进得了面板日志页（此前是 no-op）。

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{json, Value};

/// 缓冲上限（Python `LOG_BUFFER = deque(maxlen=2000)`）。
pub const LOG_MAXLEN: usize = 2000;
/// 导出上限（Python `get_logs(limit=5000)`）。
pub const LOG_EXPORT_LIMIT: usize = 5000;

/// 日志条目。
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: u64,
    pub ts: String,
    pub time: String,
    pub level: String,
    pub tag: String,
    pub msg: String,
}

static RNA: Mutex<LogState> = Mutex::new(LogState {
    counter: 0,
    buffer: VecDeque::new(),
});

/// 缓冲状态（锁内）。
struct LogState {
    counter: u64,
    buffer: VecDeque<LogEntry>,
}

/// 当前本地时间（`%Y-%m-%d %H:%M:%S`）与短时间（`%H:%M:%S`）。
fn now_texts() -> (String, String) {
    let now = chrono::Local::now();
    (
        now.format("%Y-%m-%d %H:%M:%S").to_string(),
        now.format("%H:%M:%S").to_string(),
    )
}

/// 自动定级（Python :9856-9863）。
fn auto_level(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if [
        "error",
        "exception",
        "failed",
        "11128",
        "11101",
        "11140",
        "traceback",
        "errno",
        "fatal",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        "ERROR"
    } else if ["warn", "warning", "retry", "timeout"]
        .iter()
        .any(|k| lower.contains(k))
    {
        "WARN"
    } else {
        "INFO"
    }
}

/// 自动归类（Python :9865-9879）。
fn auto_tag(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if [
        "chat:",
        "chat done",
        "/v1/chat",
        "/chat/completions",
        "responses",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        "chat"
    } else if lower.contains("scheduler") || lower.contains("调度器") {
        "scheduler"
    } else if ["task", "任务", "打卡", "猫猫", "travel"]
        .iter()
        .any(|k| lower.contains(k))
    {
        "tasks"
    } else if ["account", "账号", "pool", "imported"]
        .iter()
        .any(|k| lower.contains(k))
    {
        "accounts"
    } else if ["model", "catalog", "模型"]
        .iter()
        .any(|k| lower.contains(k))
    {
        "catalog"
    } else if ["auth", "token", "oauth"].iter().any(|k| lower.contains(k)) {
        "auth"
    } else if lower.contains("settings") || lower.contains("设置") {
        "settings"
    } else {
        "system"
    }
}

/// 追加一条日志（Python `add_log_entry`）。`level`/`tag` 缺省时自动判定。
pub fn append(msg: &str, level: Option<&str>, tag: Option<&str>) -> LogEntry {
    let msg_str = msg.trim_end().to_string();
    let level = level
        .map(str::to_string)
        .unwrap_or_else(|| auto_level(&msg_str).to_string());
    let tag = tag
        .map(str::to_string)
        .unwrap_or_else(|| auto_tag(&msg_str).to_string());
    let (ts, time) = now_texts();
    let mut state = RNA.lock().unwrap();
    state.counter += 1;
    let entry = LogEntry {
        id: state.counter,
        ts,
        time,
        level,
        tag,
        msg: msg_str,
    };
    if state.buffer.len() >= LOG_MAXLEN {
        state.buffer.pop_front();
    }
    state.buffer.push_back(entry.clone());
    entry
}

/// 读日志（Python `get_logs` :9899-9916）。
///
/// `limit` 为 `None` 或 0 表示不截断；`since_id` 非 0 时不截尾（Python：
/// `limit and limit > 0 and since_id == 0` 才截尾）。
pub fn get_logs(
    limit: Option<usize>,
    level: &str,
    tag: &str,
    search: &str,
    since_id: u64,
) -> Value {
    let state = RNA.lock().unwrap();
    let mut items: Vec<Value> = state
        .buffer
        .iter()
        .filter(|e| since_id == 0 || e.id > since_id)
        .filter(|e| level.is_empty() || e.level == level.to_uppercase())
        .filter(|e| tag.is_empty() || e.tag.eq_ignore_ascii_case(tag))
        .filter(|e| {
            if search.is_empty() {
                return true;
            }
            let s = search.to_lowercase();
            e.msg.to_lowercase().contains(&s) || e.tag.to_lowercase().contains(&s)
        })
        .map(entry_json)
        .collect();
    let total = items.len();
    if let Some(limit) = limit {
        if limit > 0 && since_id == 0 && items.len() > limit {
            let start = items.len() - limit;
            items = items[start..].to_vec();
        }
    }
    let max_id = items
        .last()
        .and_then(Value::as_object)
        .and_then(|o| o.get("id"))
        .cloned()
        .unwrap_or(json!(since_id));
    json!({ "total": total, "logs": items, "max_id": max_id })
}

/// 日志条目转 JSON（Python entry dict：id/ts/time/level/tag/msg）。
fn entry_json(e: &LogEntry) -> Value {
    json!({
        "id": e.id,
        "ts": e.ts,
        "time": e.time,
        "level": e.level,
        "tag": e.tag,
        "msg": e.msg,
    })
}

/// 导出文本（Python `_get_logs_export` 的 `get_logs(limit=5000)` + 行格式化）。
pub fn export_text() -> String {
    let state = RNA.lock().unwrap();
    let max = if LOG_EXPORT_LIMIT >= state.buffer.len() {
        state.buffer.len()
    } else {
        LOG_EXPORT_LIMIT
    };
    let start = state.buffer.len() - max;
    state
        .buffer
        .iter()
        .skip(start)
        .map(|e| format!("[{}] [{}] [{}] {}", e.ts, e.level, e.tag, e.msg))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 清空（Python `clear_logs` :9915-9917）。
pub fn clear() {
    let mut state = RNA.lock().unwrap();
    state.buffer.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 静态缓冲被并行测试共享 ⇒ 串行锁隔离。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        let mut state = RNA.lock().unwrap();
        state.counter = 0;
        state.buffer.clear();
    }

    #[test]
    fn append_autotags_level_and_tag() {
        let _g = TEST_LOCK.lock().unwrap();
        reset();
        // "restricted" 不是 Python 的 ERROR 关键词 ⇒ INFO；tag 含 "account" ⇒ accounts
        let e = append("account restricted by upstream", None, None);
        assert_eq!(e.level, "INFO");
        assert_eq!(e.tag, "accounts");
        let e2 = append("/v1/chat/completions done", None, None);
        assert_eq!(e2.level, "INFO");
        assert_eq!(e2.tag, "chat");
        let e3 = append("retrying after timeout", None, None);
        assert_eq!(e3.level, "WARN");
        // "failed" 触发 ERROR
        let e4 = append("task failed", None, None);
        assert_eq!(e4.level, "ERROR");
    }

    #[test]
    fn get_logs_filters_limit_since_and_search() {
        let _g = TEST_LOCK.lock().unwrap();
        reset();
        append("scheduler tick", None, None);
        append("chat done", None, None);
        append("scheduler again", None, None);
        let all = get_logs(None, "", "", "", 0);
        assert_eq!(all["total"], 3);
        let tag = get_logs(None, "", "scheduler", "", 0);
        assert_eq!(tag["total"], 2);
        let level = get_logs(None, "INFO", "", "", 0);
        assert_eq!(level["total"], 3);
        let search = get_logs(None, "", "", "chat", 0);
        assert_eq!(search["total"], 1);
        let since = get_logs(None, "", "", "", 2);
        assert_eq!(since["total"], 1);
        assert_eq!(since["max_id"], 3);
        // Python：total 是截断前的完整计数，items 截断到 limit 条。
        let limited = get_logs(Some(1), "", "", "", 0);
        assert_eq!(limited["total"], 3);
        assert_eq!(limited["logs"].as_array().unwrap().len(), 1);
        assert_eq!(limited["logs"][0]["id"], 3);
    }

    #[test]
    fn export_and_clear() {
        let _g = TEST_LOCK.lock().unwrap();
        reset();
        append("hello world", None, None);
        let text = export_text();
        assert!(text.contains("hello world"), "export 应包含 msg: {text}");
        clear();
        assert_eq!(get_logs(None, "", "", "", 0)["total"], 0);
    }

    #[test]
    fn buffer_caps_at_maxlen() {
        let _g = TEST_LOCK.lock().unwrap();
        reset();
        for i in 0..(LOG_MAXLEN + 50) {
            append(&format!("msg {i}"), None, None);
        }
        assert_eq!(export_text().lines().count(), LOG_MAXLEN);
    }
}
