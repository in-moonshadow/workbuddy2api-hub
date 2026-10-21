//! 关卡 4.19（线 C）：用量落盘与聚合。
//!
//! 权威源：Python `wb_proxy.py` 的 `record_usage`（:665-740）与
//! `_persist_usage`（:743-757）；聚合侧是 `usage_snapshot`（:3057）、
//! `_usage_snapshot_uncached`（:4340）、`_fold_usage_snapshot`（:3899）、
//! `usage_by_account`（:6005）、`_fold_usage_by_account`（:6042）与
//! `compute_usage_analytics`（:6133）。
//!
//! 落盘：**每个请求一行 JSONL**，请求路径内**同步 append**，没有后台线程。
//! Python `_persist_usage` 的注释把理由写得很清楚：早先每个请求都重写一遍
//! `usage-summary.json`（整份 `json.dumps` + 临时文件 + `os.replace`），
//! 而树里没有任何代码读那个文件（每个聚合都重读 `usage.jsonl`）——纯开销。
//!
//! ⚠️ **这个日志文件是 Python 与 Rust 共用的同一个文件**：
//! Python 用脚本同级的 `usage/`（`wb_proxy.py:402-403`），Rust 用
//! `accounts/` 同级的 `usage/`（`api_routes::usage_dir_from`）——同一个仓库
//! 布局下就是同一个 `usage/usage.jsonl`。所以**行的语义**必须与 Python 一致
//! （见 [`row_outcome`]），否则两个实例互相把对方的行读错。
//!
//! 本模块是**最小可用版**：逐行累加，**不做增量折叠、不做 TTL 缓存、不做
//! ETag**（Python 侧那些是纯加速，留到后续关卡）。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use crate::activity;

/// 用量日志文件名（Python `wb_proxy.USAGE_LOG`：`usage/usage.jsonl`）。
pub const USAGE_LOG_NAME: &str = "usage.jsonl";

/// Python `wb_proxy.USAGE_FIELDS`（:407-408）里 Rust 侧折叠的 5 个 token 字段。
///
/// Python 还折 `credit`（计费口径）；Rust 侧定价表未接入 ⇒ **不折**，
/// 记入本关卡的差异清单。
const TOKEN_FIELDS: [&str; 5] = [
    "prompt_tokens",
    "completion_tokens",
    "reasoning_tokens",
    "cached_tokens",
    "total_tokens",
];

/// 用量日志路径（Python `USAGE_LOG`）。
pub fn usage_log_path(usage_dir: &Path) -> PathBuf {
    usage_dir.join(USAGE_LOG_NAME)
}

/// 追加一行用量（每请求一行，Python `_persist_usage`）。
///
/// 复用 [`activity::append_row`]：原子 JSONL append + 父目录自动创建 +
/// **失败只返回 `Err` 不抛**（磁盘满不该把一次成功的请求变成失败）。
pub fn append_usage_row(usage_dir: &Path, row: &Value) -> Result<(), String> {
    activity::append_row(&usage_log_path(usage_dir), row)
}

/// 读出全部用量行。
///
/// **日志不存在 ⇒ 空向量**（不是错误）：新装的实例 `/usage` 应当回一份空
/// 聚合，而不是 500。真读失败（权限、IO 错误）才回 `Err`。
pub fn load_usage_rows(usage_dir: &Path) -> Result<Vec<Value>, String> {
    let path = usage_log_path(usage_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(parse_jsonl(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(format!("读用量日志失败: {err}")),
    }
}

/// 逐行解析 JSONL。坏行跳过——不让一条被截断的行毁掉整个文件
/// （与 `activity::load_rows` 同策略）。
fn parse_jsonl(text: &str) -> Vec<Value> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.is_object())
        .collect()
}

/// 一行的终态（Python `row_outcome`，:624-634）。
///
/// `outcome` 有值就用它；没有就回退到 `error` 字段——老行只有
/// `error`/`status`：`error` 为真 ⇒ `failed`，否则 `completed`。
///
/// ⚠️ **词表必须是 Python 的 `completed` / `failed`**：Python 的
/// `_fold_usage_snapshot`（:3954）把 `outcome != "completed"` 一律当失败，
/// 而 `row_outcome` 原样返回这个字段。日志是共用的，写 `ok` 会被 Python
/// 记成失败，成功率直接算错。
pub fn row_outcome(row: &Value) -> String {
    if let Some(outcome) = row.get("outcome").and_then(Value::as_str) {
        if !outcome.is_empty() {
            return outcome.to_string();
        }
    }
    if row.get("error").is_some_and(truthy) {
        "failed".to_string()
    } else {
        "completed".to_string()
    }
}

/// Python 的真值语义近似：`None` / `False` / `0` / 空串 / 空容器为假。
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

/// 读一行的 token 字段（缺失按 0 计，Python `row.get(k) or 0`）。
fn row_tokens(row: &Value) -> [i64; 5] {
    let mut out = [0i64; 5];
    for (index, name) in TOKEN_FIELDS.iter().enumerate() {
        out[index] = row
            .get(*name)
            .and_then(Value::as_f64)
            .map(|value| value as i64)
            .unwrap_or(0);
    }
    out
}

/// 一行的模型（Python `model or "unknown"`）。
fn model_of(row: &Value) -> String {
    row.get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// 一行是否属于某个 realm（Python `row_matches_realm`，:519-520）。
///
/// `realm` 为空或 `"all"` ⇒ **不过滤**（Python `realm_scope` 把 `"all"`
/// 归一成 `None`，即「所有 realm」；直接透传会让每一行都不匹配）。
///
/// Python 的 `row_realm`（:503-518）是「行里的 realm → 账号的 realm →
/// 模型的归属 realm」的推断链。Rust 侧两个实现落盘的行**恒带 `realm` 字段**
/// （Python `record_usage` :708、Rust `build_usage_row` 都写），所以推断链在
/// 实践中用不到；只有更老的行没有该字段——那种行**保留**（无法证明它属于
/// 别的 realm）。
pub fn realm_matches(row: &Value, realm: &str) -> bool {
    if realm.is_empty() || realm == "all" {
        return true;
    }
    match row.get("realm").and_then(Value::as_str) {
        Some(row_realm) if !row_realm.is_empty() => row_realm == realm,
        _ => true,
    }
}

/// 一行的账号（Python `row.get("account") or "(unattributed)"`）。
fn account_of(row: &Value) -> String {
    row.get("account")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|account| !account.is_empty())
        .unwrap_or("(unattributed)")
        .to_string()
}

/// 一行的日期（`YYYY-MM-DD`）。
///
/// 优先取 `iso` 的前 10 个字符；`iso` 缺失或太短时回退到 `at`（epoch 秒）
/// 按本地时区折算。
fn row_date(row: &Value) -> String {
    if let Some(iso) = row.get("iso").and_then(Value::as_str) {
        let head: String = iso.trim().chars().take(10).collect();
        if head.len() == 10 {
            return head;
        }
    }
    row.get("at")
        .and_then(Value::as_f64)
        .map(local_date)
        .unwrap_or_default()
}

/// epoch 秒 → 本地日历日（`YYYY-MM-DD`）。
pub fn local_date(epoch_secs: f64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// epoch 秒 → 本地时间字符串（`%Y-%m-%d %H:%M:%S`）。
///
/// ⚠️ 与 Python 行里的 `iso` 差一个分隔符：Python `record_usage` 写的是
/// `%Y-%m-%dT%H:%M:%S`（`T` 分隔），本关卡的规范要求 `%Y-%m-%d %H:%M:%S`
/// （空格分隔，与 Python `usage_snapshot` 的 `since` 同款）。
/// 影响面极小：Python 只把 `iso` 当展示字符串透传
/// （`wb_proxy.py:7685`、`:8981`），**没有任何聚合按 `iso` 分组**
/// （日桶一律按 `at` 折算）。
pub fn local_iso(epoch_secs: f64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
}

/// 从上游 `usage` 块抽出本侧追踪的字段（Python `_extract_usage`，:487-501）。
///
/// `usage` 不是对象（或缺失）⇒ 空对象（调用方据此打 `usage_missing`）。
/// `cached_tokens` 取**别名里第一个正值**（Python `_best_cached_tokens`，
/// :635-662），零值别名不遮蔽真命中。
pub fn extract_usage(usage: Option<&Value>) -> Value {
    let Some(usage) = usage else {
        return json!({});
    };
    let Some(map) = usage.as_object() else {
        return json!({});
    };
    let prompt_details = map
        .get("prompt_tokens_details")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let input_details = map
        .get("input_tokens_details")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let completion_details = map
        .get("completion_tokens_details")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut out = Map::new();
    out.insert(
        "prompt_tokens".into(),
        Value::from(number_of(map.get("prompt_tokens"))),
    );
    out.insert(
        "completion_tokens".into(),
        Value::from(number_of(map.get("completion_tokens"))),
    );
    out.insert(
        "reasoning_tokens".into(),
        Value::from(number_of(completion_details.get("reasoning_tokens"))),
    );
    out.insert(
        "cached_tokens".into(),
        Value::from(best_cached_tokens(
            prompt_details.get("cached_tokens"),
            map.get("prompt_cache_hit_tokens"),
            map.get("cache_read_input_tokens"),
            map.get("cached_tokens"),
            input_details.get("cached_tokens"),
            completion_details.get("cached_tokens"),
        )),
    );
    out.insert(
        "total_tokens".into(),
        Value::from(number_of(map.get("total_tokens"))),
    );
    out.insert(
        "credit".into(),
        map.get("credit").cloned().unwrap_or(Value::from(0)),
    );
    Value::Object(out)
}

/// 数值取整（Python `int(...)` 的截断语义）。
fn number_of(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_f64).map(|f| f as i64).unwrap_or(0)
}

/// 别名里第一个正值即缓存命中数（Python `_best_cached_tokens`）。
fn best_cached_tokens(
    prompt_details: Option<&Value>,
    prompt_cache_hit: Option<&Value>,
    cache_read_input: Option<&Value>,
    cached: Option<&Value>,
    input_details: Option<&Value>,
    completion_details: Option<&Value>,
) -> i64 {
    for candidate in [
        prompt_details,
        prompt_cache_hit,
        cache_read_input,
        cached,
        input_details,
        completion_details,
    ] {
        let value = number_of(candidate);
        if value > 0 {
            return value;
        }
    }
    0
}

/// 一个聚合桶：计数 + token 累加 + 两个「分组 → 次数」子表。
#[derive(Default)]
struct Bucket {
    /// 全部行数（成功 + 失败）。
    total_rows: i64,
    /// 成功行数（Python 的 `requests`）。
    requests: i64,
    /// 失败行数。
    errors: i64,
    /// 5 个 token 字段的累加。
    tokens: [i64; 5],
    /// 账号 → 次数（`by_model` 桶里的 `accounts`）。
    accounts: Map<String, Value>,
    /// 模型 → 次数（`by_account` 桶里的 `models`）。
    models: Map<String, Value>,
}

impl Bucket {
    /// 折一行**成功**的请求（Python `_fold_usage_snapshot` 的成功分支）。
    fn fold_completed(&mut self, row: &Value) {
        self.total_rows += 1;
        self.requests += 1;
        self.add_tokens(row);
        let account = row
            .get("account")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(account) = account {
            bump(&mut self.accounts, account);
        }
    }

    /// 折一行**失败**的请求。
    fn fold_failed(&mut self) {
        self.total_rows += 1;
        self.errors += 1;
    }

    /// 折一行**不分成功失败**的请求（Python `_fold_usage_by_account`：
    /// 它只跳过带 `error` 的行，不看 `outcome`）。
    fn fold_any(&mut self, row: &Value) {
        self.total_rows += 1;
        self.requests += 1;
        self.add_tokens(row);
        bump(&mut self.models, &model_of(row));
    }

    fn add_tokens(&mut self, row: &Value) {
        let row_tokens = row_tokens(row);
        for (index, value) in row_tokens.iter().enumerate() {
            self.tokens[index] += value;
        }
    }

    /// token 字段 + 计数（统计汇总用）。
    fn to_totals(&self) -> Value {
        let mut map = Map::new();
        map.insert("requests".into(), Value::from(self.requests));
        for (index, name) in TOKEN_FIELDS.iter().enumerate() {
            map.insert((*name).into(), Value::from(self.tokens[index]));
        }
        Value::Object(map)
    }
}

/// 子表计数 +1（保持插入序）。
fn bump(table: &mut Map<String, Value>, key: &str) {
    let current = table.get(key).and_then(Value::as_i64).unwrap_or(0);
    table.insert(key.to_string(), Value::from(current + 1));
}

/// 保序的「键 → 桶」表。
///
/// `serde_json::Map` 只能装 `Value`，装不下 [`Bucket`]；而 Python 的
/// `by_model` / `by_account` 是 **dict**，键序 = 首次出现顺序（响应字节的
/// 一部分）。故用 `Vec` 自保序，而不是换成 `BTreeMap`（那会按字典序排，
/// 与 Python 的行序不同）。
struct Buckets {
    items: Vec<(String, Bucket)>,
}

impl Buckets {
    fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// 取（必要时建）某个键的桶。
    fn entry(&mut self, key: String) -> &mut Bucket {
        if let Some(index) = self.items.iter().position(|(name, _)| name == &key) {
            return &mut self.items[index].1;
        }
        self.items.push((key, Bucket::default()));
        &mut self.items.last_mut().expect("刚 push 过一行，不会为空").1
    }
}

/// 用量快照（Python `usage_snapshot` / `_usage_snapshot_uncached` 的**最小可用版**）。
///
/// 逐行累加，**不做增量折叠也不做 TTL 缓存**（缓存留后续关卡）。
///
/// 与 Python 对齐的键：`requests` / `errors` / `prompt_tokens` /
/// `completion_tokens` / `reasoning_tokens` / `cached_tokens` / `total_tokens` /
/// `by_model`。`by_model` 的桶形状与键序也一致：
/// `{requests, accounts, prompt_tokens, completion_tokens, reasoning_tokens,
/// cached_tokens, total_tokens}`。
///
/// ⚠️ 与 Python 的差异（详见关卡 4.19 差异清单）：
/// - `total_requests` 是 Rust 侧新增的别名：**全部行数**（成功 + 失败）；
///   Python 的 `requests` 只数成功行。
/// - 没有 `started` / `since` / `usd_cny` / `accounts_map` / `account`
///   （进程级信息与定价汇率，Rust 侧未接入）。
/// - 没有 `by_model_realm` / `by_model_acct`（三维分组）与
///   `cost_cny` / `cost_missing`（定价表未接入）。
pub fn build_usage_snapshot(rows: &[Value]) -> Value {
    let mut totals = Bucket::default();
    let mut by_model = Buckets::new();

    for row in rows {
        if row_outcome(row) != "completed" {
            totals.fold_failed();
            continue;
        }
        totals.fold_completed(row);
        let model = model_of(row);
        by_model.entry(model).fold_completed(row);
    }

    let mut out = Map::new();
    out.insert("requests".into(), Value::from(totals.requests));
    out.insert("total_requests".into(), Value::from(totals.total_rows));
    out.insert("errors".into(), Value::from(totals.errors));
    for (index, name) in TOKEN_FIELDS.iter().enumerate() {
        out.insert((*name).into(), Value::from(totals.tokens[index]));
    }
    let mut models = Map::new();
    for (name, bucket) in &by_model.items {
        // 键序对齐 Python 的桶字面量（`_fold_usage_snapshot`）：
        // `{"requests", "accounts", ...USAGE_FIELDS}` —— `accounts` 在 token
        // 字段**之前**。键序是响应字节的一部分，不能随手排。
        let mut value = Map::new();
        value.insert("requests".into(), Value::from(bucket.requests));
        value.insert("accounts".into(), Value::Object(bucket.accounts.clone()));
        for (index, field) in TOKEN_FIELDS.iter().enumerate() {
            value.insert((*field).into(), Value::from(bucket.tokens[index]));
        }
        models.insert(name.clone(), Value::Object(value));
    }
    out.insert("by_model".into(), Value::Object(models));
    out.insert("realm".into(), Value::from("all"));
    Value::Object(out)
}

/// 按账号聚合（Python `usage_by_account` / `_usage_by_account_uncached`）。
///
/// 返回 Python 的信封 `{"accounts": [...]}`：一个按 `total_tokens` 降序的桶
/// 列表，每桶的 `models` 是按次数降序取前 5 的 `[模型, 次数]` 列表。
/// 另附 `by_account`（账号 → 同一个桶的对象），供按键直查。
///
/// ⚠️ 与 Python 的差异：桶里没有 `credit`（定价表未接入）。
pub fn build_usage_by_account(rows: &[Value]) -> Value {
    let mut buckets = Buckets::new();
    for row in rows {
        // Python `_fold_usage_by_account` 只跳过带 `error` 的行。
        if row.get("error").is_some_and(truthy) {
            continue;
        }
        let key = account_of(row);
        buckets.entry(key).fold_any(row);
    }

    // Python：`sorted(buckets.values(), key=lambda b: -b["total_tokens"])`
    // ——排序键只有 total_tokens，同值时保持插入序（Python 的 sort 稳定）。
    let mut ordered: Vec<(String, &Bucket)> =
        buckets.items.iter().map(|(k, v)| (k.clone(), v)).collect();
    ordered.sort_by(|left, right| right.1.tokens[4].cmp(&left.1.tokens[4]));

    let mut list = Vec::with_capacity(ordered.len());
    let mut by_account = Map::new();
    for (name, bucket) in &ordered {
        // 键序对齐 Python 的桶字面量（`_fold_usage_by_account`）：
        // `{"account", "requests", prompt_tokens, completion_tokens,
        //   reasoning_tokens, cached_tokens, total_tokens, "models"}`。
        let mut value = Map::new();
        value.insert("account".into(), Value::from(name.clone()));
        value.insert("requests".into(), Value::from(bucket.requests));
        for (index, field) in TOKEN_FIELDS.iter().enumerate() {
            value.insert((*field).into(), Value::from(bucket.tokens[index]));
        }
        value.insert("models".into(), models_list(&bucket.models));
        let value = Value::Object(value);
        list.push(value.clone());
        by_account.insert(name.clone(), value);
    }

    let mut out = Map::new();
    out.insert("accounts".into(), Value::Array(list));
    out.insert("by_account".into(), Value::Object(by_account));
    Value::Object(out)
}

/// 模型 → 次数的列表（Python `sorted(items(), key=-count)[:5]`）。
fn models_list(models: &Map<String, Value>) -> Value {
    let mut ordered: Vec<(&String, &Value)> = models.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .1
            .as_i64()
            .unwrap_or(0)
            .cmp(&left.1.as_i64().unwrap_or(0))
    });
    Value::Array(
        ordered
            .into_iter()
            .take(5)
            .map(|(name, count)| json!([name, count.as_i64().unwrap_or(0)]))
            .collect(),
    )
}

/// 用量分析（Python `compute_usage_analytics` 的**最小可用版**）。
///
/// 按日期（`iso` 前 10 字符，缺 `iso` 时按 `at` 的本地日）分组的请求数与
/// token，另附账号/模型两个分组与一份汇总。
///
/// ⚠️ 与 Python 的差异（Python 的形状复杂得多）：
/// - Python 是 `{"window", "realm", "usd_cny", "summary": {"window",
///   "all_time"}, "accounts", "models", "keys"}`，`summary` 里有 window /
///   all_time 两份；Rust 侧只有一份不分窗口的 `summary`。
/// - Python 的 `accounts` / `models` 元素是 `{"window":…, "all_time":…}`
///   双层结构；Rust 侧是单层桶。
/// - 没有 `keys`（Rust 侧拿不到 Python 的 settings key id）与 `usd_cny`。
/// - `by_date` 是 Rust 侧新增的（Python 的日桶只在窗口聚合内部用，不外露）。
pub fn build_usage_analytics(rows: &[Value]) -> Value {
    let mut totals = Bucket::default();
    let mut dates = Buckets::new();

    for row in rows {
        let date = row_date(row);
        if row_outcome(row) == "completed" {
            totals.fold_completed(row);
            dates.entry(date).fold_completed(row);
        } else {
            totals.fold_failed();
            dates.entry(date).fold_failed();
        }
    }

    let mut by_date = Map::new();
    for (date, bucket) in &dates.items {
        let mut value = match bucket.to_totals() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        value.insert("date".into(), Value::from(date.clone()));
        value.insert("total_requests".into(), Value::from(bucket.total_rows));
        value.insert("errors".into(), Value::from(bucket.errors));
        by_date.insert(date.clone(), Value::Object(value));
    }

    let mut summary = match totals.to_totals() {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    summary.insert("total_requests".into(), Value::from(totals.total_rows));
    summary.insert("errors".into(), Value::from(totals.errors));

    let by_account = build_usage_by_account(rows);
    let snapshot = build_usage_snapshot(rows);

    let mut out = Map::new();
    out.insert("realm".into(), Value::from("all"));
    out.insert(
        "window".into(),
        json!({"since": Value::Null, "until": Value::Null}),
    );
    out.insert("summary".into(), Value::Object(summary));
    out.insert("by_date".into(), Value::Object(by_date));
    out.insert(
        "accounts".into(),
        by_account
            .get("accounts")
            .cloned()
            .unwrap_or(Value::Array(vec![])),
    );
    out.insert(
        "models".into(),
        snapshot
            .get("by_model")
            .cloned()
            .unwrap_or(Value::Object(Map::new())),
    );
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(model: &str, account: &str, total: i64, iso: &str) -> Value {
        json!({
            "at": 1_700_000_000.0,
            "iso": iso,
            "model": model,
            "stream": false,
            "outcome": "completed",
            "elapsed_ms": 120,
            "prompt_tokens": total / 2,
            "completion_tokens": total - total / 2,
            "total_tokens": total,
            "account": account,
            "realm": "cn",
            "key": "",
        })
    }

    #[test]
    fn load_rows_missing_file_is_empty_not_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let rows = load_usage_rows(dir.path()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn append_row_appends_and_never_overwrites() {
        let dir = tempfile::TempDir::new().unwrap();
        append_usage_row(dir.path(), &row("m", "a", 10, "2026-10-21 10:00:00")).unwrap();
        append_usage_row(dir.path(), &row("m", "a", 20, "2026-10-21 10:01:00")).unwrap();
        let rows = load_usage_rows(dir.path()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["total_tokens"], 10);
        assert_eq!(rows[1]["total_tokens"], 20);
    }

    #[test]
    fn snapshot_counts_requests_models_and_tokens() {
        let rows = vec![
            row("m1", "a1", 10, "2026-10-21 10:00:00"),
            row("m1", "a2", 20, "2026-10-21 10:01:00"),
            row("m2", "a1", 30, "2026-10-22 10:01:00"),
        ];
        let snap = build_usage_snapshot(&rows);
        assert_eq!(snap["requests"], 3);
        assert_eq!(snap["total_requests"], 3);
        assert_eq!(snap["errors"], 0);
        assert_eq!(snap["total_tokens"], 60);
        assert_eq!(snap["by_model"]["m1"]["requests"], 2);
        assert_eq!(snap["by_model"]["m1"]["total_tokens"], 30);
        assert_eq!(snap["by_model"]["m2"]["requests"], 1);
        assert_eq!(snap["by_model"]["m1"]["accounts"]["a1"], 1);
    }

    #[test]
    fn snapshot_counts_failed_rows_as_errors() {
        let mut failed = row("m1", "a1", 10, "2026-10-21 10:00:00");
        failed["outcome"] = json!("failed");
        failed["error"] = json!(true);
        let rows = vec![row("m1", "a1", 10, "2026-10-21 10:00:00"), failed];
        let snap = build_usage_snapshot(&rows);
        assert_eq!(snap["requests"], 1);
        assert_eq!(snap["errors"], 1);
        assert_eq!(snap["total_requests"], 2);
        assert_eq!(snap["total_tokens"], 10);
    }

    #[test]
    fn by_account_groups_by_account_and_sorts_by_tokens() {
        let rows = vec![
            row("m1", "a1", 10, "2026-10-21 10:00:00"),
            row("m1", "a1", 5, "2026-10-21 10:01:00"),
            row("m2", "a2", 100, "2026-10-21 10:02:00"),
        ];
        let out = build_usage_by_account(&rows);
        let accounts = out["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0]["account"], "a2");
        assert_eq!(accounts[0]["requests"], 1);
        assert_eq!(accounts[0]["total_tokens"], 100);
        assert_eq!(accounts[1]["account"], "a1");
        assert_eq!(accounts[1]["requests"], 2);
        assert_eq!(accounts[1]["models"][0][0], "m1");
        assert_eq!(accounts[1]["models"][0][1], 2);
        assert_eq!(out["by_account"]["a1"]["total_tokens"], 15);
    }

    #[test]
    fn by_account_skips_error_rows_like_python() {
        let mut failed = row("m1", "a1", 10, "2026-10-21 10:00:00");
        failed["error"] = json!(true);
        let rows = vec![row("m1", "a1", 10, "2026-10-21 10:00:00"), failed];
        let out = build_usage_by_account(&rows);
        assert_eq!(out["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(out["accounts"][0]["requests"], 1);
    }

    #[test]
    fn analytics_groups_by_date() {
        let rows = vec![
            row("m1", "a1", 10, "2026-10-21 10:00:00"),
            row("m2", "a1", 20, "2026-10-21 23:00:00"),
            row("m1", "a1", 30, "2026-10-22 01:00:00"),
        ];
        let out = build_usage_analytics(&rows);
        assert_eq!(out["by_date"]["2026-10-21"]["requests"], 2);
        assert_eq!(out["by_date"]["2026-10-21"]["total_tokens"], 30);
        assert_eq!(out["by_date"]["2026-10-21"]["date"], "2026-10-21");
        assert_eq!(out["by_date"]["2026-10-22"]["requests"], 1);
        assert_eq!(out["summary"]["requests"], 3);
        assert_eq!(out["summary"]["total_tokens"], 60);
    }

    #[test]
    fn row_outcome_falls_back_to_error_field() {
        assert_eq!(row_outcome(&json!({"error": true})), "failed");
        assert_eq!(row_outcome(&json!({})), "completed");
        assert_eq!(row_outcome(&json!({"outcome": "failed"})), "failed");
    }

    #[test]
    fn extract_usage_picks_best_cached_alias() {
        let usage = json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 4},
            "cached_tokens": 0,
        });
        let out = extract_usage(Some(&usage));
        assert_eq!(out["prompt_tokens"], 10);
        assert_eq!(out["completion_tokens"], 5);
        assert_eq!(out["total_tokens"], 15);
        assert_eq!(out["cached_tokens"], 4);
        assert_eq!(out["reasoning_tokens"], 0);
    }

    #[test]
    fn extract_usage_missing_is_empty() {
        assert_eq!(extract_usage(None), json!({}));
        assert_eq!(extract_usage(Some(&json!("x"))), json!({}));
    }
}
