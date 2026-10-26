//! 关卡 4.22（D72）：`/usage/remaining` 的剩余额度引擎移植。
//!
//! 权威源：Python `wb_proxy.py` 的 `_remaining_*` 系列（:2241-2947）+
//! `remaining_usage()` / `_remaining_payload_locked()`。
//!
//! ## 设计取舍
//!
//! Python 侧的增量折叠（checkpoint / tail 签名 / 断点续读）与 TTL 缓存全部是
//! **纯加速**——载荷的值与全量冷扫完全一致。本模块做**全量冷扫**：每次调用重新
//! fold 一遍两个文件（usage.jsonl + limit-events.jsonl），结果等价但实现更简单、
//! 更易单测；checkpoint 加速与 TTL 缓存留给后续关卡。
//!
//! ## 数据源
//!
//! - 用量行：`usage.jsonl`（[`crate::usage::load_usage_rows`]）
//! - 账号：调用方传入的 `pool: &[AccountState]`
//! - `estimate-daily.json`：`usage_dir` 下（Python `_estimate_daily_read`）
//! - `limit-events.jsonl`：`usage_dir` 下（Python 上游限流事件；本关只读，缺失按空）
//!
//! ## 输出形状
//!
//! 对齐 Python `_remaining_payload_locked`：per-account per-model 剩余额度载荷，
//! 顶层 `{window_seconds, generated_at, generated_iso, activity_floor,
//! activity_floor_iso, budgets[], models[], rows[]}`；面板据此渲染「剩余额度」视图。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use chrono::Duration;
use regex::Regex;
use serde_json::{json, Map, Value};
use wb_gateway::pool::AccountState;

// ---------------------------------------------------------------------------
// 常量（Python :1935-1961）
// ---------------------------------------------------------------------------

const LIMIT_WINDOW_SECONDS: f64 = 24.0 * 3600.0;
const REMAINING_KEEP_SECONDS: f64 = 26.0 * 3600.0;
const REMAINING_MAX_EVENTS: usize = 1000;
const ESTIMATE_WEIGHTS: [i64; 3] = [6, 3, 1];
const ESTIMATE_KEEP_DAYS: usize = 3;
const ESTIMATE_SEGMENT_DAYS: usize = 2;
const ESTIMATE_SCHEMA: i64 = 1;

/// 进程级「事件表已覆盖到哪个本地日」标记（Python `state["day"]` 的等价物）。
/// 跨天归档的触发依据；重启后为空 → 第一次构建按「刚过去的那天」试归档一次。
static DAY_MARKER: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

// ---------------------------------------------------------------------------
// 核心类型
// ---------------------------------------------------------------------------

/// 一个 (账号, 模型) 的用量缓冲：时间戳 + 累计和 两个并行列表。
///
/// 与 Python `_pair_buffer()` 同构：窗口求和是两次二分 + 一次相减（O(log n)），
/// 追加保持有序（O(n) 移动，缓冲只装几十到几百个样本）。
#[derive(Debug, Clone)]
struct PairBuffer {
    at: Vec<f64>,
    cum: Vec<f64>,
    head: usize,
    base: f64,
}

impl PairBuffer {
    fn new() -> Self {
        Self {
            at: Vec::new(),
            cum: Vec::new(),
            head: 0,
            base: 0.0,
        }
    }
}

/// 一条撞线事件（Python `_merge_event` 的事件 dict）。
#[derive(Debug, Clone)]
struct CapEvent {
    at: f64,
    reset: f64,
    account: String,
    model: String,
    realm: String,
    sample: i64,
    /// 来源："log"（usage.jsonl 里的 429 行）/"file"（limit-events.jsonl）。
    ///
    /// 冷扫版不重扫已读文件，`src` 只写不读；保留它是为了对齐 Python 事件表的
    /// 形状（增量版的 `_drop_events` 靠它清来源）。
    #[allow(dead_code)]
    src: String,
}

/// 折叠状态（Python `_remaining_state` 的子集；冷扫版不持久化 checkpoint）。
#[derive(Debug, Default)]
struct FoldState {
    events: Vec<CapEvent>,
    keys: HashSet<(String, String, i64)>,
    usage: HashMap<(String, String), PairBuffer>,
}

// ---------------------------------------------------------------------------
// 时间工具
// ---------------------------------------------------------------------------

fn local_day_key(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 本地日历日 `days_back` 天前的零点（Python `_local_midnight`）。
fn local_midnight_days_back(now: f64, days_back: i64) -> f64 {
    let local = chrono::DateTime::from_timestamp(now as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local))
        .unwrap_or_else(chrono::Local::now);
    let date = local.date_naive() - Duration::days(days_back);
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    match naive.and_local_timezone(chrono::Local) {
        chrono::LocalResult::Single(dt) => dt.timestamp() as f64,
        chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp() as f64,
        chrono::LocalResult::None => local.timestamp() as f64,
    }
}

/// 日键 "YYYY-MM-DD" → 该日本地零点（Python `_day_key_start`）。
fn day_key_start(key: &str) -> f64 {
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() != 3 {
        return 0.0;
    }
    let y: i32 = parts[0].parse().unwrap_or(0);
    let m: u32 = parts[1].parse().unwrap_or(1);
    let d: u32 = parts[2].parse().unwrap_or(1);
    let date = chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap_or_default();
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    match naive.and_local_timezone(chrono::Local) {
        chrono::LocalResult::Single(dt) => dt.timestamp() as f64,
        chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp() as f64,
        chrono::LocalResult::None => 0.0,
    }
}

fn iso_at(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// Python `round(x, 1)` 语义（十进制精确舍入、平局取偶）：面板显示用。
fn py_round1(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.1}").parse::<f64>().unwrap_or(value)
}

/// Python 真值语义近似（`None`/`False`/`0`/空串/空容器为假）。
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(num) => num.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// upstream 429 的恢复时刻解析（Python `parse_rate_limit_reset`，:12345-12374）
// ---------------------------------------------------------------------------

fn parse_rate_limit_reset(detail: &str) -> Option<f64> {
    if detail.is_empty() {
        return None;
    }
    let re = Regex::new(r"(?:reset at|将在)\s*(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})").ok()?;
    let caps = re.captures(detail)?;
    let stamp = caps.get(1)?.as_str().replace('T', " ");
    let tz_re = Regex::new(r"UTC([+-]\d{1,2})(?::?(\d{2}))?").ok()?;
    let mut offset: i64 = 0;
    if let Some(tz) = tz_re.captures(detail) {
        let hours: i64 = tz.get(1).unwrap().as_str().parse().unwrap_or(0);
        let minutes: i64 = tz.get(2).map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
        offset = hours * 3600 + if hours >= 0 { minutes * 60 } else { -minutes * 60 };
    }
    let naive = chrono::NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(naive.and_utc().timestamp() as f64 - offset as f64)
}

// ---------------------------------------------------------------------------
// 二分（Python bisect 语义）
// ---------------------------------------------------------------------------

/// 第一个 `>= x` 的下标（从 `lo` 起）。
fn bisect_left(slice: &[f64], x: f64, lo: usize) -> usize {
    lo + slice[lo..].partition_point(|&v| v < x)
}

/// 第一个 `> x` 的下标（从 `lo` 起）。
fn bisect_right(slice: &[f64], x: f64, lo: usize) -> usize {
    lo + slice[lo..].partition_point(|&v| v <= x)
}

// ---------------------------------------------------------------------------
// pair 缓冲（Python `_pair_buffer` / `_pair_append` / `_pair_trim` /
// `_range_sum` / `_range_sum_open`）
// ---------------------------------------------------------------------------

fn pair_append(buf: &mut PairBuffer, at: f64, tokens: i64) {
    if buf.at.is_empty() || at >= *buf.at.last().unwrap() {
        buf.at.push(at);
        let prev = buf.cum.last().copied().unwrap_or(0.0);
        buf.cum.push(prev + tokens as f64);
        return;
    }
    // 乱序插入：先看是否落在已裁掉的死前缀里，是则先压缩（base 绝对累计规则）。
    let index = bisect_right(&buf.at, at, 0);
    if index < buf.head {
        buf.base = buf.cum[buf.head - 1];
        buf.at.drain(..buf.head);
        buf.cum.drain(..buf.head);
        buf.head = 0;
    }
    let index = bisect_right(&buf.at, at, 0);
    buf.at.insert(index, at);
    let prev_cum = if index == 0 { buf.base } else { buf.cum[index - 1] };
    buf.cum.insert(index, prev_cum + tokens as f64);
    for i in (index + 1)..buf.cum.len() {
        buf.cum[i] += tokens as f64;
    }
}

fn pair_trim(buf: &mut PairBuffer, cutoff: f64) {
    while buf.head < buf.at.len() && buf.at[buf.head] < cutoff {
        buf.head += 1;
    }
    if buf.head > 512 && buf.head * 2 > buf.at.len() {
        buf.base = buf.cum[buf.head - 1];
        buf.at.drain(..buf.head);
        buf.cum.drain(..buf.head);
        buf.head = 0;
    }
}

/// `lo <= at <= hi` 的用量之和（Python `_range_sum`）。
fn range_sum(buf: &PairBuffer, lo: f64, hi: f64) -> i64 {
    if buf.head >= buf.at.len() {
        return 0;
    }
    let j = bisect_left(&buf.at, lo, buf.head);
    let k = bisect_right(&buf.at, hi, buf.head);
    if k <= j {
        return 0;
    }
    let low = if j > 0 { buf.cum[j - 1] } else { buf.base };
    (buf.cum[k - 1] - low) as i64
}

/// `lo <= at < hi` 的用量之和（日界用左闭右开，Python `_range_sum_open`）。
fn range_sum_open(buf: &PairBuffer, lo: f64, hi: f64) -> i64 {
    if buf.head >= buf.at.len() {
        return 0;
    }
    let j = bisect_left(&buf.at, lo, buf.head);
    let k = bisect_left(&buf.at, hi, buf.head);
    if k <= j {
        return 0;
    }
    let low = if j > 0 { buf.cum[j - 1] } else { buf.base };
    (buf.cum[k - 1] - low) as i64
}

// ---------------------------------------------------------------------------
// 撞线锚点（Python `_cap_anchor` / `_cap_anchors` / `_cap_window_start` /
// `_window_sample`）
// ---------------------------------------------------------------------------

/// 重置时刻 → 可哈希/可排序的整数键（毫秒截断；f64 不满足 Hash/Eq）。
fn reset_key(reset: f64) -> i64 {
    (reset * 1000.0) as i64
}

/// (账号, 模型) 的锚点表：reset_key → (at, reset)，按重置时刻去重、at 取较晚者。
fn cap_anchors(events: &[CapEvent], uid: &str, model: &str) -> HashMap<i64, (f64, f64)> {
    let mut entries: HashMap<i64, (f64, f64)> = HashMap::new();
    for e in events {
        if e.account != uid || e.model != model {
            continue;
        }
        match entries.get(&reset_key(e.reset)) {
            None => {
                entries.insert(reset_key(e.reset), (e.at, e.reset));
            }
            Some(&(old_at, _)) if e.at > old_at => {
                entries.insert(reset_key(e.reset), (e.at, e.reset));
            }
            _ => {}
        }
    }
    entries
}

/// 窗口起点：链上上一个重置时刻（落在 24h 内时），否则 `reset - 24h`。
///
/// 遍历 `values`（真 float 重置时刻）而非键，避免毫秒截断影响「上一个」判断。
fn cap_window_start(entries: &HashMap<i64, (f64, f64)>, reset: f64) -> f64 {
    let mut prev: f64 = 0.0;
    for &(_at, other_reset) in entries.values() {
        if other_reset < reset && other_reset > prev {
            prev = other_reset;
        }
    }
    prev.max(reset - LIMIT_WINDOW_SECONDS)
}

/// 撞线事件样本：窗口（截断起点到撞线时刻 `at`）内的用量。
fn window_sample(
    usage: &HashMap<(String, String), PairBuffer>,
    uid: &str,
    model: &str,
    reset_at: f64,
    at: f64,
    start: Option<f64>,
) -> i64 {
    let lo = start.unwrap_or(reset_at - LIMIT_WINDOW_SECONDS);
    match usage.get(&(uid.to_string(), model.to_string())) {
        Some(buf) => range_sum(buf, lo, at),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// 事件表（Python `_merge_event` / `_drop_events`）
// ---------------------------------------------------------------------------

/// 按 (账号, 模型, 重置时刻) 去重追加一条事件；首个来源胜出。
fn merge_event(state: &mut FoldState, event: CapEvent) {
    if event.account.is_empty() || event.model.is_empty() || event.reset <= 0.0 {
        return;
    }
    let key = (event.account.clone(), event.model.clone(), reset_key(event.reset));
    if state.keys.contains(&key) {
        return;
    }
    state.keys.insert(key);
    state.events.push(event);
    if state.events.len() > REMAINING_MAX_EVENTS {
        // 从头部丢弃时同步清去重键。
        let drop_n = state.events.len() - REMAINING_MAX_EVENTS;
        let dropped: Vec<CapEvent> = state.events.drain(..drop_n).collect();
        for old in dropped {
            state.keys.remove(&(old.account, old.model, reset_key(old.reset)));
        }
    }
}

/// 忘掉某个来源贡献的全部事件（Python `_drop_events`）。
///
/// 冷扫版每次从零重新 fold，不需要在「文件被重写」时清来源；保留此函数仅作
/// Python 语义对照，供未来增量版使用。
#[allow(dead_code)]
fn drop_events(state: &mut FoldState, source: &str) {
    let kept: Vec<CapEvent> = state
        .events
        .iter()
        .filter(|e| e.src != source)
        .cloned()
        .collect();
    for e in &state.events {
        if e.src == source {
            state.keys.remove(&(e.account.clone(), e.model.clone(), reset_key(e.reset)));
        }
    }
    state.events = kept;
}

// ---------------------------------------------------------------------------
// 折叠（Python `_fold_remaining`）
// ---------------------------------------------------------------------------

fn fold_remaining_row(row: &Value, state: &mut FoldState) {
    let uid = row.get("account").and_then(Value::as_str).unwrap_or("").to_string();
    let model = row.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    if uid.is_empty() || model.is_empty() {
        return;
    }
    let at = row.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    if at == 0.0 {
        return;
    }
    // 撞线行（status 429）：解析恢复时刻，样本按段截断。
    if row.get("status").and_then(Value::as_i64).unwrap_or(0) == 429 {
        let Some(reset) = parse_rate_limit_reset(row.get("message").and_then(Value::as_str).unwrap_or(""))
        else {
            return;
        };
        let anchors = cap_anchors(&state.events, &uid, &model);
        let sample = window_sample(&state.usage, &uid, &model, reset, at, Some(cap_window_start(&anchors, reset)));
        merge_event(
            state,
            CapEvent {
                at,
                reset,
                account: uid,
                model,
                realm: row.get("realm").and_then(Value::as_str).unwrap_or("").to_string(),
                sample,
                src: "log".to_string(),
            },
        );
        return;
    }
    if row.get("error").is_some_and(truthy) || row.get("usage_missing").is_some_and(truthy) {
        return;
    }
    if crate::usage::row_outcome(row) == "client_aborted" {
        return;
    }
    let total_tokens = row.get("total_tokens").and_then(Value::as_i64).unwrap_or(0);
    let buf = state.usage.entry((uid, model)).or_insert_with(PairBuffer::new);
    pair_append(buf, at, total_tokens);
    pair_trim(buf, at - REMAINING_KEEP_SECONDS);
}

// ---------------------------------------------------------------------------
// estimate-daily.json（Python `_estimate_daily_read` / `_estimate_daily_write`）
// ---------------------------------------------------------------------------

/// 读出 estimate-daily.json 的 days 表；坏文件/读不到按空表处理。
fn estimate_daily_read(usage_dir: &Path) -> Map<String, Value> {
    let path = usage_dir.join("estimate-daily.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Map::new(),
    };
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Map::new(),
    };
    if !data.is_object() || data.get("schema").and_then(Value::as_i64) != Some(ESTIMATE_SCHEMA) {
        return Map::new();
    }
    match data.get("days") {
        Some(Value::Object(days)) => days.clone(),
        _ => Map::new(),
    }
}

/// 原子写 estimate-daily.json（同目录 .tmp + rename）。
fn estimate_daily_write(usage_dir: &Path, days: &Map<String, Value>) {
    let _ = std::fs::create_dir_all(usage_dir);
    let path = usage_dir.join("estimate-daily.json");
    let tmp = usage_dir.join("estimate-daily.json.tmp");
    let text = serde_json::to_string(&json!({"schema": ESTIMATE_SCHEMA, "days": days})).unwrap_or_default();
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 归档文件里一天的口径是否算数（n > 0 且 avg/min/max 都是数字）。
fn estimate_budget_entry_ok(entry: &Value) -> bool {
    if !entry.is_object() {
        return false;
    }
    let n = entry.get("n").and_then(Value::as_i64).unwrap_or(0);
    if n <= 0 {
        return false;
    }
    ["avg", "min", "max"].iter().all(|field| {
        entry.get(*field).and_then(Value::as_f64).map(|v| v.is_finite()).unwrap_or(false)
    })
}

/// 估算窗口的日键：今天 / 昨天 / 前天（Python `_estimate_day_keys`）。
fn estimate_day_keys(now: f64) -> Vec<String> {
    (0..ESTIMATE_KEEP_DAYS as i64)
        .map(|back| local_day_key(local_midnight_days_back(now, back)))
        .collect()
}

/// `{账号: [样本]}` → `{avg, min, max, n}`（Python `_budget_means`）。
fn budget_means(per_account: &HashMap<String, Vec<i64>>) -> Option<Value> {
    let means: Vec<i64> = per_account
        .values()
        .map(|v| (v.iter().sum::<i64>() as f64 / v.len() as f64).round() as i64)
        .collect();
    if means.is_empty() {
        return None;
    }
    let avg = (means.iter().sum::<i64>() as f64 / means.len() as f64).round() as i64;
    Some(json!({
        "avg": avg,
        "min": means.iter().min().copied().unwrap_or(0),
        "max": means.iter().max().copied().unwrap_or(0),
        "n": means.len(),
    }))
}

// ---------------------------------------------------------------------------
// 跨天归档（Python `_remaining_archive_due` / `_remaining_archive_day` /
// `_remaining_archive_segments`）
// ---------------------------------------------------------------------------

fn remaining_archive_segments(state: &FoldState, events: &[&CapEvent], start: f64, end: f64) -> Map<String, Value> {
    // (账号, 模型) → [(at, reset)]：仅当天的事件。
    let mut caps: HashMap<(String, String), Vec<(f64, f64)>> = HashMap::new();
    for e in events {
        caps.entry((e.account.clone(), e.model.clone())).or_default().push((e.at, e.reset));
    }
    let mut segments: Map<String, Value> = Map::new();
    for ((uid, model), buf) in &state.usage {
        let total = range_sum_open(buf, start, end);
        let pair_caps = caps.get(&(uid.clone(), model.clone())).cloned().unwrap_or_default();
        if total <= 0 && pair_caps.is_empty() {
            continue;
        }
        let mut post: i64 = 0;
        for &(_at, reset) in &pair_caps {
            if !(start <= reset && reset < end) {
                continue;
            }
            let mut nxt = end;
            for &(other_at, _other_reset) in &pair_caps {
                if reset < other_at && other_at < nxt {
                    nxt = other_at;
                }
            }
            post += range_sum_open(buf, reset, nxt);
        }
        if total <= 0 && post <= 0 {
            continue;
        }
        segments.insert(format!("{uid}\x1f{model}"), json!({"pre": total - post, "post": post}));
    }
    segments
}

/// 一天的量归档：预算口径 + 每个组合的截断前后分段；两样都空返回 `None`。
fn remaining_archive_day(
    state: &FoldState,
    day: &str,
    realm_of: &HashMap<String, String>,
) -> Option<Value> {
    let start = day_key_start(day);
    let end = start + 86400.0;
    let day_events: Vec<&CapEvent> = state.events.iter().filter(|e| local_day_key(e.at) == day).collect();

    let mut samples: HashMap<(String, String), HashMap<String, Vec<i64>>> = HashMap::new();
    for e in &day_events {
        if e.sample <= 0 {
            continue;
        }
        let realm = if e.realm.is_empty() {
            realm_of.get(&e.account).cloned().unwrap_or_default()
        } else {
            e.realm.clone()
        };
        samples.entry((realm, e.model.clone())).or_default().entry(e.account.clone()).or_default().push(e.sample);
    }
    let mut budgets: Map<String, Value> = Map::new();
    for (key, per_account) in &samples {
        if let Some(entry) = budget_means(per_account) {
            budgets.insert(format!("{}\x1f{}", key.0, key.1), entry);
        }
    }
    let segments = remaining_archive_segments(state, &day_events, start, end);
    if budgets.is_empty() && segments.is_empty() {
        return None;
    }
    Some(json!({"budgets": budgets, "segments": segments}))
}

// ---------------------------------------------------------------------------
// 预算估计（Python `_remaining_budgets`）
// ---------------------------------------------------------------------------

/// (realm, model) → 近 3 天加权的窗口预算估计 `{avg, min, max, n, days}`。
fn remaining_budgets(
    events: &[CapEvent],
    realm_of: &HashMap<String, String>,
    now: f64,
    daily: &Map<String, Value>,
) -> HashMap<(String, String), Value> {
    let keys = estimate_day_keys(now);
    // per_day: (realm, model) → {day → {avg,min,max,n}}
    let mut per_day: HashMap<(String, String), Map<String, Value>> = HashMap::new();

    // 今天：内存里的事件现算。
    let mut samples: HashMap<(String, String), HashMap<String, Vec<i64>>> = HashMap::new();
    for e in events {
        if e.sample <= 0 || local_day_key(e.at) != keys[0] {
            continue;
        }
        let realm = if e.realm.is_empty() {
            realm_of.get(&e.account).cloned().unwrap_or_default()
        } else {
            e.realm.clone()
        };
        samples.entry((realm, e.model.clone())).or_default().entry(e.account.clone()).or_default().push(e.sample);
    }
    for (key, per_account) in &samples {
        if let Some(entry) = budget_means(per_account) {
            per_day.entry(key.clone()).or_default().insert(keys[0].clone(), entry);
        }
    }
    // 昨天/前天：直接读归档文件里当天的口径。
    for day in &keys[1..] {
        let Some(record) = daily.get(day) else { continue };
        let Some(budgets_obj) = record.get("budgets").and_then(Value::as_object) else { continue };
        for (pair_key, entry) in budgets_obj {
            if !estimate_budget_entry_ok(entry) {
                continue;
            }
            let mut parts = pair_key.splitn(2, '\x1f');
            let realm = parts.next().unwrap_or("").to_string();
            let model = parts.next().unwrap_or("").to_string();
            if model.is_empty() {
                continue;
            }
            per_day.entry((realm, model)).or_default().insert(day.clone(), entry.clone());
        }
    }

    let mut budgets: HashMap<(String, String), Value> = HashMap::new();
    for (key, days_map) in &per_day {
        let mut rows: Vec<(String, Value)> = Vec::new();
        for (index, day) in keys.iter().enumerate() {
            let Some(entry) = days_map.get(day) else { continue };
            if entry.get("n").and_then(Value::as_i64).unwrap_or(0) <= 0 {
                continue;
            }
            rows.push((
                day.clone(),
                json!({
                    "day": day,
                    "w": ESTIMATE_WEIGHTS[index],
                    "avg": entry.get("avg").and_then(Value::as_f64).unwrap_or(0.0).round() as i64,
                    "n": entry.get("n").and_then(Value::as_i64).unwrap_or(0),
                }),
            ));
        }
        if rows.is_empty() {
            continue;
        }
        let weight: i64 = rows.iter().map(|r| r.1.get("w").and_then(Value::as_i64).unwrap_or(0)).sum();
        if weight == 0 {
            continue;
        }
        let mut min_val = i64::MAX;
        let mut max_val = i64::MIN;
        let mut total_n: i64 = 0;
        let mut weighted_avg: f64 = 0.0;
        for (day, row) in &rows {
            let w = row.get("w").and_then(Value::as_i64).unwrap_or(0);
            weighted_avg += w as f64 * row.get("avg").and_then(Value::as_i64).unwrap_or(0) as f64;
            total_n += row.get("n").and_then(Value::as_i64).unwrap_or(0);
            if let Some(entry) = days_map.get(day) {
                let mn = entry.get("min").and_then(Value::as_f64).unwrap_or(0.0).round() as i64;
                let mx = entry.get("max").and_then(Value::as_f64).unwrap_or(0.0).round() as i64;
                min_val = min_val.min(mn);
                max_val = max_val.max(mx);
            }
        }
        let days_out: Vec<Value> = rows.into_iter().map(|r| r.1).collect();
        budgets.insert(
            key.clone(),
            json!({
                "avg": (weighted_avg / weight as f64).round() as i64,
                "min": if min_val == i64::MAX { 0 } else { min_val },
                "max": if max_val == i64::MIN { 0 } else { max_val },
                "n": total_n,
                "days": days_out,
            }),
        );
    }
    budgets
}

// ---------------------------------------------------------------------------
// 载荷构建（Python `_remaining_payload_locked`）
// ---------------------------------------------------------------------------

/// 撞线锚点：事件日志 + 账号 `model_caps`，按 (账号, 模型, 重置时刻) 去重。
fn collect_anchors(
    state: &FoldState,
    accounts: &[&AccountState],
) -> HashMap<(String, String), HashMap<i64, (f64, f64)>> {
    let mut anchors: HashMap<(String, String), HashMap<i64, (f64, f64)>> = HashMap::new();
    for e in &state.events {
        let entry = anchors.entry((e.account.clone(), e.model.clone())).or_default();
        match entry.get(&reset_key(e.reset)) {
            None => {
                entry.insert(reset_key(e.reset), (e.at, e.reset));
            }
            Some(&(old_at, _)) if e.at > old_at => {
                entry.insert(reset_key(e.reset), (e.at, e.reset));
            }
            _ => {}
        }
    }
    for a in accounts {
        for (model, cap) in &a.model_caps {
            let entry = anchors.entry((a.uid.clone(), model.clone())).or_default();
            match entry.get(&reset_key(cap.reset)) {
                None => {
                    entry.insert(reset_key(cap.reset), (cap.at, cap.reset));
                }
                Some(&(old_at, _)) if cap.at > old_at => {
                    entry.insert(reset_key(cap.reset), (cap.at, cap.reset));
                }
                _ => {}
            }
        }
    }
    anchors
}

/// 按需过滤之后的活跃 (realm, model) 集合（昨天 00:00 后有消耗/撞线）。
fn active_set(
    state: &FoldState,
    realm_of: &HashMap<String, String>,
    anchors: &HashMap<(String, String), HashMap<i64, (f64, f64)>>,
    floor: f64,
) -> HashSet<(String, String)> {
    let mut active: HashSet<(String, String)> = HashSet::new();
    for ((uid, model), buf) in &state.usage {
        let Some(realm) = realm_of.get(uid) else { continue };
        if buf.at.is_empty() {
            continue;
        }
        if *buf.at.last().unwrap() >= floor {
            active.insert((realm.clone(), model.clone()));
        }
    }
    for ((uid, model), entries) in anchors {
        let Some(realm) = realm_of.get(uid) else { continue };
        if entries.values().any(|e| e.0.max(e.1) >= floor) {
            active.insert((realm.clone(), model.clone()));
        }
    }
    active
}

/// `_remaining_payload_locked` 的纯函数版：调用方持有折叠好的状态与预算表。
fn payload_build(
    state: &FoldState,
    now: f64,
    realm_of: &HashMap<String, String>,
    accounts: &[&AccountState],
    budgets: &HashMap<(String, String), Value>,
    active: &HashSet<(String, String)>,
    floor: f64,
) -> Value {
    let mut iso_cache: HashMap<i64, String> = HashMap::new();
    let mut iso_at_cached = |ts: f64| -> String {
        let key = ts as i64;
        if let Some(v) = iso_cache.get(&key) {
            return v.clone();
        }
        let v = iso_at(ts);
        iso_cache.insert(key, v.clone());
        v
    };

    let anchors = collect_anchors(state, accounts);
    let mut rows: Vec<Value> = Vec::new();
    let mut totals: HashMap<(String, String), Map<String, Value>> = HashMap::new();

    for a in accounts {
        let uid = a.uid.clone();
        let realm = realm_of.get(&uid).cloned().unwrap_or_default();
        let mut models: HashSet<String> = HashSet::new();
        for ((r, m), _) in budgets {
            if *r == realm && active.contains(&(r.clone(), m.clone())) {
                models.insert(m.clone());
            }
        }
        for ((u, m), _) in &state.usage {
            if *u == uid {
                let key = (realm.clone(), m.clone());
                if active.contains(&key) {
                    models.insert(m.clone());
                }
            }
        }
        let mut sorted_models: Vec<String> = models.into_iter().collect();
        sorted_models.sort();

        for model in sorted_models {
            let entries = anchors.get(&(uid.clone(), model.clone())).cloned().unwrap_or_default();
            let buf = state.usage.get(&(uid.clone(), model.clone()));
            let mut past: Vec<(f64, f64)> = Vec::new();
            let mut future: Vec<(f64, f64)> = Vec::new();
            for e in entries.values() {
                if e.1 <= now {
                    past.push((e.0, e.1));
                } else {
                    future.push((e.0, e.1));
                }
            }
            let last = past.iter().max_by(|x, y| x.1.partial_cmp(&y.1).unwrap()).copied();
            let upcoming = future.iter().min_by(|x, y| x.1.partial_cmp(&y.1).unwrap()).copied();

            let mut segments: Vec<Value> = Vec::new();
            let mut cooling = false;
            let mut estimated = false;
            let (used, window_start, reset_at) = if let Some(up) = upcoming {
                cooling = true;
                let reset_at = up.1;
                let ws = cap_window_start(&entries, reset_at);
                let cap_time = if up.0 > 0.0 { up.0 } else { reset_at };
                let end = now.min(cap_time);
                let used = buf.map(|b| range_sum(b, ws, end)).unwrap_or(0);
                segments.push(json!({
                    "start": ws,
                    "end": end,
                    "used": used,
                    "capped": true,
                    "current": true,
                    "start_iso": iso_at_cached(ws),
                    "end_iso": iso_at_cached(end),
                }));
                (used, ws, Some(reset_at))
            } else if let Some(la) = last {
                let reset_at = la.1;
                let ws = reset_at.max(now - LIMIT_WINDOW_SECONDS);
                let used = buf.map(|b| range_sum(b, ws, now)).unwrap_or(0);
                let est = reset_at < now - LIMIT_WINDOW_SECONDS;
                if !est {
                    if la.0 > 0.0 {
                        let prev_end = la.0.min(reset_at);
                        let prev_start = cap_window_start(&entries, reset_at);
                        let prev_used = buf.map(|b| range_sum(b, prev_start, prev_end)).unwrap_or(0);
                        segments.push(json!({
                            "start": prev_start,
                            "end": prev_end,
                            "used": prev_used,
                            "capped": true,
                            "current": false,
                            "start_iso": iso_at_cached(prev_start),
                            "end_iso": iso_at_cached(prev_end),
                        }));
                    }
                    segments.push(json!({
                        "start": reset_at,
                        "end": now,
                        "used": used,
                        "capped": false,
                        "current": true,
                        "start_iso": iso_at_cached(reset_at),
                        "end_iso": iso_at_cached(now),
                    }));
                }
                estimated = est;
                (used, ws, Some(reset_at))
            } else {
                let ws = now - LIMIT_WINDOW_SECONDS;
                let used = buf.map(|b| range_sum(b, ws, now)).unwrap_or(0);
                estimated = true;
                (used, ws, None)
            };

            let budget = budgets.get(&(realm.clone(), model.clone()));
            let avg = budget.and_then(|b| b.get("avg")).and_then(Value::as_i64).unwrap_or(0);
            let mut remaining: Option<i64> = None;
            let mut pct: Option<f64> = None;
            if cooling {
                remaining = Some(0);
                if avg != 0 {
                    pct = Some(py_round1(used as f64 * 100.0 / avg as f64));
                }
            } else if budget.is_some() {
                remaining = Some((avg - used).max(0));
                if avg != 0 {
                    pct = Some(py_round1(used as f64 * 100.0 / avg as f64));
                }
            }

            rows.push(json!({
                "uid": uid,
                "nickname": "",
                "realm": realm,
                "model": model,
                "used": used,
                "budget": budget.and_then(|b| b.get("avg")).and_then(Value::as_i64),
                "budget_min": budget.and_then(|b| b.get("min")).and_then(Value::as_i64),
                "budget_max": budget.and_then(|b| b.get("max")).and_then(Value::as_i64),
                "samples": budget.map(|b| b.get("n").and_then(Value::as_i64).unwrap_or(0)).unwrap_or(0),
                "remaining": remaining,
                "used_pct": pct,
                "window_start": window_start,
                "window_iso": iso_at_cached(window_start),
                "reset_at": reset_at,
                "reset_iso": reset_at.map(|r| iso_at_cached(r)),
                "cooling": cooling,
                "estimated": estimated,
                "segments": segments,
            }));

            let total_entry = totals.entry((realm.clone(), model.clone())).or_insert_with(|| {
                let mut m = Map::new();
                m.insert("realm".to_string(), Value::String(realm.clone()));
                m.insert("model".to_string(), Value::String(model.clone()));
                m.insert("total_remaining".to_string(), Value::Null);
                m.insert("budget".to_string(), budget.and_then(|b| b.get("avg")).cloned().unwrap_or(Value::Null));
                m.insert("pairs".to_string(), Value::from(0i64));
                m.insert("available".to_string(), Value::from(0i64));
                m.insert("cooling".to_string(), Value::from(0i64));
                m.insert("unknown".to_string(), Value::from(0i64));
                m.insert("estimated".to_string(), Value::from(0i64));
                m
            });
            let pairs = total_entry.get("pairs").and_then(Value::as_i64).unwrap_or(0) + 1;
            total_entry.insert("pairs".to_string(), Value::from(pairs));
            if cooling {
                let c = total_entry.get("cooling").and_then(Value::as_i64).unwrap_or(0) + 1;
                total_entry.insert("cooling".to_string(), Value::from(c));
            }
            if estimated {
                let e = total_entry.get("estimated").and_then(Value::as_i64).unwrap_or(0) + 1;
                total_entry.insert("estimated".to_string(), Value::from(e));
            }
            if let Some(r) = remaining {
                let cur = total_entry.get("total_remaining").and_then(Value::as_i64).unwrap_or(0);
                total_entry.insert("total_remaining".to_string(), Value::from(cur + r));
                if !cooling {
                    let av = total_entry.get("available").and_then(Value::as_i64).unwrap_or(0) + 1;
                    total_entry.insert("available".to_string(), Value::from(av));
                }
            } else {
                let u = total_entry.get("unknown").and_then(Value::as_i64).unwrap_or(0) + 1;
                total_entry.insert("unknown".to_string(), Value::from(u));
            }
        }
    }

    // 行排序：Python key `(remaining is None, remaining or 0, realm, nickname, model)`。
    rows.sort_by(|a, b| {
        let a_none = a.get("remaining").map(|v| v.is_null()).unwrap_or(true);
        let b_none = b.get("remaining").map(|v| v.is_null()).unwrap_or(true);
        a_none.cmp(&b_none).then_with(|| {
            let a_val = a.get("remaining").and_then(Value::as_i64).unwrap_or(0);
            let b_val = b.get("remaining").and_then(Value::as_i64).unwrap_or(0);
            a_val.cmp(&b_val)
        }).then_with(|| {
            a.get("realm").and_then(Value::as_str).unwrap_or("").cmp(b.get("realm").and_then(Value::as_str).unwrap_or(""))
        }).then_with(|| {
            a.get("nickname").and_then(Value::as_str).unwrap_or("").cmp(b.get("nickname").and_then(Value::as_str).unwrap_or(""))
        }).then_with(|| {
            a.get("model").and_then(Value::as_str).unwrap_or("").cmp(b.get("model").and_then(Value::as_str).unwrap_or(""))
        })
    });

    let mut budgets_arr: Vec<Value> = budgets
        .iter()
        .map(|((realm, model), b)| {
            let mut bb = b.clone();
            if let Some(obj) = bb.as_object_mut() {
                obj.insert("realm".to_string(), Value::String(realm.clone()));
                obj.insert("model".to_string(), Value::String(model.clone()));
                obj.insert("active".to_string(), Value::Bool(active.contains(&(realm.clone(), model.clone()))));
            }
            bb
        })
        .collect();
    budgets_arr.sort_by(|a, b| {
        a.get("realm").and_then(Value::as_str).unwrap_or("")
            .cmp(b.get("realm").and_then(Value::as_str).unwrap_or(""))
            .then_with(|| {
                a.get("model").and_then(Value::as_str).unwrap_or("")
                    .cmp(b.get("model").and_then(Value::as_str).unwrap_or(""))
            })
    });

    // models：Python key `(total_remaining is None, -(total_remaining or 0), realm, model)`。
    let mut models_arr: Vec<Value> = totals.values().cloned().map(Value::Object).collect();
    models_arr.sort_by(|a, b| {
        let a_none = a.get("total_remaining").map(|v| v.is_null()).unwrap_or(true);
        let b_none = b.get("total_remaining").map(|v| v.is_null()).unwrap_or(true);
        a_none.cmp(&b_none).then_with(|| {
            let a_val = a.get("total_remaining").and_then(Value::as_i64).unwrap_or(0);
            let b_val = b.get("total_remaining").and_then(Value::as_i64).unwrap_or(0);
            b_val.cmp(&a_val)
        }).then_with(|| {
            a.get("realm").and_then(Value::as_str).unwrap_or("").cmp(b.get("realm").and_then(Value::as_str).unwrap_or(""))
        }).then_with(|| {
            a.get("model").and_then(Value::as_str).unwrap_or("").cmp(b.get("model").and_then(Value::as_str).unwrap_or(""))
        })
    });

    json!({
        "window_seconds": LIMIT_WINDOW_SECONDS,
        "generated_at": now,
        "generated_iso": iso_at_cached(now),
        "activity_floor": floor,
        "activity_floor_iso": iso_at_cached(floor),
        "budgets": budgets_arr,
        "models": models_arr,
        "rows": rows,
    })
}

// ---------------------------------------------------------------------------
// 主入口（Python `_remaining_payload` + `remaining_usage` 的冷扫版）
// ---------------------------------------------------------------------------

/// `/usage/remaining` 的剩余额度载荷。
///
/// 每次调用做全量冷扫：fold `usage.jsonl` + `limit-events.jsonl`、读
/// `estimate-daily.json`、必要时做跨天归档（原子写盘）、构建 per-account
/// per-model 剩余额度载荷。
pub async fn remaining_payload(accounts_dir: &Path, usage_dir: &Path, pool: &[AccountState]) -> Value {
    let _ = accounts_dir; // 约定签名，接线侧后续按需使用
    let now = crate::activity::now_epoch();

    // 折叠状态（冷扫：不采用 checkpoint）。
    let mut state = FoldState::default();

    // 用量行 fold（Python 先折 log，再折 file）。
    let rows = crate::usage::load_usage_rows(usage_dir).unwrap_or_default();
    for row in &rows {
        fold_remaining_row(row, &mut state);
    }
    // limit-events.jsonl（只读；缺失按空）。
    let events_text = std::fs::read_to_string(usage_dir.join("limit-events.jsonl")).unwrap_or_default();
    for line in events_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.is_object() {
                merge_event(&mut state, event_from_file_row(&v));
            }
        }
    }

    // 启用账号 → realm 映射。
    let accounts: Vec<&AccountState> = pool.iter().filter(|a| a.enabled && !a.uid.is_empty()).collect();
    let mut realm_of: HashMap<String, String> = HashMap::new();
    for a in &accounts {
        realm_of.insert(a.uid.clone(), a.realm.clone());
    }

    // 跨天前先释放借用的临时函数，再归档。
    // 本地日期与进程标记不同（或重启后为空）时，把刚过去的那天归档。
    let mut day_marker = DAY_MARKER.lock().unwrap().clone();
    let today = local_day_key(now);
    let archived = if day_marker == today {
        None
    } else {
        if day_marker.is_empty() || day_marker >= today {
            day_marker = local_day_key(local_midnight_days_back(now, 1));
        }
        if day_marker == today {
            None
        } else {
            remaining_archive_day(&state, &day_marker, &realm_of).map(|record| {
                let mut m = Map::new();
                m.insert(day_marker.clone(), record);
                m
            })
        }
    };
    // 无论归档成没成，标记推进到今天。
    *DAY_MARKER.lock().unwrap() = today;

    // 归档视图：文件表 + 本次归档（已归档的天不被本次重算盖掉）。
    let daily_read = estimate_daily_read(usage_dir);
    let daily_view = if let Some(arch) = &archived {
        let mut view = daily_read.clone();
        for (day, rec) in arch {
            view.entry(day.clone()).or_insert(rec.clone());
        }
        view
    } else {
        daily_read.clone()
    };

    // 预算与活跃集。
    let budgets = remaining_budgets(&state.events, &realm_of, now, &daily_view);
    let anchors = collect_anchors(&state, &accounts);
    let floor = crate::activity::local_midnight(now) - LIMIT_WINDOW_SECONDS;
    let active = active_set(&state, &realm_of, &anchors, floor);

    let payload = payload_build(&state, now, &realm_of, &accounts, &budgets, &active, floor);

    // 跨天归档写盘（原子写）：内容不变则跳过。
    if let Some(arch) = &archived {
        let mut days = daily_read.clone();
        for (day, rec) in arch {
            days.entry(day.clone()).or_insert(rec.clone());
        }
        let keep = estimate_day_keys(now);
        let segment_days: HashSet<String> = keep.iter().take(ESTIMATE_SEGMENT_DAYS).cloned().collect();
        let mut pruned: Map<String, Value> = Map::new();
        for (day, rec) in &days {
            if !keep.contains(day) {
                continue;
            }
            let mut r = rec.clone();
            if !segment_days.contains(day) {
                if let Some(obj) = r.as_object_mut() {
                    obj.remove("segments");
                }
            }
            pruned.insert(day.clone(), r);
        }
        // 幂等：与盘上内容一致的写盘是纯写放大——先比对再写。
        let path = usage_dir.join("estimate-daily.json");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let text = serde_json::to_string(&json!({"schema": ESTIMATE_SCHEMA, "days": pruned})).unwrap_or_default();
        if !existing.trim_end_matches('\n').is_empty() && existing.trim() == text.trim() {
            // 内容不变，跳过写盘。
        } else if existing.is_empty() || existing.trim() != text.trim() {
            estimate_daily_write(usage_dir, &pruned);
        }
    }

    payload
}

// 折 limit-events 行 → CapEvent（文件事件原样带 sample）。
fn event_from_file_row(v: &Value) -> CapEvent {
    CapEvent {
        at: v.get("at").and_then(Value::as_f64).unwrap_or(0.0),
        reset: v.get("reset").and_then(Value::as_f64).unwrap_or(0.0),
        account: v.get("account").and_then(Value::as_str).unwrap_or("").to_string(),
        model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
        realm: v.get("realm").and_then(Value::as_str).unwrap_or("").to_string(),
        sample: v.get("sample").and_then(Value::as_i64).unwrap_or(0),
        src: "file".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一条撞线事件（样本固定 400）。
    fn mk_event(state: &mut FoldState, at: f64, reset: f64, account: &str, model: &str) {
        merge_event(
            state,
            CapEvent {
                at,
                reset,
                account: account.into(),
                model: model.into(),
                realm: "cn".into(),
                sample: 400,
                src: "file".into(),
            },
        );
    }

    fn buf_from(pairs: &[(f64, i64)]) -> PairBuffer {
        let mut b = PairBuffer::new();
        for &(at, tokens) in pairs {
            pair_append(&mut b, at, tokens);
        }
        b
    }

    #[test]
    fn range_sum_obeys_inclusive_bounds() {
        let b = buf_from(&[(10.0, 100), (20.0, 50), (30.0, 25)]);
        assert_eq!(range_sum(&b, 10.0, 10.0), 100);
        assert_eq!(range_sum(&b, 10.0, 20.0), 150);
        assert_eq!(range_sum(&b, 11.0, 19.0), 0);
        assert_eq!(range_sum(&b, 15.0, 35.0), 75);
        assert_eq!(range_sum_open(&b, 10.0, 20.0), 100);
        assert_eq!(range_sum_open(&b, 10.0, 30.0), 150);
    }

    #[test]
    fn out_of_order_append_stays_sorted() {
        let mut b = PairBuffer::new();
        // 模拟两条请求同时完成、at 相差不足 1 秒的乱序写入。
        pair_append(&mut b, 30.0, 25);
        pair_append(&mut b, 10.0, 100);
        pair_append(&mut b, 20.0, 50);
        assert_eq!(b.at, vec![10.0, 20.0, 30.0]);
        assert_eq!(range_sum(&b, 10.0, 30.0), 175);
        assert_eq!(range_sum(&b, 20.0, 30.0), 75);
    }

    #[test]
    fn trim_keeps_absolute_base_so_sums_do_not_go_negative() {
        let mut d = PairBuffer::new();
        for i in 0..514 {
            pair_append(&mut d, i as f64, 1);
        }
        d.head = 513;
        pair_trim(&mut d, 500.0);
        // 存活区只剩 at=513 一行，base=513 保持绝对累计。
        assert_eq!(d.head, 0);
        assert_eq!(d.base, 513.0);
        assert_eq!(range_sum(&d, 513.0, 514.0), 1);
        assert_eq!(range_sum_open(&d, 513.0, 514.0), 1);
        // 左端完全落在已裁区 ⇒ 0 而不是负值（真机 -1293.5M 的回归点）。
        assert_eq!(range_sum(&d, 0.0, 512.0), 0);
        assert_eq!(range_sum(&d, 500.0, 600.0), 1);
    }

    #[test]
    fn parse_reset_handles_both_realms() {
        let intl = "usage will reset at 2026-09-19 18:29:03 UTC+8";
        let cn = "将在 2026-10-09 14:44:59 UTC+8 重置";
        let a = parse_rate_limit_reset(intl).unwrap();
        let b = parse_rate_limit_reset(cn).unwrap();
        // 与 Python 同口径：UTC 墙上时钟 − 声明偏移。
        assert_eq!(a, chrono::NaiveDateTime::parse_from_str("2026-09-19 18:29:03", "%Y-%m-%d %H:%M:%S").unwrap().and_utc().timestamp() as f64 - 8.0 * 3600.0);
        assert_eq!(b, chrono::NaiveDateTime::parse_from_str("2026-10-09 14:44:59", "%Y-%m-%d %H:%M:%S").unwrap().and_utc().timestamp() as f64 - 8.0 * 3600.0);
        assert!(parse_rate_limit_reset("upstream is tired").is_none());
        assert!(parse_rate_limit_reset("").is_none());
    }

    #[test]
    fn cap_window_start_uses_previous_reset_inside_window() {
        let curr = 1_800_000_000.0;
        let prev = curr - 5.0 * 3600.0; // 上一个重置（5h 前，落在窗口内）
        let mut entries: HashMap<i64, (f64, f64)> = HashMap::new();
        entries.insert(reset_key(prev), (prev, prev));
        entries.insert(reset_key(curr), (curr, curr));
        // 链式撞线：窗口起点是上一个重置时刻，而不是 reset-24h。
        assert_eq!(cap_window_start(&entries, curr), prev);
        entries.remove(&reset_key(prev));
        assert_eq!(cap_window_start(&entries, curr), curr - LIMIT_WINDOW_SECONDS);
    }

    #[test]
    fn archive_day_builds_budget_and_segments() {
        let now = 1_800_000_000.0;
        let day = local_day_key(now);
        let mut st = FoldState::default();
        mk_event(&mut st, now - 10.0, 100.0, "a1", "m1");
        mk_event(&mut st, now - 20.0, 200.0, "a2", "m1");
        let mut realm_of = HashMap::new();
        realm_of.insert("a1".into(), "cn".into());
        realm_of.insert("a2".into(), "cn".into());
        let rec = remaining_archive_day(&st, &day, &realm_of).unwrap();
        let budgets = rec["budgets"].as_object().unwrap();
        assert!(budgets.contains_key("cn\x1fm1"));
        // 两个账号各一个样本 400 → 账号均值 400 → 跨账号均值 400，n = 账号数 2。
        assert_eq!(budgets["cn\x1fm1"]["avg"], 400);
        assert_eq!(budgets["cn\x1fm1"]["n"], 2);
    }

    #[test]
    fn payload_build_cooling_row_is_zero_remaining() {
        let now = 1_800_000_000.0;
        let mut state = FoldState::default();
        // 组合 (a1, m1) 已经撞线：upcoming 锚点 reset > now。
        mk_event(&mut state, now - 100.0, now + 3600.0, "a1", "m1");
        // 用量：撞线前 100 token。
        state.usage.insert(("a1".into(), "m1".into()), buf_from(&[(now - 200.0, 60), (now - 50.0, 40)]));
        let mut realm_of = HashMap::new();
        realm_of.insert("a1".into(), "cn".into());
        let acct = AccountState::new("a1", "cn");
        let account_refs = vec![&acct];
        let mut budgets = HashMap::new();
        budgets.insert(
            ("cn".into(), "m1".into()),
            json!({"avg": 100, "min": 80, "max": 120, "n": 2, "days": []}),
        );
        let floor = crate::activity::local_midnight(now) - LIMIT_WINDOW_SECONDS;
        let active = active_set(&state, &realm_of, &collect_anchors(&state, &account_refs), floor);
        let payload = payload_build(&state, now, &realm_of, &account_refs, &budgets, &active, floor);
        let rows = payload["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["uid"], "a1");
        assert_eq!(rows[0]["model"], "m1");
        assert_eq!(rows[0]["remaining"], 0);
        assert_eq!(rows[0]["cooling"], true);
        assert_eq!(rows[0]["realm"], "cn");
        // 窗口起点 = reset-24h（没有链上前一个重置）。
        assert_eq!(rows[0]["window_start"], now + 3600.0 - LIMIT_WINDOW_SECONDS);
        assert!(rows[0]["budget"].as_i64().unwrap() > 0);
    }

    #[test]
    fn payload_build_estimated_row_without_budget_keeps_unknown() {
        let now = 1_800_000_000.0;
        let mut state = FoldState::default();
        state.usage.insert(("a1".into(), "m1".into()), buf_from(&[(now - 100.0, 30)]));
        let mut realm_of = HashMap::new();
        realm_of.insert("a1".into(), "cn".into());
        let acct = AccountState::new("a1", "cn");
        let account_refs = vec![&acct];
        let budgets = HashMap::new(); // 没有预算（never hit a cap）
        let floor = crate::activity::local_midnight(now) - LIMIT_WINDOW_SECONDS;
        // 活跃集：只有用量行（or anchors 都为空）
        let active = active_set(&state, &realm_of, &collect_anchors(&state, &account_refs), floor);
        let payload = payload_build(&state, now, &realm_of, &account_refs, &budgets, &active, floor);
        let rows = payload["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["remaining"], Value::Null);
        assert_eq!(rows[0]["estimated"], true);
        assert_eq!(rows[0]["cooling"], false);
        assert_eq!(rows[0]["budget"], Value::Null);
        assert_eq!(payload["models"][0]["unknown"], 1);
        assert_eq!(payload["models"][0]["available"], 0);
        assert_eq!(payload["budgets"].as_array().unwrap().len(), 0);
    }
}