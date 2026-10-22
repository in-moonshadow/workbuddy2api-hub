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
//!
//! ## 关卡 4.20（线 B）新增
//!
//! - [`build_usage_snapshot_with`]：快照 + Python `_usage_snapshot_uncached`
//!   的装饰键（`started` / `since` / `log_file` / `realm` / `accounts_map` /
//!   `account`；`usd_cny` 依赖定价表 ⇒ 省略，见关卡 4.21 的 `_priced` 变体）。
//! - [`build_usage_recent`]：分页的最近行（Python `recent_usage`）。
//! - [`build_usage_timeseries`]：分桶序列（Python `usage_timeseries`）。
//! - [`build_usage_perf`]：延迟分位（Python `perf_stats`；分位从原始样本算）。
//! - [`resolve_range_window`] / [`pick_bucket_seconds`]：区间与桶粒度
//!   （Python `range_window` / `_usage_timeseries_scope`）。
//!
//! ## 关卡 4.21（线 A）新增：成本估算
//!
//! 定价表接进来之后，聚合里多出一组成本键。**既有公开函数的签名一律没动**
//! （`crates/api/src/routes.rs` 在调它们）⇒ 成本能力挂在**同名的 `_priced`
//! 变体上**，传 `None` 时与旧函数**逐字节一致**：
//!
//! - [`build_usage_snapshot_priced`]：补 `cost_cny`（合计）/ `cost_missing`
//!   （模型 → 次数），每个模型桶补 `cost_cny`。
//! - [`build_usage_snapshot_with_priced`]：同上，装饰组补 `usd_cny`。
//! - [`build_usage_recent_priced`]：每行补 `cost_*` 明细键组。
//! - [`build_usage_analytics_priced`]：`summary` 与 `models` 桶补 `cost_cny`。
//!
//! ⚠️ **没有定价表 ⇒ 一个成本键都不写**（不是 0、不是空表、不是 null）：
//! 「没有价表」与「价表说这条不要钱」是两回事，面板必须能分开看。
//! 未定价的**行**同理：整组 `cost_*` 键省略。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::activity;
use crate::price::{self, Pricing};

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
    /// 该桶内**已定价**行的金额合计（人民币）。
    ///
    /// 只有 `by_model` 桶用（Python 的桶字面量里有 `cost_cny`）；总桶不读它。
    cny: f64,
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
///   `cost_cny` / `cost_missing`（**定价表未接入**：本函数不接定价表，
///   要成本请用 [`build_usage_snapshot_priced`]）。
pub fn build_usage_snapshot(rows: &[Value]) -> Value {
    build_usage_snapshot_priced(rows, None)
}

/// 用量快照（**带成本估算**）。
///
/// `pricing` 为 `None` ⇒ 与 [`build_usage_snapshot`] **逐字节一致**（不写任何
/// 成本键）；有定价表 ⇒ 补上 Python 的 `cost_cny` / `cost_missing`（总量）
/// 与每个模型桶的 `cost_cny`。
///
/// ⚠️ 口径（与 Python `_fold_usage_snapshot` / `_fold_cost` 对齐）：
/// - **只折成功行与失败行，不折中止行**（`outcome == "client_aborted"` 的
///   usage 块不完整）。
/// - 折不进价的行记进 `cost_missing`（模型 id → 次数），**不静默少报**。
/// - **定价表缺失 ⇒ `cost_cny` / `cost_missing` 整组省略**（不是 0、不是
///   空表）——「没有价表」与「价表说这条不要钱」是两回事。
pub fn build_usage_snapshot_priced(rows: &[Value], pricing: Option<&Pricing>) -> Value {
    let mut totals = Bucket::default();
    let mut by_model = Buckets::new();
    let mut cost = CostFold::default();

    for row in rows {
        let outcome = row_outcome(row);
        // 中止行的 usage 块不完整 ⇒ 不计价（Python 同款）。
        let row_cost = match pricing {
            Some(pricing) if outcome != "client_aborted" => Some(price::compute_row(row, pricing)),
            _ => None,
        };
        if outcome != "completed" {
            totals.fold_failed();
        } else {
            totals.fold_completed(row);
            let model = model_of(row);
            let bucket = by_model.entry(model);
            bucket.fold_completed(row);
            if let Some(row_cost) = &row_cost {
                if row_cost.known {
                    bucket.cny += row_cost.cny;
                }
            }
        }
        if let Some(row_cost) = &row_cost {
            if row_cost.known {
                cost.cny += row_cost.cny;
            } else {
                cost.bump_missing(model_of(row));
            }
        }
    }

    let mut out = Map::new();
    out.insert("requests".into(), Value::from(totals.requests));
    out.insert("total_requests".into(), Value::from(totals.total_rows));
    out.insert("errors".into(), Value::from(totals.errors));
    for (index, name) in TOKEN_FIELDS.iter().enumerate() {
        out.insert((*name).into(), Value::from(totals.tokens[index]));
    }
    // 成本键（Python `_empty_stats` 的 `cost_cny` / `cost_missing`，位置也在
    // token 字段之后、`by_model` 之前）。**没有定价表 ⇒ 整组省略**。
    if pricing.is_some() {
        out.insert("cost_cny".into(), Value::from(cost.cny));
        out.insert("cost_missing".into(), Value::Object(cost.missing.clone()));
    }
    let mut models = Map::new();
    for (name, bucket) in &by_model.items {
        // 键序对齐 Python 的桶字面量（`_fold_usage_snapshot`）：
        // `{"requests", "accounts", "cost_cny", ...USAGE_FIELDS}` —— `accounts`
        // 在 token 字段**之前**，`cost_cny` 夹在中间。键序是响应字节的一部分，
        // 不能随手排。
        let mut value = Map::new();
        value.insert("requests".into(), Value::from(bucket.requests));
        value.insert("accounts".into(), Value::Object(bucket.accounts.clone()));
        if pricing.is_some() {
            value.insert("cost_cny".into(), Value::from(bucket.cny));
        }
        for (index, field) in TOKEN_FIELDS.iter().enumerate() {
            value.insert((*field).into(), Value::from(bucket.tokens[index]));
        }
        models.insert(name.clone(), Value::Object(value));
    }
    out.insert("by_model".into(), Value::Object(models));
    out.insert("realm".into(), Value::from("all"));
    Value::Object(out)
}

/// 一次扫描的成本累加器（Python 的 `bucket["cost_cny"]` + `bucket["cost_missing"]`）。
#[derive(Default)]
struct CostFold {
    /// 已定价行的金额合计（人民币）。
    cny: f64,
    /// 未定价的模型 → 出现次数（Python `cost_missing`）。
    missing: Map<String, Value>,
}

impl CostFold {
    /// 记一次「这个模型没有价」（Python `_fold_cost` 的 else 分支）。
    fn bump_missing(&mut self, model: String) {
        let count = self
            .missing
            .get(&model)
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.missing.insert(model, Value::from(count + 1));
    }
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
    build_usage_analytics_priced(rows, None)
}

/// 用量分析（**带成本估算**）。
///
/// `pricing` 为 `None` ⇒ 与 [`build_usage_analytics`] 完全一致。
/// 有定价表 ⇒ `summary` 与 `models` 各桶补上 `cost_cny`（Python 的
/// `_new_analytics_stat` / `_bump_analytics_models` 同款）。
pub fn build_usage_analytics_priced(rows: &[Value], pricing: Option<&Pricing>) -> Value {
    let mut totals = Bucket::default();
    let mut dates = Buckets::new();
    let mut cost = CostFold::default();

    for row in rows {
        let date = row_date(row);
        let outcome = row_outcome(row);
        // 中止行不计价（与快照同口径）。
        let row_cost = match pricing {
            Some(pricing) if outcome != "client_aborted" => Some(price::compute_row(row, pricing)),
            _ => None,
        };
        if outcome == "completed" {
            totals.fold_completed(row);
            dates.entry(date).fold_completed(row);
        } else {
            totals.fold_failed();
            dates.entry(date).fold_failed();
        }
        if let Some(row_cost) = &row_cost {
            if row_cost.known {
                cost.cny += row_cost.cny;
            } else {
                cost.bump_missing(model_of(row));
            }
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
    if pricing.is_some() {
        summary.insert("cost_cny".into(), Value::from(cost.cny));
    }

    let by_account = build_usage_by_account(rows);
    let snapshot = build_usage_snapshot_priced(rows, pricing);

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

// ---------------------------------------------------------------------------
// 关卡 4.20（线 B）：聚合三件套 + 快照装饰键
// ---------------------------------------------------------------------------

/// 进程启动时刻（epoch 秒）。
///
/// Python 的 `_usage = {"started": time.time()}` 是模块级的：进程启动时打点，
/// 之后每次快照都回同一个值。Rust 侧用「首次使用时打点」等价。
static PROCESS_STARTED: LazyLock<f64> = LazyLock::new(activity::now_epoch);

/// 进程启动时刻（epoch 秒）。
pub fn process_started() -> f64 {
    *PROCESS_STARTED
}

/// `usage_snapshot` 的装饰键（Python `_usage_snapshot_uncached` 追加的那一组）。
///
/// 拿不到的键**整键省略**（不写假的 0 值）：
/// - `usd_cny`：定价表未接入（Rust 侧不做定价）⇒ 省略。
/// - `accounts_map` / `account`：由调用方按池里的真实账号填；池为空时省略。
#[derive(Debug, Clone, Default)]
pub struct SnapshotDecor {
    /// 生效的 realm（`"all"` = 不过滤）。
    pub realm: String,
    /// 用量日志路径（Python `snap["log_file"]`）。
    pub log_file: String,
    /// 账号 → `{nickname, realm}`（Python `snap["accounts_map"]`）。
    pub accounts_map: Option<Value>,
    /// 代表账号（Python `snap["account"]`）。
    pub account: Option<Value>,
}

/// 用量快照 + 装饰键（Python `usage_snapshot` 的完整返回体）。
///
/// 与 [`build_usage_snapshot`] 的差别只在装饰键：基底的键与键序不变。
///
/// ⚠️ 与 Python 的差异：
/// - 没有 `usd_cny`（定价表未接入：要用它请调
///   [`build_usage_snapshot_with_priced`]）。
/// - `accounts_map` 的 `nickname` 恒为 `""`：Rust 的账号池结构体
///   （`AccountState`）不携带昵称，而 Python 的 `Account.nickname` 缺省也是
///   空串（`wb_accounts.py:495`）。
/// - `account` 里没有 `domain` / `issuer`：Rust 侧拿不到这两个值 ⇒ 省略
///   这两个子键，而不是写假的空串。
pub fn build_usage_snapshot_with(rows: &[Value], decor: &SnapshotDecor) -> Value {
    build_usage_snapshot_with_priced(rows, decor, None)
}

/// 用量快照 + 装饰键（**带成本估算**）。
///
/// `pricing` 为 `None` ⇒ 与 [`build_usage_snapshot_with`] 完全一致。
/// 有定价表 ⇒ 基底补 `cost_cny` / `cost_missing`，装饰组按 Python 的顺序
/// 在 `realm` 之后补 `usd_cny`（Python `_usage_snapshot_uncached` 的
/// `snap["usd_cny"] = wb_pricing.usd_cny()`）。
pub fn build_usage_snapshot_with_priced(
    rows: &[Value],
    decor: &SnapshotDecor,
    pricing: Option<&Pricing>,
) -> Value {
    let mut out = match build_usage_snapshot_priced(rows, pricing) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // Python 的装饰组：`started` / `since` / `log_file` / `realm` / `usd_cny`
    // / `accounts_map` / `account`。`realm` 在基底里已是最后一个键，这里先摘掉
    // 再按 Python 的顺序重新追加，保证响应字节的键序一致。
    out.remove("realm");
    let started = process_started();
    out.insert("started".into(), Value::from(started));
    out.insert("since".into(), Value::from(local_iso(started)));
    out.insert("log_file".into(), Value::from(decor.log_file.clone()));
    out.insert(
        "realm".into(),
        Value::from(if decor.realm.is_empty() {
            "all".to_string()
        } else {
            decor.realm.clone()
        }),
    );
    if let Some(pricing) = pricing {
        out.insert("usd_cny".into(), Value::from(pricing.usd_cny()));
    }
    if let Some(accounts_map) = &decor.accounts_map {
        out.insert("accounts_map".into(), accounts_map.clone());
    }
    if let Some(account) = &decor.account {
        out.insert("account".into(), account.clone());
    }
    Value::Object(out)
}

/// 把面板的区间选择解析成 `(since, until)`（epoch 秒）。
///
/// 对齐 Python `wb_proxy.range_window`（:576-610）：
/// - `today` / `day` / `1d` → 本地零点起
/// - `week` / `w` → 本地**周一**零点起
/// - `month` / `m` → 本地月初起
/// - `custom` → 取 `since` / `until` 两个 epoch（可以只有一边；反了会自动换）
/// - 其它 / 缺省 → `(None, None)` = **不过滤**（旧面板的老行为）
pub fn resolve_range_window(
    range_key: &str,
    since: Option<&str>,
    until: Option<&str>,
    now: f64,
) -> (Option<f64>, Option<f64>) {
    let key = range_key.trim().to_lowercase();
    use chrono::Datelike;
    match key.as_str() {
        "today" | "day" | "1d" => (Some(activity::local_midnight(now)), None),
        "week" | "w" => {
            let weekday = chrono::DateTime::from_timestamp(now as i64, 0)
                .map(|dt| dt.with_timezone(&chrono::Local))
                .map(|local| local.weekday().num_days_from_monday())
                .unwrap_or(0);
            (
                Some(activity::local_midnight(now - weekday as f64 * 86400.0)),
                None,
            )
        }
        "month" | "m" => {
            let local = chrono::DateTime::from_timestamp(now as i64, 0)
                .map(|dt| dt.with_timezone(&chrono::Local))
                .unwrap_or_else(chrono::Local::now);
            let naive = local
                .date_naive()
                .with_day(1)
                .and_then(|day| day.and_hms_opt(0, 0, 0))
                .expect("月初是合法时间");
            let midnight = match naive.and_local_timezone(chrono::Local) {
                chrono::LocalResult::Single(dt) => dt.timestamp() as f64,
                chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp() as f64,
                chrono::LocalResult::None => local.timestamp() as f64,
            };
            (Some(midnight), None)
        }
        "custom" => {
            let mut lo = epoch_or_none(since);
            let mut hi = epoch_or_none(until);
            if let (Some(low), Some(high)) = (lo, hi) {
                if high < low {
                    lo = Some(high);
                    hi = Some(low);
                }
            }
            (lo, hi)
        }
        _ => (None, None),
    }
}

/// 面板给的 epoch 秒；负数 / 解析不了 ⇒ `None`（该侧不设限）。
///
/// 对齐 Python `_epoch_or_none`（:559-573）。
fn epoch_or_none(value: Option<&str>) -> Option<f64> {
    let text = value.map(str::trim).unwrap_or("");
    if text.is_empty() {
        return None;
    }
    match text.parse::<f64>() {
        Ok(parsed) if parsed >= 0.0 => Some(parsed),
        _ => None,
    }
}

/// 一行是否落在 `[since, until]` 窗口内（两端都含）。
///
/// ⚠️ 边界为 0 时**当作没有边界**（Python 的真值语义：折叠里写的是
/// `if since and at < since`，`since == 0` 是假值 ⇒ 那一侧不筛）。
pub fn row_in_window(row: &Value, since: Option<f64>, until: Option<f64>) -> bool {
    let at = row.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    if let Some(low) = since {
        if low > 0.0 && at < low {
            return false;
        }
    }
    if let Some(high) = until {
        if high > 0.0 && at > high {
            return false;
        }
    }
    true
}

/// 桶粒度自适应（Python `_usage_timeseries_scope`，:4445-4483）。
///
/// 窗口 ≤6h ⇒ 60s；≤14d ⇒ 3600s；更长 ⇒ 86400s。显式 `bucket` 覆盖，
/// 但**下限 60 秒**（Python `max(60, int(bucket_seconds))`）。
pub fn pick_bucket_seconds(span_secs: f64, requested: Option<i64>) -> i64 {
    if let Some(value) = requested {
        if value != 0 {
            return value.max(60);
        }
    }
    if span_secs <= 6.0 * 3600.0 {
        60
    } else if span_secs <= 14.0 * 86400.0 {
        3600
    } else {
        86400
    }
}

/// 分页的最近用量行（Python `recent_usage`）。
///
/// **行序：最新在前**（Python 从日志尾部往回读）；页 1 是最新的一页。
///
/// ⚠️ 与 Python 的差异：定价字段（`cost_cny` / `cost_band` / `cost_source` /
/// `cost_rates` / `cost_unit` / `cost_currency` / `cost_usd_cny` / `cost_or_id`
/// / `cost_via` / `cost_inherited_from` / `cost_override_from` /
/// `cost_band_note` / `cost_via_derived`）依赖定价表 ⇒ 要用它们请调
/// [`build_usage_recent_priced`]；`usd_cny` 同理。
pub fn build_usage_recent(rows: &[Value], realm: &str, page: i64, limit: i64) -> Value {
    build_usage_recent_priced(rows, realm, page, limit, None)
}

/// 分页的最近用量行（**带成本估算**）。
///
/// `pricing` 为 `None` ⇒ 与 [`build_usage_recent`] 完全一致（行里没有任何
/// `cost_*` 键）。
///
/// 有定价表时，每一行按 Python `recent_usage`（`wb_proxy.py:5607-5637`）补上
/// 这一组键（键序与 Python 一致）：
///
/// | 键 | 来源 | Rust 侧能否算出 |
/// |---|---|---|
/// | `cost_cny` | `compute_row` | ✅ |
/// | `cost_band` | 命中的条件档序号 | ✅（无档位 ⇒ 省略） |
/// | `cost_source` | 条目自身的 `source` | ⚠️ Python 是策略 id / "builtin"，Rust 无策略层 |
/// | `cost_source_at` | 策略首次取到时刻 | ❌ 省略（策略层） |
/// | `cost_backfilled` | 补算标记 | ✅ 恒 `false`（Rust 无补算） |
/// | `cost_rates` | 命中的那档单价表 | ✅ |
/// | `cost_unit` | 条目的计价单位 | ✅ |
/// | `cost_currency` | 条目的货币 | ✅ |
/// | `cost_usd_cny` | 快照里的汇率 | ✅ |
/// | `cost_or_id` | OpenRouter 模型 id | ✅ |
/// | `cost_via` | 匹配方式（direct/variant/override） | ✅ |
/// | `cost_inherited_from` | 变体继承的基准名 | ✅ |
/// | `cost_override_from` | 人工映射的来源名 | ✅ |
/// | `cost_band_note` | 条件档的中文说明 | ✅ |
/// | `cost_via_derived` | `via` 是否推断得来 | ✅ 恒 `false` |
///
/// **未定价的行整组省略**（Python 写 `None`）：本仓库的口径是「省略胜过
/// 编 0」，面板按 `cost_cny == null` 显示「该模型暂无定价数据」。
pub fn build_usage_recent_priced(
    rows: &[Value],
    realm: &str,
    page: i64,
    limit: i64,
    pricing: Option<&Pricing>,
) -> Value {
    let limit = limit.max(1) as usize;
    let matched: Vec<&Value> = rows
        .iter()
        .filter(|row| realm_matches(row, realm))
        .collect();
    let total = matched.len();
    let total_pages = if total > 0 { total.div_ceil(limit) } else { 1 };
    let page = page.max(1).min(total_pages as i64) as usize;
    let start = (page - 1) * limit;
    // `matched` 是文件序（最旧 → 最新）⇒ 倒着取才是「最新在前」。
    let page_rows: Vec<Value> = matched
        .iter()
        .rev()
        .skip(start)
        .take(limit)
        .map(|row| match pricing {
            Some(pricing) => decorate_cost_row(row, pricing),
            None => (*row).clone(),
        })
        .collect();

    let mut out = Map::new();
    out.insert("total".into(), Value::from(total as i64));
    out.insert("page".into(), Value::from(page as i64));
    out.insert("limit".into(), Value::from(limit as i64));
    out.insert("total_pages".into(), Value::from(total_pages as i64));
    if let Some(pricing) = pricing {
        out.insert("usd_cny".into(), Value::from(pricing.usd_cny()));
    }
    out.insert("rows".into(), Value::Array(page_rows));
    Value::Object(out)
}

/// 给一行补上成本明细键（Python `recent_usage` 里的那一组）。
///
/// **未定价 ⇒ 原样返回**（不写半截明细、不写假的 0）。
fn decorate_cost_row(row: &Value, pricing: &Pricing) -> Value {
    let Some(detail) = price::compute_row_detail(row, pricing) else {
        return row.clone();
    };
    let mut out = match row {
        Value::Object(map) => map.clone(),
        other => return other.clone(),
    };
    // 键序与 Python `recent_usage`（`wb_proxy.py:5618-5637`）一致。
    out.insert("cost_cny".into(), Value::from(price::round6(detail.cny)));
    if let Some(band) = detail.band {
        out.insert("cost_band".into(), Value::from(band));
    }
    out.insert("cost_source".into(), Value::from(detail.source.clone()));
    // Rust 侧没有「补算」这回事（没有策略表层）⇒ 恒 false。
    out.insert("cost_backfilled".into(), Value::Bool(false));
    if let Some(rates) = detail.rates {
        out.insert("cost_rates".into(), rates.to_json());
    }
    out.insert("cost_unit".into(), Value::from(detail.unit));
    out.insert("cost_currency".into(), Value::from(detail.currency.clone()));
    out.insert("cost_usd_cny".into(), Value::from(detail.usd_cny));
    out.insert("cost_or_id".into(), Value::from(detail.or_id.clone()));
    out.insert(
        "cost_via".into(),
        Value::from(detail.via.clone().unwrap_or_else(|| "direct".to_string())),
    );
    if let Some(base) = &detail.inherited_from {
        out.insert("cost_inherited_from".into(), Value::from(base.clone()));
    }
    if let Some(from) = &detail.override_from {
        out.insert("cost_override_from".into(), Value::from(from.clone()));
    }
    if let Some(note) = &detail.band_note {
        out.insert("cost_band_note".into(), Value::from(note.clone()));
    }
    out.insert("cost_via_derived".into(), Value::Bool(detail.via_derived));
    Value::Object(out)
}

/// 一个时间序列桶（Python `_usage_timeseries_scan` 的桶字面量）。
#[derive(Default)]
struct SeriesBucket {
    requests: i64,
    errors: i64,
    tokens: [i64; 5],
    credit: f64,
}

/// 一条计费行的载荷（Python `_series_credit_entry`，:7679-7689）。
fn series_credit_entry(at: f64, row: &Value) -> Value {
    json!({
        "at": at,
        "iso": row.get("iso").and_then(Value::as_str).unwrap_or(""),
        "model": row.get("model").and_then(Value::as_str).unwrap_or(""),
        "account": row.get("account").and_then(Value::as_str).unwrap_or(""),
        "credit": row.get("credit").and_then(Value::as_f64).unwrap_or(0.0),
        "total_tokens": row.get("total_tokens").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// 时间序列（Python `usage_timeseries` / `_usage_timeseries_scan`）。
///
/// 返回 `{ok, realm, bucket_seconds, since, until, series[], credits[]}`。
/// 桶键序与 Python 的桶字面量一致；`credits` 是窗口内**按 `at` 倒序的前 50 条**
/// 计费行（`at` 相同时保持日志顺序）。
///
/// ⚠️ 与 Python 的差异：没有增量折叠与 TTL 缓存（Python 侧那两样是纯加速）。
pub fn build_usage_timeseries(
    rows: &[Value],
    realm: &str,
    since: f64,
    until: f64,
    bucket_seconds: i64,
) -> Value {
    let step = bucket_seconds.max(1) as f64;
    let mut buckets: Vec<(i64, SeriesBucket)> = Vec::new();
    let mut credits: Vec<(f64, usize, Value)> = Vec::new();

    for (index, row) in rows.iter().enumerate() {
        if !realm_matches(row, realm) {
            continue;
        }
        let at = row.get("at").and_then(Value::as_f64).unwrap_or(0.0);
        if at < since || at > until {
            continue;
        }
        let key = ((at - since) / step).floor() as i64;
        let position = match buckets.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(found) => found,
            Err(found) => {
                buckets.insert(found, (key, SeriesBucket::default()));
                found
            }
        };
        let bucket = &mut buckets[position].1;
        let outcome = row_outcome(row);
        if outcome == "completed" {
            bucket.requests += 1;
            for (slot, field) in TOKEN_FIELDS.iter().enumerate() {
                bucket.tokens[slot] += row
                    .get(*field)
                    .and_then(Value::as_f64)
                    .map(|value| value as i64)
                    .unwrap_or(0);
            }
        } else {
            bucket.errors += 1;
        }
        let credit = row.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        // 客户端自己走掉的请求没有消耗计费额度（Python 同款）。
        if outcome != "client_aborted" {
            bucket.credit += credit;
        }
        if credit > 0.0 {
            credits.push((at, index, series_credit_entry(at, row)));
        }
    }

    // Python：按 `(at, 序号)` 倒序（`at` 相同时序号小的在前 = 日志顺序在前），
    // 取前 50 条。
    credits.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.1.cmp(&right.1))
    });
    credits.truncate(SERIES_CREDITS_KEEP);

    let mut series = Vec::with_capacity(buckets.len());
    for (key, bucket) in &buckets {
        let mut value = Map::new();
        value.insert("at".into(), Value::from(since + *key as f64 * step));
        value.insert("requests".into(), Value::from(bucket.requests));
        value.insert("errors".into(), Value::from(bucket.errors));
        for (slot, field) in TOKEN_FIELDS.iter().enumerate() {
            value.insert((*field).into(), Value::from(bucket.tokens[slot]));
        }
        value.insert("credit".into(), Value::from(bucket.credit));
        series.push(Value::Object(value));
    }

    let mut out = Map::new();
    out.insert("ok".into(), Value::Bool(true));
    out.insert(
        "realm".into(),
        Value::from(if realm.is_empty() {
            "all".to_string()
        } else {
            realm.to_string()
        }),
    );
    out.insert("bucket_seconds".into(), Value::from(bucket_seconds));
    out.insert("since".into(), Value::from(since));
    out.insert("until".into(), Value::from(until));
    out.insert("series".into(), Value::Array(series));
    out.insert(
        "credits".into(),
        Value::Array(credits.into_iter().map(|(_, _, entry)| entry).collect()),
    );
    Value::Object(out)
}

/// 窗口内保留的计费行条数（Python `_SERIES_CREDITS_KEEP`）。
const SERIES_CREDITS_KEEP: usize = 50;

/// 一个性能指标的原始样本集合。
#[derive(Default)]
struct PerfSamples {
    ttfts: Vec<f64>,
    gens: Vec<f64>,
    walls: Vec<f64>,
    tok_rates: Vec<f64>,
    hits: Vec<f64>,
}

/// 单个模型的性能桶（Python `m_buckets` 的一项）。
#[derive(Default)]
struct PerfModelBucket {
    total: i64,
    ok: i64,
    err: i64,
    aborted: i64,
    samples: PerfSamples,
}

/// Python 的 `round()`（平局取偶）——分位下标用它，不能用 `.round()`
/// （Rust 是「平局远离零」，`round(0.5)` 会差一位）。
fn py_round(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    let diff = value - floor;
    if diff > 0.5 {
        floor + 1.0
    } else if diff < 0.5 || (floor as i64) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// Python 的 `round(x, 1)`：按十进制精确舍入（平局取偶）。
///
/// Rust 的 `format!("{:.1}")` 走的是同一套「精确二进制值 → 就近舍入、平局
/// 取偶」，所以两者对同一个 `f64` 给出同一个结果。
fn py_round1(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.1}").parse::<f64>().unwrap_or(value)
}

/// 一个指标的汇总块（Python `_perf_block`，:819-843）。
///
/// **没有样本 ⇒ 整块为 `null`**（不是 0 值块）。分位从原始样本算（不许用
/// 聚合近似），排序一次喂三个分位。
fn perf_block(samples: &[f64]) -> Value {
    if samples.is_empty() {
        return Value::Null;
    }
    let mut ordered = samples.to_vec();
    ordered.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let count = ordered.len();
    let rank = |quantile: f64| -> f64 {
        let index = py_round((quantile / 100.0) * (count as f64 - 1.0)) as i64;
        let index = index.clamp(0, count as i64 - 1) as usize;
        ordered[index]
    };
    json!({
        "avg": py_round1(samples.iter().sum::<f64>() / count as f64),
        "p50": rank(50.0),
        "p90": rank(90.0),
        "p99": rank(99.0),
        "max": ordered[count - 1],
        "samples": count,
    })
}

/// 一个模型桶的返回体（Python `by_model` 的一项）。
fn perf_model_json(bucket: &PerfModelBucket) -> Value {
    let mut out = Map::new();
    out.insert("requests".into(), Value::from(bucket.total));
    out.insert("errors".into(), Value::from(bucket.err));
    out.insert("client_aborted".into(), Value::from(bucket.aborted));
    out.insert(
        "success_rate_pct".into(),
        success_rate(bucket.ok, bucket.err),
    );
    out.insert("ttft_ms".into(), perf_block(&bucket.samples.ttfts));
    out.insert("generation_ms".into(), perf_block(&bucket.samples.gens));
    out.insert("wall_ms".into(), perf_block(&bucket.samples.walls));
    out.insert(
        "tokens_per_sec".into(),
        perf_block(&bucket.samples.tok_rates),
    );
    out.insert("cache_hit_pct".into(), perf_block(&bucket.samples.hits));
    Value::Object(out)
}

/// 成功率（%）：分母为 0 ⇒ `null`（Python 同款）。
fn success_rate(ok: i64, err: i64) -> Value {
    if ok + err == 0 {
        return Value::Null;
    }
    Value::from(py_round1(ok as f64 * 100.0 / (ok + err) as f64))
}

/// 性能统计（Python `perf_stats` / `_perf_stats_uncached`）。
///
/// `sample` 是「日志**末尾**多少行」参与统计（Python 读文件尾部），不是时间
/// 窗；窗口过滤在这之后做。分位一律从原始样本算。
///
/// ⚠️ 与 Python 的差异：没有 `by_model_realm` / `by_model_acct` 两层三维分组
/// （Rust 的行不带账号 → realm 的推断链，跑不了）。
pub fn build_usage_perf(
    rows: &[Value],
    realm: &str,
    sample: i64,
    since: Option<f64>,
    until: Option<f64>,
) -> Value {
    let sample = sample.max(1) as usize;
    let sample_capped = rows.len() >= sample;
    let tail: &[Value] = if rows.len() > sample {
        &rows[rows.len() - sample..]
    } else {
        rows
    };
    // Python `sample_from` = 尾部第一行解析出来的 `at`（**在窗口过滤之前**）。
    let sample_from = tail
        .first()
        .and_then(|row| row.get("at"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut total = 0i64;
    let mut ok = 0i64;
    let mut err = 0i64;
    let mut aborted = 0i64;
    let mut samples = PerfSamples::default();
    let mut models: Vec<(String, PerfModelBucket)> = Vec::new();

    for row in tail {
        if !realm_matches(row, realm) {
            continue;
        }
        let at = row.get("at").and_then(Value::as_f64).unwrap_or(0.0);
        if let Some(low) = since {
            if low > 0.0 && at < low {
                continue;
            }
        }
        if let Some(high) = until {
            if high > 0.0 && at > high {
                continue;
            }
        }
        total += 1;
        let outcome = row_outcome(row);
        let model = model_of(row);
        let model_index = match models.iter().position(|(name, _)| *name == model) {
            Some(found) => found,
            None => {
                models.push((model.clone(), PerfModelBucket::default()));
                models.len() - 1
            }
        };
        let bucket = &mut models[model_index].1;
        bucket.total += 1;

        // 客户端自己走掉的请求既不是成功也不是失败，单独一档。
        if outcome == "client_aborted" {
            aborted += 1;
            bucket.aborted += 1;
            if let Some(value) = row.get("elapsed_ms").and_then(Value::as_f64) {
                samples.walls.push(value);
                bucket.samples.walls.push(value);
            }
            continue;
        }
        if outcome != "completed" {
            err += 1;
            bucket.err += 1;
            if let Some(value) = row.get("elapsed_ms").and_then(Value::as_f64) {
                samples.walls.push(value);
                bucket.samples.walls.push(value);
            }
            continue;
        }
        ok += 1;
        bucket.ok += 1;
        if let Some(value) = row.get("ttft_ms").and_then(Value::as_f64) {
            samples.ttfts.push(value);
            bucket.samples.ttfts.push(value);
        }
        if let Some(value) = row.get("gen_ms").and_then(Value::as_f64) {
            samples.gens.push(value);
            bucket.samples.gens.push(value);
        }
        if let Some(value) = row.get("elapsed_ms").and_then(Value::as_f64) {
            samples.walls.push(value);
            bucket.samples.walls.push(value);
        }
        if let Some(value) = row.get("tokens_per_sec").and_then(Value::as_f64) {
            samples.tok_rates.push(value);
            bucket.samples.tok_rates.push(value);
        }
        if let Some(value) = row.get("cache_hit_pct").and_then(Value::as_f64) {
            samples.hits.push(value);
            bucket.samples.hits.push(value);
        }
    }

    let mut by_model = Map::new();
    for (name, bucket) in &models {
        by_model.insert(name.clone(), perf_model_json(bucket));
    }

    let mut out = Map::new();
    out.insert("sampled".into(), Value::from(total));
    out.insert("sample_from".into(), sample_from);
    out.insert("sample_capped".into(), Value::Bool(sample_capped));
    out.insert("success".into(), Value::from(ok));
    out.insert("errors".into(), Value::from(err));
    out.insert("client_aborted".into(), Value::from(aborted));
    out.insert("success_rate_pct".into(), success_rate(ok, err));
    out.insert("ttft_ms".into(), perf_block(&samples.ttfts));
    out.insert("generation_ms".into(), perf_block(&samples.gens));
    out.insert("wall_ms".into(), perf_block(&samples.walls));
    out.insert("tokens_per_sec".into(), perf_block(&samples.tok_rates));
    out.insert("cache_hit_pct".into(), perf_block(&samples.hits));
    out.insert("by_model".into(), Value::Object(by_model));
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

    // -----------------------------------------------------------------
    // 关卡 4.20（线 B）：聚合三件套
    // -----------------------------------------------------------------

    /// 一条带 `at` 的用量行。
    fn timed_row(at: f64, model: &str, total_tokens: i64) -> Value {
        json!({
            "at": at,
            "iso": "2026-10-21 10:00:00",
            "model": model,
            "outcome": "completed",
            "prompt_tokens": total_tokens / 2,
            "completion_tokens": total_tokens - total_tokens / 2,
            "total_tokens": total_tokens,
            "account": "a1",
            "realm": "cn",
            "credit": 0.25,
        })
    }

    #[test]
    fn recent_usage_paginates_newest_first() {
        let rows: Vec<Value> = (0..5)
            .map(|index| timed_row(1_700_000_000.0 + index as f64, "m", index + 1))
            .collect();
        let out = build_usage_recent(&rows, "cn", 1, 2);
        assert_eq!(out["total"], 5);
        assert_eq!(out["limit"], 2);
        assert_eq!(out["page"], 1);
        assert_eq!(out["total_pages"], 3);
        let page = out["rows"].as_array().unwrap();
        assert_eq!(page.len(), 2);
        // 最新在前。
        assert_eq!(page[0]["total_tokens"], 5);
        assert_eq!(page[1]["total_tokens"], 4);

        // 页码越界 ⇒ 夹回最后一页。
        let out = build_usage_recent(&rows, "cn", 99, 2);
        assert_eq!(out["page"], 3);
        assert_eq!(out["rows"].as_array().unwrap().len(), 1);

        // 空集 ⇒ total_pages 至少 1。
        let out = build_usage_recent(&[], "cn", 1, 10);
        assert_eq!(out["total"], 0);
        assert_eq!(out["total_pages"], 1);
        assert!(out["rows"].as_array().unwrap().is_empty());
    }

    #[test]
    fn timeseries_buckets_rows_by_window() {
        let rows = vec![
            timed_row(1_700_000_000.0, "m1", 10),
            timed_row(1_700_000_030.0, "m1", 20),
            timed_row(1_700_000_090.0, "m2", 30),
        ];
        // 60s 桶 ⇒ 前两行同桶，第三行另开一桶。
        let out = build_usage_timeseries(&rows, "cn", 1_700_000_000.0, 1_700_000_120.0, 60);
        assert_eq!(out["ok"], true);
        assert_eq!(out["realm"], "cn");
        assert_eq!(out["bucket_seconds"], 60);
        let series = out["series"].as_array().unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0]["at"], 1_700_000_000.0);
        assert_eq!(series[0]["requests"], 2);
        assert_eq!(series[0]["total_tokens"], 30);
        assert_eq!(series[1]["at"], 1_700_000_060.0);
        assert_eq!(series[1]["requests"], 1);
        // 有 credit 的行进 `credits`（按 at 倒序）。
        let credits = out["credits"].as_array().unwrap();
        assert_eq!(credits.len(), 3);
        assert_eq!(credits[0]["at"], 1_700_000_090.0);

        // 窗口外的行不进桶。
        let out = build_usage_timeseries(&rows, "cn", 1_700_000_000.0, 1_700_000_010.0, 60);
        let series = out["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["requests"], 1);
    }

    #[test]
    fn perf_blocks_come_from_raw_samples_and_are_null_when_empty() {
        let rows = vec![
            json!({"at": 1.0, "model": "m1", "outcome": "completed", "realm": "cn",
                   "ttft_ms": 100.0, "gen_ms": 10.0, "elapsed_ms": 50.0,
                   "tokens_per_sec": 12.5, "cache_hit_pct": 33.3}),
            json!({"at": 2.0, "model": "m1", "outcome": "completed", "realm": "cn",
                   "ttft_ms": 200.0, "gen_ms": 10.0, "elapsed_ms": 50.0,
                   "tokens_per_sec": 12.5, "cache_hit_pct": 33.3}),
            json!({"at": 3.0, "model": "m1", "outcome": "completed", "realm": "cn",
                   "ttft_ms": 300.0, "gen_ms": 10.0, "elapsed_ms": 50.0,
                   "tokens_per_sec": 12.5, "cache_hit_pct": 33.3}),
        ];
        let out = build_usage_perf(&rows, "cn", 5000, None, None);
        assert_eq!(out["sampled"], 3);
        assert_eq!(out["success"], 3);
        assert_eq!(out["success_rate_pct"], 100.0);
        assert_eq!(out["sample_from"], 1.0);
        let ttft = &out["ttft_ms"];
        assert_eq!(ttft["avg"], 200.0);
        assert_eq!(ttft["p50"], 200.0);
        assert_eq!(ttft["p90"], 300.0);
        assert_eq!(ttft["p99"], 300.0);
        assert_eq!(ttft["max"], 300.0);
        assert_eq!(ttft["samples"], 3);
        assert_eq!(out["by_model"]["m1"]["requests"], 3);

        // 没有样本 ⇒ 整块为 null。
        let out = build_usage_perf(&[], "cn", 5000, None, None);
        assert_eq!(out["sampled"], 0);
        assert!(out["ttft_ms"].is_null());
        assert!(out["generation_ms"].is_null());
        assert!(out["wall_ms"].is_null());
        assert!(out["tokens_per_sec"].is_null());
        assert!(out["cache_hit_pct"].is_null());
        assert!(out["success_rate_pct"].is_null());
    }

    #[test]
    fn perf_counts_client_aborted_separately() {
        let rows = vec![
            json!({"at": 1.0, "model": "m1", "outcome": "completed", "realm": "cn"}),
            json!({"at": 2.0, "model": "m1", "outcome": "failed", "realm": "cn"}),
            json!({"at": 3.0, "model": "m1", "outcome": "client_aborted", "realm": "cn"}),
        ];
        let out = build_usage_perf(&rows, "cn", 5000, None, None);
        assert_eq!(out["sampled"], 3);
        assert_eq!(out["success"], 1);
        assert_eq!(out["errors"], 1);
        assert_eq!(out["client_aborted"], 1);
        // 客户端走掉的请求不进成功率的分母。
        assert_eq!(out["success_rate_pct"], 50.0);
        assert_eq!(out["by_model"]["m1"]["client_aborted"], 1);
    }

    #[test]
    fn range_window_matches_python_ranges() {
        let now = 1_800_000_000.0;
        // 认不出来的 range ⇒ 不过滤。
        assert_eq!(resolve_range_window("nope", None, None, now), (None, None));
        assert_eq!(resolve_range_window("", None, None, now), (None, None));
        // custom ⇒ 取两个 epoch；反了自动换。
        let (lo, hi) = resolve_range_window("custom", Some("100"), Some("200"), now);
        assert_eq!(lo, Some(100.0));
        assert_eq!(hi, Some(200.0));
        let (lo, hi) = resolve_range_window("custom", Some("200"), Some("100"), now);
        assert_eq!(lo, Some(100.0));
        assert_eq!(hi, Some(200.0));
        // 负数 / 解析不了 ⇒ 该侧不设限。
        let (lo, hi) = resolve_range_window("custom", Some("-5"), Some("abc"), now);
        assert_eq!(lo, None);
        assert_eq!(hi, None);
        // today ⇒ 本地零点起。
        let (lo, hi) = resolve_range_window("today", None, None, now);
        assert!(lo.is_some());
        assert_eq!(hi, None);
    }

    #[test]
    fn bucket_seconds_scale_with_the_window_span() {
        assert_eq!(pick_bucket_seconds(3600.0, None), 60);
        assert_eq!(pick_bucket_seconds(6.0 * 3600.0, None), 60);
        assert_eq!(pick_bucket_seconds(6.0 * 3600.0 + 1.0, None), 3600);
        assert_eq!(pick_bucket_seconds(7.0 * 86400.0, None), 3600);
        assert_eq!(pick_bucket_seconds(14.0 * 86400.0, None), 3600);
        assert_eq!(pick_bucket_seconds(14.0 * 86400.0 + 1.0, None), 86400);
        // 显式 bucket 覆盖，但下限 60。
        assert_eq!(pick_bucket_seconds(3600.0, Some(300)), 300);
        assert_eq!(pick_bucket_seconds(3600.0, Some(5)), 60);
        assert_eq!(pick_bucket_seconds(3600.0, Some(0)), 60);
    }

    #[test]
    fn snapshot_decoration_appends_python_keys() {
        let rows = vec![timed_row(1_700_000_000.0, "m1", 10)];
        let decor = SnapshotDecor {
            realm: "cn".to_string(),
            log_file: "/tmp/usage/usage.jsonl".to_string(),
            accounts_map: Some(json!({"a1": {"nickname": "", "realm": "cn"}})),
            account: Some(json!({"uid": "a1"})),
        };
        let out = build_usage_snapshot_with(&rows, &decor);
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        // 基底键在前，装饰组在后（Python 的顺序）。
        assert_eq!(keys[0], "requests");
        assert_eq!(keys[keys.len() - 6], "started");
        assert_eq!(keys[keys.len() - 5], "since");
        assert_eq!(keys[keys.len() - 4], "log_file");
        assert_eq!(keys[keys.len() - 3], "realm");
        assert_eq!(keys[keys.len() - 2], "accounts_map");
        assert_eq!(keys[keys.len() - 1], "account");
        assert_eq!(out["realm"], "cn");
        assert_eq!(out["log_file"], "/tmp/usage/usage.jsonl");
        assert!(out["started"].as_f64().unwrap() > 0.0);
        // 定价未接入 ⇒ 不写假的 usd_cny。
        assert!(out.get("usd_cny").is_none());
    }

    #[test]
    fn python_rounding_matches_python_semantics() {
        // Python `round(x, 1)`：平局取偶 + 十进制精确舍入。
        assert_eq!(py_round1(2.25), 2.2);
        assert_eq!(py_round1(2.35), 2.4);
        assert_eq!(py_round1(200.0 / 3.0), 66.7);
        // Python `round(x)`：平局取偶（Rust 的 `.round()` 是平局远离零）。
        assert_eq!(py_round(0.5), 0.0);
        assert_eq!(py_round(1.5), 2.0);
        assert_eq!(py_round(2.5), 2.0);
    }
}
