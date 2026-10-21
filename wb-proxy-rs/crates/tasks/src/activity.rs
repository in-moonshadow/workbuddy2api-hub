//! M5 数据侧：账号每日活动的结构化历史（JSONL）。
//!
//! 权威源：Python `wb_activity.py`。
//!
//! 落盘的是**运维元数据**，不是凭证：时间 / uid / 昵称 / 区域 / 任务 / 触发来源 /
//! 结果 / 一句截断并脱敏过的说明。token、cookie、API key、授权头、原始请求体
//! 既不落盘，也不会出现在读取结果里。
//!
//! 本模块的读路径**一律先投影**（[`project`]）再筛选/返回：JSONL 是磁盘上的
//! 文件，一行里完全可能多出别的键（别的写入方、手工补的行、被塞进来的凭证），
//! 白名单投影是最后一道闸。

use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// 历史文件名。
pub const HISTORY_NAME: &str = "activity_history.jsonl";

/// 国内版签到。
pub const TASK_CHECKIN: &str = "checkin";
/// 国际版每日活跃。
pub const TASK_DAILY_CHAT: &str = "daily_chat";
/// 可记录的任务（只这两个）。
pub const TASKS: [&str; 2] = [TASK_CHECKIN, TASK_DAILY_CHAT];

/// 触发来源。认不出来的值归一成 `unknown` —— 记历史绝不能让签到本身失败。
pub const TRIGGERS: [&str; 5] = [
    "scheduler",
    "manual",
    "account_add",
    "account_import",
    "unknown",
];

/// 可记录的区域。
pub const REALMS: [&str; 2] = ["cn", "intl"];

/// 允许出现在 API 响应里的字段，一个不多。
pub const ALLOWED_FIELDS: [&str; 8] = [
    "ts", "uid", "nickname", "realm", "task", "trigger", "ok", "message",
];

/// 默认查询区间。
pub const DEFAULT_RANGE: &str = "7d";
/// 可用的区间。
pub const RANGES: [&str; 6] = ["today", "1d", "7d", "30d", "90d", "all"];

/// 默认返回条数。
pub const DEFAULT_LIMIT: usize = 100;
/// 返回条数上限。
pub const MAX_LIMIT: usize = 1000;

/// 保留策略：行数上限。
pub const MAX_RECORDS: usize = 5000;
/// 保留策略：天数上限。
pub const MAX_AGE_DAYS: i64 = 90;
/// 文件超过这个大小就立刻整理。
pub const COMPACT_TRIGGER_BYTES: u64 = 2_000_000;
/// 距上次整理超过这个秒数就整理一次（低流量历史跨不过大小阈值）。
pub const COMPACT_INTERVAL_SECONDS: f64 = 6.0 * 3600.0;

/// 消息截断长度：历史表一行只该是一句人能读的原因。
pub const MAX_MESSAGE: usize = 200;

/// 脱敏后的占位符。
pub const REDACTED: &str = "[redacted]";

/// 已知凭证形态（Python `wb_activity._SECRET_PATTERNS`，逐条对齐）。
///
/// 上游的错误文本是自由文本，401/403 的正文里回显授权头是常见现象。
/// 这是「截断 + 已知形态打码」，不是通用清洗。
static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\b(?:bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}").unwrap(),
        Regex::new(
            r"(?i)\b(?:access[_-]?token|refresh[_-]?token|api[_-]?key|apikey|authorization|cookie|set-cookie|password|secret|token)\b\s*[:=]\s*\S+",
        )
        .unwrap(),
        Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\b").unwrap(),
        Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").unwrap(),
        Regex::new(r"\b[0-9a-fA-F]{32,}\b").unwrap(),
        Regex::new(r"\b[A-Za-z0-9+/]{40,}={0,2}\b").unwrap(),
    ]
});

/// Python 的真值字符串集合（`wb_activity._TRUE`）。
const TRUE_STRINGS: [&str; 5] = ["1", "true", "yes", "ok", "success"];

/// 把上游原文压成一行安全的短说明。
///
/// 步骤与 Python `safe_message` 一致：先压空白，再按 6 条凭证形态打码，
/// 最后截到 200 字。
pub fn safe_message(text: &str) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for pattern in SECRET_PATTERNS.iter() {
        line = pattern.replace_all(&line, REDACTED).into_owned();
    }
    if line.chars().count() > MAX_MESSAGE {
        line = line.chars().take(MAX_MESSAGE).collect();
    }
    line
}

/// Python 的 `str(text or "")`：空值变空串，`0` / `False` / 空容器也变空串。
pub fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                String::new()
            }
        }
        Value::Number(n) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(a) => {
            if a.is_empty() {
                String::new()
            } else {
                value.to_string()
            }
        }
        Value::Object(o) => {
            if o.is_empty() {
                String::new()
            } else {
                value.to_string()
            }
        }
    }
}

/// JSONL 里的 `ok` 可能是字符串（别的写入方写的），别把 `"false"` 读成成功。
pub fn as_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        other => {
            let text = match other {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                v => value_text(v),
            };
            TRUE_STRINGS.contains(&text.trim().to_lowercase().as_str())
        }
    }
}

/// 把一行投影到 [`ALLOWED_FIELDS`]，多出来的键一个都不带出去。
///
/// 每个字段同时归一成它该有的类型和长度，`message` 再过一遍脱敏
/// （等于读取侧也做一次）。
pub fn project(row: &Value) -> Value {
    let text = |key: &str, max: usize| -> String {
        let raw = row.get(key).cloned().unwrap_or(Value::Null);
        let s = value_text(&raw);
        s.chars().take(max).collect()
    };
    let enum_or = |key: &str, allowed: &[&str], default: &str| -> String {
        let raw = row.get(key).and_then(Value::as_str).unwrap_or("");
        if allowed.contains(&raw) {
            raw.to_string()
        } else {
            default.to_string()
        }
    };
    let mut out = Map::new();
    out.insert("ts".into(), json!(text("ts", 40)));
    out.insert("uid".into(), json!(text("uid", 80)));
    out.insert("nickname".into(), json!(text("nickname", 120)));
    out.insert("realm".into(), json!(enum_or("realm", &REALMS, "")));
    out.insert("task".into(), json!(enum_or("task", &TASKS, "")));
    out.insert(
        "trigger".into(),
        json!(enum_or("trigger", &TRIGGERS, "unknown")),
    );
    out.insert(
        "ok".into(),
        json!(row.get("ok").map(as_bool).unwrap_or(false)),
    );
    out.insert(
        "message".into(),
        json!(safe_message(&text("message", usize::MAX))),
    );
    Value::Object(out)
}

/// 构造一行记录（Python `wb_activity.record` 的 `row` 字面量）。
///
/// `ts` 传空串时用当前本地时间（带偏移量，秒精度）。
///
/// 参数与 Python `wb_activity.record` 的 8 个形参逐字对应（改用结构体拆开
/// 反而不好并排比对），故豁免 clippy 的「参数过多」。
#[allow(clippy::too_many_arguments)]
pub fn build_row(
    ts: &str,
    uid: &str,
    nickname: &str,
    realm: &str,
    task: &str,
    trigger: &str,
    ok: bool,
    message: &str,
) -> Value {
    let ts = if ts.is_empty() {
        local_iso_now()
    } else {
        ts.to_string()
    };
    json!({
        "ts": ts,
        "uid": uid,
        "nickname": nickname,
        "realm": if REALMS.contains(&realm) { realm } else { "" },
        "task": task,
        "trigger": if TRIGGERS.contains(&trigger) { trigger } else { "unknown" },
        "ok": ok,
        "message": safe_message(message),
    })
}

/// 当前本地时间（带 UTC 偏移，秒精度）。
///
/// Python `datetime.now().astimezone().isoformat(timespec="seconds")`。
pub fn local_iso_now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// 解析一行的时间戳（epoch 秒）。
///
/// Python `datetime.fromisoformat(str(value)).timestamp()`。
pub fn parse_ts(value: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

/// 本地日历零点的 epoch。
///
/// 按「那个零点本身」的本地时间规则推导，不能拿此刻生效的 UTC 偏移去替换
/// 字段：夏令时切换日里，本地零点与此刻的偏移差一小时。
pub fn local_midnight(now: f64) -> f64 {
    let local = chrono::DateTime::from_timestamp(now as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local))
        .unwrap_or_else(chrono::Local::now);
    let naive = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time");
    match naive.and_local_timezone(chrono::Local) {
        chrono::LocalResult::Single(dt) => dt.timestamp() as f64,
        chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp() as f64,
        chrono::LocalResult::None => local.timestamp() as f64,
    }
}

/// `range` 参数对应的下界（epoch 秒）；`None` 表示不设下界。
///
/// `today` 是本地日历日（本地零点到现在），不是滚动 24 小时；
/// `1d` 保持滚动 24 小时。
///
/// 认不出来的 range 抛错（Python `ValueError`）：静默忽略一个写错的筛选条件
/// 会让调用方以为「这几天没有失败」，而实际上它把全部结果都拿回去了。
pub fn range_cutoff(range_key: &str, now: f64) -> Result<Option<f64>, String> {
    let key = if range_key.trim().is_empty() {
        DEFAULT_RANGE
    } else {
        range_key.trim()
    }
    .to_lowercase();
    match key.as_str() {
        "today" => Ok(Some(local_midnight(now))),
        "1d" => Ok(Some(now - 86400.0)),
        "7d" => Ok(Some(now - 7.0 * 86400.0)),
        "30d" => Ok(Some(now - 30.0 * 86400.0)),
        "90d" => Ok(Some(now - 90.0 * 86400.0)),
        "all" => Ok(None),
        _ => Err(format!("range must be one of: {}", RANGES.join(", "))),
    }
}

/// 结果筛选值的归一（Python `RESULT_ALIASES`）。
///
/// 归一后的取值是 `"ok"` / `"failed"` / `""`（不筛）。
pub fn normalize_result(result: &str) -> Result<Option<String>, String> {
    let key = result.trim().to_lowercase();
    if key.is_empty() {
        return Ok(None);
    }
    match key.as_str() {
        "ok" | "success" | "succeeded" => Ok(Some("ok".to_string())),
        "failed" | "fail" | "failure" => Ok(Some("failed".to_string())),
        _ => Err("result must be one of: ok, failed".to_string()),
    }
}

/// 查询返回体。
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityQuery {
    pub rows: Vec<Value>,
    /// 命中总数（不受 `limit` 截断）。
    pub total: usize,
    pub limit: usize,
    pub range: String,
    pub filters: QueryFilters,
}

/// 查询用的筛选条件回显。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryFilters {
    pub uid: String,
    pub task: String,
    /// 归一后的结果筛选（`None` = 不筛）。
    pub result: Option<String>,
}

impl ActivityQuery {
    /// Python 返回体的键序：`rows` / `total` / `limit` / `range` / `filters`。
    pub fn to_json(&self) -> Value {
        json!({
            "rows": self.rows,
            "total": self.total,
            "limit": self.limit,
            "range": self.range,
            "filters": {
                "uid": self.filters.uid,
                "task": self.filters.task,
                "result": self.filters.result.clone().unwrap_or_default(),
            },
        })
    }
}

/// 读取历史（最新在前）。
///
/// 每一行都先投影到白名单再筛选/返回。文件是追加写的，追加序就是时间序，
/// 倒过来即最新在前。
pub fn query(
    rows: &[Value],
    range_key: &str,
    uid: &str,
    task: &str,
    result: &str,
    limit: Option<i64>,
    now: f64,
) -> Result<ActivityQuery, String> {
    let key = if range_key.trim().is_empty() {
        DEFAULT_RANGE
    } else {
        range_key.trim()
    }
    .to_lowercase();
    let cutoff = range_cutoff(&key, now)?;

    let task = task.trim();
    if !task.is_empty() && !TASKS.contains(&task) {
        return Err(format!("task must be one of: {}", TASKS.join(", ")));
    }
    let result_filter = normalize_result(result)?;

    let limit = match limit {
        Some(v) => (v.max(1) as usize).min(MAX_LIMIT),
        None => DEFAULT_LIMIT,
    };

    let uid = uid.trim();
    let mut matched: Vec<Value> = Vec::new();
    for raw in rows.iter().rev() {
        let row = project(raw);
        if !uid.is_empty() && row.get("uid").and_then(Value::as_str).unwrap_or("") != uid {
            continue;
        }
        if !task.is_empty() && row.get("task").and_then(Value::as_str).unwrap_or("") != task {
            continue;
        }
        if let Some(want) = &result_filter {
            let ok = row.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if ok != (want == "ok") {
                continue;
            }
        }
        if let Some(cutoff) = cutoff {
            let ts = row.get("ts").and_then(Value::as_str).and_then(parse_ts);
            // 有界区间里，时间读不出来或缺失的行不能算「在区间内」：
            // 那样一条坏行就能绕过 cutoff。`all` 没有下界，也就不存在绕过。
            match ts {
                None => continue,
                Some(ts) if ts < cutoff => continue,
                Some(_) => {}
            }
        }
        matched.push(row);
    }

    let total = matched.len();
    matched.truncate(limit);
    Ok(ActivityQuery {
        rows: matched,
        total,
        limit,
        range: key,
        filters: QueryFilters {
            uid: uid.to_string(),
            task: task.to_string(),
            result: result_filter,
        },
    })
}

/// 按保留策略整理历史，返回 `(保留行数, 丢弃行数)`。
///
/// 时间读不出来的行**不按「过期」丢**：那是一次无声的数据损失，
/// 宁可交给记录数上限去管它。
pub fn compact_rows(
    rows: &[Value],
    max_records: Option<usize>,
    max_age_days: Option<i64>,
    now: f64,
) -> (Vec<Value>, usize) {
    let max_records = max_records.unwrap_or(MAX_RECORDS);
    let max_age_days = max_age_days.unwrap_or(MAX_AGE_DAYS);
    if rows.is_empty() {
        return (Vec::new(), 0);
    }
    let cutoff = now - max_age_days as f64 * 86400.0;
    let mut keep: Vec<Value> = Vec::new();
    for row in rows {
        let ts = row.get("ts").and_then(Value::as_str).and_then(parse_ts);
        if let Some(ts) = ts {
            if ts < cutoff {
                continue;
            }
        }
        keep.push(row.clone());
    }
    if max_records > 0 && keep.len() > max_records {
        let drop = keep.len() - max_records;
        keep.drain(..drop);
    }
    let dropped = rows.len() - keep.len();
    (keep, dropped)
}

/// 历史文件路径。
pub fn history_path(usage_dir: &Path) -> PathBuf {
    usage_dir.join(HISTORY_NAME)
}

/// 读出全部原始行；坏行跳过，不让一条截断的行毁掉整个文件。
pub fn load_rows(path: &Path) -> Vec<Value> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.is_object())
        .collect()
}

/// 追加一行。**绝不抛错给调用方**：磁盘满、文件被占用都不该把一次成功的
/// 签到变成失败——返回 `Err` 供调用方记日志即可。
pub fn append_row(path: &Path, row: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let mut line = serde_json::to_string(row).map_err(|e| e.to_string())?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("打开失败: {e}"))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("写入失败: {e}"))
}

/// 原子重写历史文件（先写临时文件再替换）。
///
/// 任何时刻读到的都是一份完整文件，整理中途失败也不会碰原文件。
pub fn write_rows_atomic(path: &Path, rows: &[Value]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let mut buf = String::new();
    for row in rows {
        buf.push_str(&serde_json::to_string(row).map_err(|e| e.to_string())?);
        buf.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, buf.as_bytes()).map_err(|e| format!("写临时文件失败: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("替换失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_800_000_000.0;

    #[test]
    fn safe_message_collapses_whitespace_and_truncates() {
        assert_eq!(safe_message("  a \n b\t c  "), "a b c");
        // 「ab 」重复 200 次：压空白后 600 字，截到 200。
        let long = "ab ".repeat(200);
        assert_eq!(safe_message(&long).chars().count(), MAX_MESSAGE);
    }

    #[test]
    fn safe_message_redacts_known_secret_shapes() {
        // 期望值逐条对照 Python `wb_activity.safe_message` 的实测输出：
        // 六条形态按序打码，前一条的结果会被后一条继续匹配，所以
        // 「Authorization: Bearer …」整体被吃掉（Python 实测同为 "[redacted]"）。
        assert_eq!(
            safe_message("Authorization: Bearer abcdefgh12345678"),
            "[redacted]"
        );
        assert_eq!(safe_message("token=abcd1234efgh"), "[redacted]");
        assert_eq!(
            safe_message("eyJhbGciOiJIUzI1NiIs.abcdefgh12345678.abcdefgh"),
            "[redacted]"
        );
        assert_eq!(safe_message("sk-abcdefghijklmnop"), "[redacted]");
        assert_eq!(
            safe_message(&"a".repeat(32)),
            "[redacted]",
            "32 位十六进制串按密码形态打码"
        );
        // 40 位以上的 base64 形态同样被打码
        assert_eq!(
            safe_message(&"QWxhZGRpbjpvcGVuIHNlc2FtZQ".repeat(3)),
            "[redacted]"
        );
    }

    #[test]
    fn project_drops_extra_keys_and_normalizes_enums() {
        let row = json!({
            "ts": "2026-10-20T08:00:00+08:00",
            "uid": "u1",
            "nickname": "n",
            "realm": "xx",
            "task": "checkin",
            "trigger": "weird",
            "ok": "false",
            "message": "hi",
            "accessToken": "leak"
        });
        let out = project(&row);
        let obj = out.as_object().unwrap();
        assert_eq!(obj.len(), 8);
        assert!(obj.get("accessToken").is_none());
        assert_eq!(out["realm"], "");
        assert_eq!(out["trigger"], "unknown");
        // 字符串 "false" 不能读成成功
        assert_eq!(out["ok"], false);
    }

    #[test]
    fn project_reads_string_booleans_the_python_way() {
        assert_eq!(project(&json!({"ok": "success"}))["ok"], true);
        assert_eq!(project(&json!({"ok": "false"}))["ok"], false);
        assert_eq!(project(&json!({"ok": 1}))["ok"], true);
        assert_eq!(project(&json!({"ok": 0}))["ok"], false);
    }

    #[test]
    fn build_row_normalizes_unknown_realm_and_trigger() {
        let row = build_row(
            "2026-10-20T08:00:00+08:00",
            "u",
            "n",
            "jp",
            "checkin",
            "x",
            true,
            "ok",
        );
        assert_eq!(row["realm"], "");
        assert_eq!(row["trigger"], "unknown");
        assert_eq!(row["ok"], true);
    }

    #[test]
    fn range_cutoff_rejects_unknown_range() {
        assert!(range_cutoff("nope", NOW).is_err());
        assert_eq!(range_cutoff("all", NOW).unwrap(), None);
        assert!(range_cutoff("7d", NOW).unwrap().unwrap() < NOW);
        assert!(range_cutoff("today", NOW).unwrap().unwrap() <= NOW);
    }

    #[test]
    fn query_filters_and_limits() {
        let rows = vec![
            json!({"ts": "2026-10-20T08:00:00+08:00", "uid": "u1", "task": "checkin", "ok": true}),
            json!({"ts": "2026-10-19T08:00:00+08:00", "uid": "u1", "task": "checkin", "ok": false}),
            json!({"ts": "2026-10-18T08:00:00+08:00", "uid": "u2", "task": "daily_chat", "ok": true}),
        ];
        let q = query(&rows, "all", "u1", "", "", None, NOW).unwrap();
        assert_eq!(q.total, 2);
        assert_eq!(q.limit, DEFAULT_LIMIT);

        let q = query(&rows, "all", "", "checkin", "failed", None, NOW).unwrap();
        assert_eq!(q.total, 1);
        assert_eq!(q.rows[0]["ok"], false);

        let q = query(&rows, "all", "", "", "", Some(1), NOW).unwrap();
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.total, 3);
        assert_eq!(q.limit, 1);
    }

    #[test]
    fn query_rejects_unknown_task_and_result() {
        let rows: Vec<Value> = Vec::new();
        assert!(query(&rows, "all", "", "nope", "", None, NOW).is_err());
        assert!(query(&rows, "all", "", "", "weird", None, NOW).is_err());
    }

    #[test]
    fn query_excludes_rows_outside_a_bounded_range() {
        let rows = vec![json!({"ts": "2020-01-01T00:00:00+08:00", "uid": "u1"})];
        let q = query(&rows, "7d", "", "", "", None, NOW).unwrap();
        assert_eq!(q.total, 0);
        let q = query(&rows, "all", "", "", "", None, NOW).unwrap();
        assert_eq!(q.total, 1);
    }

    #[test]
    fn query_keeps_rows_with_unparsable_timestamps_out_of_bounded_ranges() {
        let rows = vec![json!({"ts": "not-a-time", "uid": "u1"})];
        let q = query(&rows, "7d", "", "", "", None, NOW).unwrap();
        assert_eq!(q.total, 0);
    }

    #[test]
    fn compact_drops_expired_and_truncates_to_the_record_cap() {
        let rows = vec![
            json!({"ts": "2020-01-01T00:00:00+08:00"}),
            json!({"ts": "not-a-time"}),
            json!({"ts": "2026-10-19T08:00:00+08:00"}),
        ];
        let (keep, dropped) = compact_rows(&rows, None, None, NOW);
        assert_eq!(keep.len(), 2);
        assert_eq!(dropped, 1);

        let (keep, dropped) = compact_rows(&rows, Some(1), None, NOW);
        assert_eq!(keep.len(), 1);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn jsonl_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = history_path(dir.path());
        let row = build_row(
            "2026-10-20T08:00:00+08:00",
            "u1",
            "nick",
            "cn",
            "checkin",
            "scheduler",
            true,
            "签到成功",
        );
        append_row(&path, &row).expect("append must succeed");
        let rows = load_rows(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["uid"], "u1");
    }

    #[test]
    fn load_rows_skips_broken_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = history_path(dir.path());
        fs::write(&path, "{\"uid\":\"a\"}\nnot json\n\n{\"uid\":\"b\"}\n")
            .expect("write must succeed");
        let rows = load_rows(&path);
        assert_eq!(rows.len(), 2);
    }
}
