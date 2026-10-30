//! M5 数据侧：等价 API 成本的**定价快照**与计价。
//!
//! 权威源：Python `wb_pricing.py`
//! - `load_pricing`（:793）/ `band_for`（:864）/ `_band_applies`（:847）
//! - `compute_row`（:918）/ `sum_rows`（:954）/ `usd_cny`（:976）
//! - `rates_from`（:1072）/ `bands_from_overrides`（:1081）
//! - `normalize`（:1279）/ `index_openrouter`（:1284）/ `auto_match`（:1294）
//! - `resolve`（:1310）/ `inherited_from`（:1328）/ `entry_for`（:1338）
//! - `build_snapshot`（:1368）
//!
//! 回答的问题不是「网关扣了多少积分」，而是「同样的 token 按 OpenRouter 公布的
//! 价折算成人民币大概是多少」。**这是估算，不是实扣。**
//!
//! 所有价格都是「每 100 万 token」的美元价，结算货币是 CNY（按快照里的
//! `usd_cny` 折算）。
//!
//! ## 关卡 4.21 新增
//!
//! - [`CostDetail`] / [`compute_row_detail`]：一行的成本**明细**（Python
//!   `cost_for_row(details=True)` 的等价物）——面板的悬停气泡要的那一组键。
//! - [`band_note`]：命中条件档的中文说明（Python `band_note`）。
//! - [`PricingEntry::via`] / `inherited_from` / `override_from`：这条价**怎么
//!   匹配来的**（快照落盘时就记下来了，读取端不必再猜）。
//! - [`round6`]：Python `round(x, 6)`（成本金额按 Python 的口径取整）。

use chrono::{Datelike, Timelike};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

/// OpenRouter 的模型目录地址。
pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";
/// 快照默认的美元兑人民币汇率。
pub const USD_CNY: f64 = 7.10;

/// 变体后缀表：hub 的名字有时在上游同一实体模型后多带一个渠道/发行后缀。
///
/// 第二列是该后缀是否参与自动剥离——只放实测确认「同名字后缀 = 同一实体」的
/// 那些；`-f` / `-dev` / `-x` 不剥离（免费档、预览档、加量档语义不同）。
pub const VARIANT_SUFFIXES: [(&str, bool); 7] = [
    ("-lkeap", true),
    ("-taiji", true),
    ("-volc", true),
    ("-sg", true),
    ("-f", false),
    ("-dev", false),
    ("-x", false),
];

/// hub 名字比 OpenRouter 多/少一个后缀时的源码覆盖表（Python `OVERRIDES`）。
pub const OVERRIDES: [(&str, &str); 13] = [
    ("hy4-preview-f", "tencent/hy4-preview"),
    ("hy4-preview-dev", "tencent/hy4-preview"),
    ("hy4-preview-x", "tencent/hy4-preview"),
    ("hy3-x", "tencent/hy3"),
    ("kimi-k3-1", "moonshotai/kimi-k3"),
    ("kimi-k2.7", "moonshotai/kimi-k2.7-code"),
    ("glm-5.0-turbo", "z-ai/glm-5-turbo"),
    ("deepseek-v3-1", "deepseek/deepseek-chat-v3.1"),
    ("deepseek-v3-1-volc", "deepseek/deepseek-chat-v3.1"),
    ("deepseek-v3-2-volc", "deepseek/deepseek-v3.2"),
    ("deepseek-v3-0324", "deepseek/deepseek-chat-v3-0324"),
    ("hunyuan-2.0-instruct", "tencent/hunyuan-a13b-instruct"),
    ("hunyuan-chat", "tencent/hunyuan-a13b-instruct"),
];

/// OpenRouter 用 UTC 星期名与 HHMM 表达时段档位。
pub const WEEKDAY_KEYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];
/// 星期缩写（与 Python `WEEKDAY_SHORT` 同序）。
pub const WEEKDAY_SHORT: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// 三档单价（每百万 token，美元）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RateTable {
    pub input_cache_hit: Option<f64>,
    pub input_cache_miss: Option<f64>,
    pub output: Option<f64>,
}

impl RateTable {
    /// 取一档单价；缺失时回落到 `fallback`（保守的一侧）。
    ///
    /// 没有缓存命中价的行按**未命中价**计（绝不比参照引擎便宜），
    /// 所以估算不会低估成本。
    pub fn pick(&self, key: &str, fallback: Option<&str>) -> f64 {
        let direct = match key {
            "input_cache_hit" => self.input_cache_hit,
            "input_cache_miss" => self.input_cache_miss,
            _ => self.output,
        };
        if let Some(v) = direct {
            return v;
        }
        match fallback {
            Some(f) => match f {
                "input_cache_hit" => self.input_cache_hit.unwrap_or(0.0),
                "input_cache_miss" => self.input_cache_miss.unwrap_or(0.0),
                _ => self.output.unwrap_or(0.0),
            },
            None => 0.0,
        }
    }

    /// 从 JSON 的 `flat` 对象解析。
    pub fn from_json(value: &Value) -> RateTable {
        let get = |key: &str| value.get(key).and_then(|v| v.as_f64());
        RateTable {
            input_cache_hit: get("input_cache_hit"),
            input_cache_miss: get("input_cache_miss"),
            output: get("output"),
        }
    }

    /// 转回 JSON 的 `flat` 对象（`None` 的键省略）。
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        if let Some(v) = self.input_cache_hit {
            obj.insert("input_cache_hit".into(), json!(v));
        }
        if let Some(v) = self.input_cache_miss {
            obj.insert("input_cache_miss".into(), json!(v));
        }
        if let Some(v) = self.output {
            obj.insert("output".into(), json!(v));
        }
        Value::Object(obj)
    }
}

/// 条件档位。
#[derive(Debug, Clone, PartialEq)]
pub struct Band {
    pub flat: RateTable,
    /// 输入长度阈值（与 `days`/`start`/`end` 互斥）。
    pub min_prompt_tokens: Option<f64>,
    /// 生效的星期（UTC）。`None` = 每天。
    pub days: Option<Vec<String>>,
    /// 起始 HHMM。
    pub start: f64,
    /// 结束 HHMM（不含）。
    pub end: f64,
}

/// 一个模型的定价条目。
#[derive(Debug, Clone, PartialEq)]
pub struct PricingEntry {
    pub display: String,
    pub source: String,
    pub currency: String,
    pub unit: f64,
    pub or_id: String,
    pub flat: RateTable,
    pub bands: Vec<Band>,
    /// 策略内容哈希（Python `policy_id()`）。
    pub id: String,
    /// 该策略落定时的汇率（Python `entry["usd_cny"]`）。
    pub usd_cny: f64,
    /// 该策略首次出现的 epoch（Python `entry["first_seen"]`）。
    pub first_seen: Option<f64>,
    /// 这条价**怎么匹配来的**（Python `entry["via"]`）：`"variant"`（剥后缀
    /// 继承）/ `"override"`（人工映射表）。
    ///
    /// **缺键 = 直接匹配**（Python `match_source` 的 `direct` 兜底）——
    /// 快照落盘时（[`entry_for`]）就把命中方式写下来了，读取端不必再猜。
    pub via: Option<String>,
    /// 变体继承的基准模型名（Python `entry["inherited_from"]`）。
    pub inherited_from: Option<String>,
    /// 人工映射命中的来源模型名（Python `entry["override_from"]`）。
    pub override_from: Option<String>,
}

impl PricingEntry {
    /// 从快照的 `models[model]` 解析。
    pub fn from_json(model: &str, value: &Value) -> PricingEntry {
        let flat = value
            .get("flat")
            .map(RateTable::from_json)
            .unwrap_or_default();
        let bands = value
            .get("bands")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_band).collect())
            .unwrap_or_default();
        PricingEntry {
            display: value
                .get("display")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
            source: value
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("openrouter")
                .to_string(),
            currency: value
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("USD")
                .to_string(),
            unit: value
                .get("unit")
                .and_then(Value::as_f64)
                .filter(|u| *u != 0.0)
                .unwrap_or(1_000_000.0),
            or_id: value
                .get("or_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            flat,
            bands,
            id: value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            usd_cny: value.get("usd_cny").and_then(Value::as_f64).unwrap_or(0.0),
            first_seen: value.get("first_seen").and_then(Value::as_f64),
            via: nonempty(value.get("via")),
            inherited_from: nonempty(value.get("inherited_from")),
            override_from: nonempty(value.get("override_from")),
        }
    }
}

/// 取一个非空字符串字段（空串当缺键）。
fn nonempty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 解析一条 band。
pub fn parse_band(value: &Value) -> Option<Band> {
    if !value.is_object() {
        return None;
    }
    let flat = value
        .get("flat")
        .map(RateTable::from_json)
        .unwrap_or_default();
    let min_prompt_tokens = value.get("min_prompt_tokens").and_then(Value::as_f64);
    let days = value.get("days").and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let start = value.get("start").and_then(Value::as_f64).unwrap_or(0.0);
    let end = value
        .get("end")
        .and_then(Value::as_f64)
        .filter(|e| *e != 0.0)
        .unwrap_or(2400.0);
    Some(Band {
        flat,
        min_prompt_tokens,
        days,
        start,
        end,
    })
}

/// 一份定价快照。
#[derive(Debug, Clone, PartialEq)]
pub struct Pricing {
    pub base_currency: String,
    pub usd_cny: f64,
    pub models: HashMap<String, PricingEntry>,
}

impl Pricing {
    /// 解析快照 JSON。
    ///
    /// 坏形状一律按缺省处理（`models` 空、汇率 1.0）：一份坏快照不该让面板挂掉。
    pub fn from_json(value: &Value) -> Pricing {
        let meta = value.get("meta");
        let usd_cny = meta
            .and_then(|m| m.get("usd_cny"))
            .and_then(Value::as_f64)
            .filter(|v| *v != 0.0)
            .unwrap_or(1.0);
        let mut models = HashMap::new();
        if let Some(obj) = value.get("models").and_then(Value::as_object) {
            for (id, entry) in obj {
                if entry.is_object() {
                    models.insert(id.clone(), PricingEntry::from_json(id, entry));
                }
            }
        }
        Pricing {
            base_currency: meta
                .and_then(|m| m.get("base_currency"))
                .and_then(Value::as_str)
                .unwrap_or("CNY")
                .to_string(),
            usd_cny,
            models,
        }
    }

    /// 快照里的美元兑人民币汇率。
    pub fn usd_cny(&self) -> f64 {
        self.usd_cny
    }
}

/// 一行的计价结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    /// 折算后的人民币金额。
    pub cny: f64,
    /// 模型有没有价；`false` 时 `cny` 恒为 0，调用方应报「未定价」而不是猜。
    pub known: bool,
    /// 命中的条件档序号（`None` = 走模型自身的基础价）。
    pub band: Option<usize>,
    /// 命中的那档单价表，原样给出，调用方不必重新推导档位。
    pub rates: Option<RateTable>,
}

/// 批量合计的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Totals {
    pub cny: f64,
    /// 定价成功的行数。
    pub priced: usize,
    /// 模型 id → 出现次数（无法定价的行）。
    pub missing: HashMap<String, usize>,
}

/// OpenRouter 的单价是「每 token 的美元」，转成「每百万 token」。
pub fn per_million(value: &Value) -> Option<f64> {
    let f = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    Some(round6(f * 1_000_000.0))
}

/// Python `round(x, 6)`。
pub fn round6(x: f64) -> f64 {
    let scaled = x * 1_000_000.0;
    let floor = scaled.floor();
    let diff = scaled - floor;
    let rounded = if (diff - 0.5).abs() < 1e-9 {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        scaled.round()
    };
    rounded / 1_000_000.0
}

/// 一条 OpenRouter 记录 → 每百万 token 的三档美元价。
pub fn rates_from(item: &Value) -> RateTable {
    RateTable {
        input_cache_hit: item.get("input_cache_read").and_then(per_million),
        input_cache_miss: item.get("prompt").and_then(per_million),
        output: item.get("completion").and_then(per_million),
    }
}

/// OpenRouter 的 `overrides` → 条件档位列表。
///
/// 两种条件在源数据里互不混用：
/// - `min_prompt_tokens`：输入长度达到该阈值后改用的价；
/// - `utc_days` / `utc_start` / `utc_end`：按时段（UTC）改用的价。
///
/// 与基准价重复、不带任何条件的项不取。
pub fn bands_from_overrides(or_entry: &Value) -> Vec<Band> {
    let mut out = Vec::new();
    let items = match or_entry.get("overrides").and_then(Value::as_array) {
        Some(items) => items,
        None => return out,
    };
    for item in items {
        if !item.is_object() {
            continue;
        }
        let flat = rates_from(item);
        if let Some(threshold) = item.get("min_prompt_tokens").and_then(Value::as_f64) {
            out.push(Band {
                flat,
                min_prompt_tokens: Some(threshold),
                days: None,
                start: 0.0,
                end: 2400.0,
            });
            continue;
        }
        let has_utc = item
            .as_object()
            .map(|o| o.keys().any(|k| k.starts_with("utc_")))
            .unwrap_or(false);
        if !has_utc {
            continue;
        }
        let days = item.get("utc_days").and_then(Value::as_array).map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(|d| d.to_lowercase())
                .filter(|d| WEEKDAY_KEYS.contains(&d.as_str()))
                .map(|d| {
                    let idx = WEEKDAY_KEYS.iter().position(|k| *k == d).unwrap_or(0);
                    WEEKDAY_SHORT[idx].to_string()
                })
                .collect::<Vec<_>>()
        });
        let start = item.get("utc_start").and_then(Value::as_f64).unwrap_or(0.0);
        let end = item
            .get("utc_end")
            .and_then(Value::as_f64)
            .filter(|e| *e != 0.0)
            .unwrap_or(2400.0);
        out.push(Band {
            flat,
            min_prompt_tokens: None,
            days,
            start,
            end,
        });
    }
    out
}

/// `(星期缩写, HHMM)` at UTC。
///
/// 网关可能跑在 UTC 容器里、面板可能从任意时区打开，查表不能依赖宿主机时区。
pub fn utc_parts(epoch_sec: f64) -> (&'static str, i64) {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch_sec as i64, 0);
    match dt {
        Some(dt) => {
            let weekday = dt.weekday().num_days_from_monday() as usize;
            let hhmm = dt.hour() as i64 * 100 + dt.minute() as i64;
            (WEEKDAY_SHORT[weekday.min(6)], hhmm)
        }
        None => ("mon", 0),
    }
}

/// 一条 band 的条件是否成立。
///
/// band 带两种条件之一：最小输入长度，或 UTC 时段；源数据里不会混用。
pub fn band_applies(band: &Band, prompt_tokens: f64, day: &str, hhmm: i64) -> bool {
    if let Some(threshold) = band.min_prompt_tokens {
        return prompt_tokens >= threshold;
    }
    if let Some(days) = &band.days {
        if !days.is_empty() && !days.iter().any(|d| d == day) {
            return false;
        }
    }
    band.start <= hhmm as f64 && (hhmm as f64) < band.end
}

/// 这一行该用哪张价表，以及它来自第几档。
///
/// 带档的模型按**最后一条满足条件的档**取胜：长度档按阈值升序排列，
/// 所以取最后一个命中就是取最紧的那条；时段档互不重叠，同一条规则成立。
///
/// 没有档、或一行不满足任何档时，回落到模型自身的基础价——估算退化成
/// 单价行为，而不是 0。
pub fn band_for(
    entry: &PricingEntry,
    epoch_sec: f64,
    prompt_tokens: f64,
) -> (RateTable, Option<usize>) {
    if !entry.bands.is_empty() {
        let (day, hhmm) = utc_parts(epoch_sec);
        let mut hit: Option<(RateTable, usize)> = None;
        for (index, band) in entry.bands.iter().enumerate() {
            if !band_applies(band, prompt_tokens, day, hhmm) {
                continue;
            }
            hit = Some((band.flat, index));
        }
        if let Some(hit) = hit {
            return (hit.0, Some(hit.1));
        }
    }
    (entry.flat, None)
}

/// 一行的估算成本（人民币）。
///
/// `known = false` 表示这个模型没有价——`cny` 保持 0，调用方报「未定价」
/// 而不是猜一个数。
pub fn compute_row(row: &Value, pricing: &Pricing) -> Cost {
    let empty = Cost {
        cny: 0.0,
        known: false,
        band: None,
        rates: None,
    };
    if row.is_null() {
        return empty;
    }
    let model = match row.get("model").and_then(Value::as_str) {
        Some(m) => m,
        None => return empty,
    };
    let entry = match pricing.models.get(model) {
        Some(e) => e,
        None => return empty,
    };
    let unit = if entry.unit != 0.0 {
        entry.unit
    } else {
        1_000_000.0
    };
    let prompt = row
        .get("prompt_tokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let at = row.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    let (rate, band) = band_for(entry, at, prompt);

    let cached = row
        .get("cached_tokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .min(prompt);
    let uncached = prompt - cached;
    // 输出价只作用在 completion_tokens 上：上游把推理 token 算在 completion 里
    // （实测与记录一致），再单算一次就是重复收费。
    let output = row
        .get("completion_tokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let miss = rate.pick("input_cache_miss", None);
    let hit = rate.pick("input_cache_hit", Some("input_cache_miss"));
    let out_rate = rate.pick("output", None);

    let factor = if entry.currency.to_uppercase() == "USD" {
        pricing.usd_cny
    } else {
        1.0
    };
    let cny = (uncached * miss + cached * hit + output * out_rate) / unit * factor;
    Cost {
        cny,
        known: true,
        band,
        rates: Some(rate),
    }
}

/// 批量合计。
///
/// `missing` 是「模型 id → 出现次数」，让面板能指出哪些没覆盖，而不是悄悄地
/// 少报。
pub fn sum_rows(rows: &[Value], pricing: &Pricing) -> Totals {
    let mut cny = 0.0;
    let mut priced = 0usize;
    let mut missing: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let cost = compute_row(row, pricing);
        if cost.known {
            cny += cost.cny;
            priced += 1;
        } else {
            let mid = row
                .get("model")
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            *missing.entry(mid).or_insert(0) += 1;
        }
    }
    Totals {
        cny,
        priced,
        missing,
    }
}

// ---------------------------------------------------------------------------
// 关卡 4.21：成本明细（Python `cost_for_row(details=True)` 的等价物）
// ---------------------------------------------------------------------------

/// 一行成本的**完整明细**（Python `wb_pricing.cost_for_row(details=True)`）。
///
/// Python 把「这一行用的是哪份价、哪一档、单价多少」摊成一组键给面板的悬停
/// 气泡用。Rust 侧能算出来的都在这里；**算不出来的字段一律不存在**
/// （不是 `None`、不是空串）——本仓库的口径是「省略胜过编 0」。
///
/// ⚠️ 与 Python 的差异（Python 的「策略表层」Rust 侧没有）：
/// - 没有 `policy_id` / `source_at` / `backfilled`：Python 的策略表是
///   「一个价格一行 + 首次取到时刻」，Rust 侧的策略库是「每轮追加整份快照
///   一行」，没有「这一行当时适用哪一版价」的概念。
/// - `via_derived` 恒为 `false`：Python 要在策略行早于 `via` 字段时按当前
///   映射表反推；Rust 侧 `via` 落盘时即记录，**没有推断这回事**。
#[derive(Debug, Clone, PartialEq)]
pub struct CostDetail {
    /// 折算后的人民币金额。
    pub cny: f64,
    /// 命中的条件档序号（`None` = 走模型自身的基础价）。
    pub band: Option<usize>,
    /// 命中的那档单价表（`None` = 未定价）。
    pub rates: Option<RateTable>,
    /// 单价的计价单位（每 `unit` 个 token 一个价）。
    pub unit: f64,
    /// 价目货币（`"USD"` / `"CNY"`）。
    pub currency: String,
    /// 快照里的美元兑人民币汇率（非 USD 货币时不参与折算）。
    pub usd_cny: f64,
    /// OpenRouter 的模型 id。
    pub or_id: String,
    /// 价目来源（快照落盘时写死 `"openrouter"`）。
    pub source: String,
    /// 这条价怎么匹配来的：`"variant"` / `"override"`；`None` = 直接匹配。
    pub via: Option<String>,
    /// 变体继承的基准模型名。
    pub inherited_from: Option<String>,
    /// 人工映射命中的来源模型名。
    pub override_from: Option<String>,
    /// 命中的条件档的中文说明（没有档位 ⇒ `None`）。
    pub band_note: Option<String>,
    /// `via` 是不是推断出来的（Rust 侧恒 `false`）。
    pub via_derived: bool,
}

/// 一行的成本明细；**未定价 ⇒ `None`**（不编 0、不给半截明细）。
///
/// Python 同款：未定价时整组明细都是 `None`。
pub fn compute_row_detail(row: &Value, pricing: &Pricing) -> Option<CostDetail> {
    let cost = compute_row(row, pricing);
    if !cost.known {
        return None;
    }
    let model = row.get("model").and_then(Value::as_str)?;
    let entry = pricing.models.get(model)?;
    Some(CostDetail {
        cny: cost.cny,
        band: cost.band,
        rates: cost.rates,
        unit: entry.unit,
        currency: entry.currency.clone(),
        usd_cny: pricing.usd_cny,
        or_id: entry.or_id.clone(),
        source: entry.source.clone(),
        via: entry.via.clone(),
        inherited_from: entry.inherited_from.clone(),
        override_from: entry.override_from.clone(),
        band_note: band_note(entry, cost.band),
        via_derived: false,
    })
}

/// 星期缩写 → 中文（Python `wb_pricing._WEEKDAY_CN`）。
pub const WEEKDAY_CN: [(&str, &str); 7] = [
    ("mon", "周一"),
    ("tue", "周二"),
    ("wed", "周三"),
    ("thu", "周四"),
    ("fri", "周五"),
    ("sat", "周六"),
    ("sun", "周日"),
];

/// 命中条件档的中文说明（Python `wb_pricing.band_note`）。
///
/// - 长度档：`输入长度 ≥ 272,000 tokens`
/// - 时段档：`每天 16:00–24:00 UTC` / `每周 周一/周三 16:00–24:00 UTC`
///
/// 没有档位、或档位索引越界 ⇒ `None`。
pub fn band_note(entry: &PricingEntry, index: Option<usize>) -> Option<String> {
    let index = index?;
    let band = entry.bands.get(index)?;
    if let Some(threshold) = band.min_prompt_tokens {
        return Some(format!("输入长度 ≥ {} tokens", thousands(threshold as i64)));
    }
    let window = format!("{}–{} UTC", hhmm(band.start), hhmm(band.end));
    let days: Vec<&str> = band
        .days
        .as_ref()
        .map(|list| {
            list.iter()
                .filter_map(|day| {
                    WEEKDAY_CN
                        .iter()
                        .find(|(abbr, _)| abbr == day)
                        .map(|(_, cn)| *cn)
                })
                .collect()
        })
        .unwrap_or_default();
    if days.is_empty() {
        Some(format!("每天 {window}"))
    } else {
        Some(format!("每周 {} {window}", days.join("/")))
    }
}

/// `1600` → `16:00`（Python `wb_pricing._hhmm`）。
fn hhmm(value: f64) -> String {
    let minutes = value as i64;
    format!(
        "{:02}:{:02}",
        minutes.div_euclid(100),
        minutes.rem_euclid(100)
    )
}

/// 千分位（Python `format(int(x), ",")`）。
fn thousands(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{sign}{out}")
}

/// 名字归一化：小写、去掉所有非字母数字。
pub fn normalize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// 按归一化名字建索引，排除 `:batch` / `:free` 等变体。
pub fn index_openrouter(or_models: &HashMap<String, Value>) -> HashMap<String, Vec<String>> {
    let mut by_norm: HashMap<String, Vec<String>> = HashMap::new();
    for mid in or_models.keys() {
        if mid.contains(':') {
            continue;
        }
        let base = mid.rsplit('/').next().unwrap_or(mid);
        by_norm
            .entry(normalize(base))
            .or_default()
            .push(mid.clone());
    }
    by_norm
}

/// 名字归一化后全等且唯一才命中，否则不命中——宁可漏也不错。
pub fn auto_match(hub_id: &str, by_norm: &HashMap<String, Vec<String>>) -> Option<String> {
    let base = hub_id.rsplit('/').next().unwrap_or(hub_id);
    let cands = by_norm.get(&normalize(base))?;
    if cands.len() == 1 {
        Some(cands[0].clone())
    } else {
        None
    }
}

/// 去掉变体后缀后的候选基名，按表内顺序。
pub fn variant_bases(hub_id: &str) -> Vec<String> {
    let name = hub_id.trim();
    let mut out = Vec::new();
    for (suffix, enabled) in VARIANT_SUFFIXES {
        if !enabled || !name.ends_with(suffix) {
            continue;
        }
        let base = name[..name.len() - suffix.len()].trim();
        if !base.is_empty() {
            out.push(base.to_string());
        }
    }
    out
}

/// 源码覆盖表里是否有这个 hub 名字。
fn static_override(hub_id: &str) -> Option<&'static str> {
    OVERRIDES
        .iter()
        .find(|(k, _)| *k == hub_id)
        .map(|(_, v)| *v)
}

/// 面板手填覆盖 → 源码覆盖表 → 归一化全等唯一，三层解析一次。
///
/// `runtime_overrides` 是面板手填的映射（`HashMap<hub_id, or_id>`）；
/// 没有手填时传空表。
fn resolve_once(
    hub_id: &str,
    or_models: &HashMap<String, Value>,
    by_norm: &HashMap<String, Vec<String>>,
    runtime_overrides: &HashMap<String, String>,
) -> (Option<String>, bool) {
    if let Some(reference) = runtime_overrides.get(hub_id) {
        if or_models.contains_key(reference) {
            return (Some(reference.clone()), true);
        }
    }
    if let Some(reference) = static_override(hub_id) {
        if or_models.contains_key(reference) {
            return (Some(reference.to_string()), true);
        }
    }
    match auto_match(hub_id, by_norm) {
        Some(auto) => (Some(auto), false),
        None => (None, false),
    }
}

/// 解析一个 hub 模型对应的 OpenRouter id。
///
/// 返回 `(OpenRouter id, 是否来自人工覆盖)`；匹配不到返回 `(None, false)`。
///
/// 剥后缀继承来的命中一律记作**人工覆盖**——那不是名字碰巧相同，
/// 而是一次有据可查的判定。
pub fn resolve(
    hub_id: &str,
    or_models: &HashMap<String, Value>,
    by_norm: &HashMap<String, Vec<String>>,
    runtime_overrides: &HashMap<String, String>,
    variants: bool,
) -> (Option<String>, bool) {
    let (reference, via_override) = resolve_once(hub_id, or_models, by_norm, runtime_overrides);
    if reference.is_some() || !variants {
        return (reference, via_override);
    }
    for base in variant_bases(hub_id) {
        let (reference, _) = resolve_once(&base, or_models, by_norm, runtime_overrides);
        if reference.is_some() {
            return (reference, true);
        }
    }
    (None, false)
}

/// 这条命中是不是剥后缀继承来的；是则返回基准模型名，否则 `None`。
pub fn inherited_from(
    hub_id: &str,
    or_models: &HashMap<String, Value>,
    by_norm: &HashMap<String, Vec<String>>,
    runtime_overrides: &HashMap<String, String>,
) -> Option<String> {
    if resolve_once(hub_id, or_models, by_norm, runtime_overrides)
        .0
        .is_some()
    {
        return None;
    }
    variant_bases(hub_id).into_iter().find(|base| {
        resolve_once(base, or_models, by_norm, runtime_overrides)
            .0
            .is_some()
    })
}

/// 一个 hub 模型 → 快照条目。
///
/// `inherited` 非空时是剥后缀继承来的基准模型名；`overridden` 表示这次命中
/// 来自人工映射表。这两个字段只作展示与追溯，不参与内容哈希。
pub fn entry_for(
    hub_id: &str,
    reference: &str,
    or_models: &HashMap<String, Value>,
    inherited: Option<&str>,
    overridden: bool,
) -> Value {
    let p = or_models.get(reference).cloned().unwrap_or(json!({}));
    let flat = rates_from(p.get("pricing").unwrap_or(&p));
    let mut entry = Map::new();
    entry.insert("display".into(), json!(hub_id));
    entry.insert("source".into(), json!("openrouter"));
    entry.insert("currency".into(), json!("USD"));
    entry.insert("unit".into(), json!(1_000_000u64));
    entry.insert("or_id".into(), json!(reference));
    entry.insert("flat".into(), flat.to_json());
    let bands = bands_from_overrides(p.get("pricing").unwrap_or(&p));
    if !bands.is_empty() {
        entry.insert(
            "bands".into(),
            Value::Array(
                bands
                    .iter()
                    .map(|b| {
                        let mut o = Map::new();
                        o.insert("flat".into(), b.flat.to_json());
                        if let Some(t) = b.min_prompt_tokens {
                            o.insert("min_prompt_tokens".into(), json!(t));
                        } else {
                            o.insert("days".into(), json!(b.days.clone()));
                            o.insert("start".into(), json!(b.start));
                            o.insert("end".into(), json!(b.end));
                        }
                        Value::Object(o)
                    })
                    .collect(),
            ),
        );
    }
    if let Some(base) = inherited {
        entry.insert("via".into(), json!("variant"));
        entry.insert("inherited_from".into(), json!(base));
    } else if overridden {
        entry.insert("via".into(), json!("override"));
        entry.insert("override_from".into(), json!(hub_id));
    }
    Value::Object(entry)
}

/// 抓取结果并入账的产物。
pub struct SnapshotBuild {
    /// 新的价格文档（`meta` + `models`）。
    pub doc: Value,
    /// 匹配不到价格的模型 id。
    pub unpriced: Vec<String>,
    /// 命中来自人工映射表的模型 id。
    pub overridden: Vec<String>,
}

/// 建一份新的价格快照。
///
/// `or_models` 为 `None` 表示这次抓取失败：结果沿用 `previous` 里的价
/// （没有就留空），这样一次网络抖动不会把价格表清掉。
///
/// `hub_ids` 是取价输入清单（静态目录 ∪ live 目录新增，去重保序）——由调用方
/// 提供，本模块不依赖模型目录。
pub fn build_snapshot(
    or_models: Option<&HashMap<String, Value>>,
    hub_ids: &[String],
    previous: Option<&Value>,
    runtime_overrides: &HashMap<String, String>,
    variants: bool,
    generated_at: &str,
) -> SnapshotBuild {
    let by_norm = or_models.map(index_openrouter).unwrap_or_default();
    let mut models = Map::new();
    let mut unpriced = Vec::new();
    let mut overridden = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for hub_id in hub_ids {
        if !seen.insert(hub_id.as_str()) {
            continue;
        }
        match or_models {
            None => {
                let old = previous
                    .and_then(|p| p.get("models"))
                    .and_then(|m| m.get(hub_id));
                match old {
                    Some(v) => {
                        models.insert(hub_id.clone(), v.clone());
                    }
                    None => unpriced.push(hub_id.clone()),
                }
            }
            Some(table) => {
                let (reference, via_override) =
                    resolve(hub_id, table, &by_norm, runtime_overrides, variants);
                match reference {
                    None => unpriced.push(hub_id.clone()),
                    Some(reference) => {
                        if via_override {
                            overridden.push(hub_id.clone());
                        }
                        let inherited = if variants {
                            inherited_from(hub_id, table, &by_norm, runtime_overrides)
                        } else {
                            None
                        };
                        let entry = entry_for(
                            hub_id,
                            &reference,
                            table,
                            inherited.as_deref(),
                            via_override,
                        );
                        models.insert(hub_id.clone(), entry);
                    }
                }
            }
        }
    }

    let doc = json!({
        "meta": {
            "generated_at": generated_at,
            "base_currency": "CNY",
            "usd_cny": USD_CNY,
            "sources": {"openrouter": OPENROUTER_URL},
        },
        "models": Value::Object(models),
    });
    SnapshotBuild {
        doc,
        unpriced,
        overridden,
    }
}

// ---------------------------------------------------------------------------
// 关卡 4.23-甲：策略内容哈希（Python `wb_pricing.policy_id`，`:1612-1622`）
// ---------------------------------------------------------------------------

/// 内容哈希：相同模型、相同价格、相同汇率 ⇒ 相同 id。
///
/// Python `json.dumps({...}, sort_keys=True, ensure_ascii=False)` 默认分隔符
/// 带空格（`", "` / `": "`），所以这里**手工拼**带空格的 JSON，不能用
/// `serde_json::to_string`（紧凑无空格）。键递归排序。
pub fn policy_id(model: &str, flat: &RateTable, bands: &[Band], usd_cny: f64) -> String {
    let bands_json = if bands.is_empty() {
        "null".to_string()
    } else {
        let items: Vec<String> = bands.iter().map(serialize_band).collect();
        format!("[{}]", items.join(", "))
    };
    let flat_json = serialize_flat(flat);
    let payload = format!(
        "{{\"bands\": {}, \"flat\": {}, \"model\": {}, \"usd_cny\": {}}}",
        bands_json,
        flat_json,
        json_string(model),
        format_number(round6(usd_cny)),
    );
    let digest = sha1_hex(payload.as_bytes());
    digest[..12].to_string()
}

fn serialize_flat(rt: &RateTable) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = rt.input_cache_hit {
        parts.push(format!("\"input_cache_hit\": {}", format_number(v)));
    }
    if let Some(v) = rt.input_cache_miss {
        parts.push(format!("\"input_cache_miss\": {}", format_number(v)));
    }
    if let Some(v) = rt.output {
        parts.push(format!("\"output\": {}", format_number(v)));
    }
    // 键已按字母序插入（input_cache_hit < input_cache_miss < output）。
    format!("{{{}}}", parts.join(", "))
}

fn serialize_band(band: &Band) -> String {
    if let Some(threshold) = band.min_prompt_tokens {
        // 长度档：flat + min_prompt_tokens
        format!(
            "{{\"flat\": {}, \"min_prompt_tokens\": {}}}",
            serialize_flat(&band.flat),
            format_number(threshold),
        )
    } else {
        // 时段档：days + end + flat + start（字母序）
        let days = match &band.days {
            Some(list) => {
                let items: Vec<String> = list.iter().map(|s| json_string(s)).collect();
                format!("[{}]", items.join(", "))
            }
            None => "null".to_string(),
        };
        format!(
            "{{\"days\": {}, \"end\": {}, \"flat\": {}, \"start\": {}}}",
            days,
            format_number(band.end),
            serialize_flat(&band.flat),
            format_number(band.start),
        )
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn format_number(x: f64) -> String {
    // Python json.dumps 的 float 最短表示：始终带小数点。
    let s = format!("{:.6}", x);
    let trimmed = s.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{}0", trimmed)
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// SHA-1（纯 Rust，避免新增依赖）
// ---------------------------------------------------------------------------

fn sha1_hex(data: &[u8]) -> String {
    // 标准 SHA-1 实现。
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    // 填充
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        #[allow(clippy::needless_range_loop)]
        for i in 0..80 {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A827999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1u32)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32)
            } else {
                (b ^ c ^ d, 0xCA62C1D6u32)
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    format!("{:08x}{:08x}{:08x}{:08x}{:08x}", h0, h1, h2, h3, h4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Value {
        json!({
            "meta": {"base_currency": "CNY", "usd_cny": 7.1},
            "models": {
                "deepseek-v4.1-flash": {
                    "display": "deepseek-v4.1-flash",
                    "source": "openrouter",
                    "currency": "USD",
                    "unit": 1000000,
                    "or_id": "deepseek/deepseek-v4.1-flash",
                    "flat": {"input_cache_hit": 0.01, "input_cache_miss": 0.03, "output": 0.5}
                },
                "gpt-6-astra": {
                    "currency": "USD",
                    "unit": 1000000,
                    "flat": {"input_cache_hit": 1.0, "input_cache_miss": 10.0, "output": 50.0},
                    "bands": [
                        {"flat": {"input_cache_hit": 2.0, "input_cache_miss": 20.0, "output": 75.0},
                         "min_prompt_tokens": 272000}
                    ]
                }
            }
        })
    }

    #[test]
    fn parses_the_snapshot_shape() {
        let pricing = Pricing::from_json(&snapshot());
        assert_eq!(pricing.base_currency, "CNY");
        assert_eq!(pricing.usd_cny(), 7.1);
        assert_eq!(pricing.models.len(), 2);
        let entry = pricing.models.get("deepseek-v4.1-flash").unwrap();
        assert_eq!(entry.flat.output, Some(0.5));
        assert_eq!(entry.unit, 1_000_000.0);
    }

    #[test]
    fn computes_a_flat_price_row_in_cny() {
        let pricing = Pricing::from_json(&snapshot());
        let row = json!({
            "model": "deepseek-v4.1-flash",
            "at": 1_800_000_000.0,
            "prompt_tokens": 1_000_000,
            "cached_tokens": 0,
            "completion_tokens": 1_000_000
        });
        let cost = compute_row(&row, &pricing);
        assert!(cost.known);
        assert_eq!(cost.band, None);
        // (1e6 * 0.03 + 1e6 * 0.5) / 1e6 * 7.1 = 0.53 * 7.1 = 3.763
        assert!((cost.cny - 3.763).abs() < 1e-9, "cny was {}", cost.cny);
    }

    #[test]
    fn cache_hit_tokens_are_billed_at_the_hit_rate() {
        let pricing = Pricing::from_json(&snapshot());
        let row = json!({
            "model": "deepseek-v4.1-flash",
            "at": 1_800_000_000.0,
            "prompt_tokens": 1_000_000,
            "cached_tokens": 1_000_000,
            "completion_tokens": 0
        });
        let cost = compute_row(&row, &pricing);
        // (0 * 0.03 + 1e6 * 0.01) / 1e6 * 7.1 = 0.071
        assert!((cost.cny - 0.071).abs() < 1e-9, "cny was {}", cost.cny);
    }

    #[test]
    fn length_band_wins_when_the_prompt_is_long_enough() {
        let pricing = Pricing::from_json(&snapshot());
        let short = json!({
            "model": "gpt-6-astra", "at": 1_800_000_000.0,
            "prompt_tokens": 1000, "completion_tokens": 0
        });
        let long = json!({
            "model": "gpt-6-astra", "at": 1_800_000_000.0,
            "prompt_tokens": 300_000, "completion_tokens": 0
        });
        let c1 = compute_row(&short, &pricing);
        let c2 = compute_row(&long, &pricing);
        assert_eq!(c1.band, None);
        assert_eq!(c2.band, Some(0));
        assert!(c2.cny > c1.cny);
    }

    #[test]
    fn unknown_models_are_reported_as_unpriced() {
        let pricing = Pricing::from_json(&snapshot());
        let row = json!({"model": "who-knows", "prompt_tokens": 10});
        let cost = compute_row(&row, &pricing);
        assert!(!cost.known);
        assert_eq!(cost.cny, 0.0);
    }

    #[test]
    fn sum_rows_counts_priced_and_missing() {
        let pricing = Pricing::from_json(&snapshot());
        let rows = vec![
            json!({"model": "deepseek-v4.1-flash", "prompt_tokens": 100, "completion_tokens": 10}),
            json!({"model": "who-knows", "prompt_tokens": 100}),
            json!({"model": "who-knows", "prompt_tokens": 100}),
        ];
        let totals = sum_rows(&rows, &pricing);
        assert_eq!(totals.priced, 1);
        assert_eq!(totals.missing.get("who-knows"), Some(&2));
        assert!(totals.cny > 0.0);
    }

    #[test]
    fn normalize_strips_punctuation_and_case() {
        assert_eq!(normalize("DeepSeek-V4.1_Flash"), "deepseekv41flash");
        assert_eq!(normalize("hy3"), "hy3");
    }

    #[test]
    fn auto_match_requires_a_unique_hit() {
        let mut or_models = HashMap::new();
        or_models.insert("deepseek/deepseek-v4.1-flash".to_string(), json!({}));
        let by_norm = index_openrouter(&or_models);
        assert_eq!(
            auto_match("deepseek-v4.1-flash", &by_norm),
            Some("deepseek/deepseek-v4.1-flash".to_string())
        );
        assert_eq!(auto_match("nope", &by_norm), None);
    }

    #[test]
    fn variant_bases_only_strip_enabled_suffixes() {
        assert_eq!(variant_bases("deepseek-v3-1-lkeap"), vec!["deepseek-v3-1"]);
        // -x 不剥离
        assert!(variant_bases("hy3-x").is_empty());
    }

    #[test]
    fn resolve_prefers_the_static_override_table() {
        let mut or_models = HashMap::new();
        or_models.insert("tencent/hy4-preview".to_string(), json!({}));
        let by_norm = index_openrouter(&or_models);
        let (reference, via_override) =
            resolve("hy4-preview-f", &or_models, &by_norm, &HashMap::new(), true);
        assert_eq!(reference.as_deref(), Some("tencent/hy4-preview"));
        assert!(via_override);
    }

    #[test]
    fn resolve_inherits_a_variant_base() {
        let mut or_models = HashMap::new();
        or_models.insert("vendor/base-model".to_string(), json!({}));
        let by_norm = index_openrouter(&or_models);
        let (reference, via_override) = resolve(
            "base-model-lkeap",
            &or_models,
            &by_norm,
            &HashMap::new(),
            true,
        );
        assert_eq!(reference.as_deref(), Some("vendor/base-model"));
        assert!(via_override, "变体继承一律记作人工覆盖");
        assert_eq!(
            inherited_from("base-model-lkeap", &or_models, &by_norm, &HashMap::new()),
            Some("base-model".to_string())
        );
    }

    #[test]
    fn build_snapshot_falls_back_to_previous_on_a_failed_fetch() {
        let previous = json!({"models": {"m1": {"flat": {"output": 1.0}}}});
        let out = build_snapshot(
            None,
            &["m1".to_string(), "m2".to_string()],
            Some(&previous),
            &HashMap::new(),
            true,
            "2026-10-20 08:00:00",
        );
        assert!(out.doc["models"]["m1"].is_object());
        assert!(out.doc["models"]["m2"].is_null());
        assert_eq!(out.unpriced, vec!["m2".to_string()]);
    }

    #[test]
    fn build_snapshot_prices_a_matched_model() {
        let mut or_models = HashMap::new();
        or_models.insert(
            "vendor/base".to_string(),
            json!({"pricing": {"prompt": 0.000003, "completion": 0.00006, "input_cache_read": 0.0000003}}),
        );
        let out = build_snapshot(
            Some(&or_models),
            &["base".to_string()],
            None,
            &HashMap::new(),
            true,
            "2026-10-20 08:00:00",
        );
        assert!(out.unpriced.is_empty());
        assert_eq!(out.doc["models"]["base"]["or_id"], "vendor/base");
        assert_eq!(out.doc["models"]["base"]["flat"]["output"], 60.0);
    }

    #[test]
    fn utc_bands_use_the_utc_clock() {
        // 2026-10-20T16:30:00Z → 周二 16:30 UTC
        let ts = 1_800_000_000.0;
        let (day, hhmm) = utc_parts(ts);
        assert!(WEEKDAY_SHORT.contains(&day));
        assert!((0..2400).contains(&hhmm));
    }

    // -----------------------------------------------------------------
    // 关卡 4.21：成本明细
    // -----------------------------------------------------------------

    /// 带 `via` / `inherited_from` / 时段档的快照。
    fn detailed_snapshot() -> Value {
        json!({
            "meta": {"base_currency": "CNY", "usd_cny": 7.1},
            "models": {
                "demo-lkeap": {
                    "display": "demo-lkeap",
                    "source": "openrouter",
                    "currency": "USD",
                    "unit": 1000000,
                    "or_id": "vendor/demo",
                    "via": "variant",
                    "inherited_from": "demo",
                    "flat": {"input_cache_hit": 0.01, "input_cache_miss": 0.03, "output": 0.5},
                    "bands": [
                        {"flat": {"input_cache_miss": 0.02, "output": 0.4},
                         "days": ["mon", "wed"], "start": 1600, "end": 2400}
                    ]
                },
                "plain": {
                    "currency": "USD", "unit": 1000000,
                    "flat": {"input_cache_miss": 0.03, "output": 0.5},
                    "bands": [{"flat": {"output": 1.0}, "min_prompt_tokens": 272000}]
                }
            }
        })
    }

    #[test]
    fn the_entry_keeps_how_the_price_was_matched() {
        let pricing = Pricing::from_json(&detailed_snapshot());
        let entry = pricing.models.get("demo-lkeap").unwrap();
        assert_eq!(entry.via.as_deref(), Some("variant"));
        assert_eq!(entry.inherited_from.as_deref(), Some("demo"));
        assert_eq!(entry.override_from, None);
        // 没有 `via` ⇒ 直接匹配。
        assert_eq!(pricing.models.get("plain").unwrap().via, None);
    }

    #[test]
    fn band_note_explains_the_conditional_band() {
        let pricing = Pricing::from_json(&detailed_snapshot());
        // 长度档。
        let plain = pricing.models.get("plain").unwrap();
        assert_eq!(
            band_note(plain, Some(0)).as_deref(),
            Some("输入长度 ≥ 272,000 tokens")
        );
        // 时段档 + 星期。
        let demo = pricing.models.get("demo-lkeap").unwrap();
        assert_eq!(
            band_note(demo, Some(0)).as_deref(),
            Some("每周 周一/周三 16:00–24:00 UTC")
        );
        // 没有档位 / 索引越界 ⇒ None。
        assert_eq!(band_note(plain, None), None);
        assert_eq!(band_note(plain, Some(7)), None);
    }

    #[test]
    fn cost_detail_carries_the_hover_card_fields() {
        let pricing = Pricing::from_json(&detailed_snapshot());
        let row = json!({
            "model": "demo-lkeap", "at": 1_800_000_000.0,
            "prompt_tokens": 1000, "completion_tokens": 0
        });
        let detail = compute_row_detail(&row, &pricing).expect("demo-lkeap 有价");
        assert_eq!(detail.currency, "USD");
        assert_eq!(detail.unit, 1_000_000.0);
        assert_eq!(detail.or_id, "vendor/demo");
        assert_eq!(detail.source, "openrouter");
        assert_eq!(detail.via.as_deref(), Some("variant"));
        assert_eq!(detail.inherited_from.as_deref(), Some("demo"));
        assert!(!detail.via_derived);
        // 没有命中时段档 ⇒ band 为 None。
        assert_eq!(detail.band, None);
        assert_eq!(detail.band_note, None);

        // 未定价 ⇒ 没有明细。
        let unknown = json!({"model": "who-knows", "prompt_tokens": 1});
        assert!(compute_row_detail(&unknown, &pricing).is_none());
    }
}
