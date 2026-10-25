//! 关卡 4.21（线 A）：定价策略库的**读取端**。
//!
//! 权威源：Python `wb_pricing.py` 的 `_data_paths()`（`:1456-1468`）与
//! `record_policies()`（`:1940-2008`）。⚠️ **Rust 侧写的格式与 Python 不同**
//! （D56）：Python 是「一个价格一行 + 内容哈希去重」，Rust 侧是「每轮追加
//! 整份快照一行」⇒ 读取端必须按 **Rust 自己的格式**实现，不能照抄 Python
//! 的读法。
//!
//! 本模块把 `usage/pricing-policies.jsonl` 里的快照行读成
//! [`crate::price::Pricing`]，供成本估算（`crate::price::compute_row` /
//! `sum_rows`）使用。
//!
//! ## 「取最后一行」的依据
//!
//! 写端在 [`crate::price_refresher::write_snapshot_files`]：每刷新成功一次，
//! 就把**整份快照**（`meta` + `models`）作为**一行**追加进策略库
//! （`activity::append_row` 的原子 append）。⇒ 文件里的行序 = 刷新轮次序，
//! **最后一行就是最新的一份快照**。
//!
//! 这与 Python 的策略表**语义不同**（Python 的一行是一份「不可变策略」，
//! 按内容哈希去重，同一份价不会重复落盘），所以读取端不能照抄 Python 的
//! `load_policies()`。
//!
//! ## 容错口径
//!
//! - **文件不存在 / 空 / 全是坏行 ⇒ `None`**（不报错）：定价是锦上添花，
//!   读不到价表不该让面板挂掉。
//! - **末行是坏行 ⇒ 退到上一份好快照**（坏行跳过，与 [`crate::usage`]
//!   的 `parse_jsonl` 同策略）。
//! - **IO 失败（权限/磁盘）⇒ `None` + 一行 stderr 日志**（不静默吞掉）。

use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::price::Pricing;

/// 策略库文件名（写端在 [`crate::price_refresher`]）。
pub use crate::price_refresher::POLICIES_FILE;

/// 读末尾这么多字节就够找最新一份快照（策略库每轮追加一行，稳态下几 KB 一行）。
const TAIL_LIMIT: u64 = 8 * 1024 * 1024;

/// 策略库路径（`<数据目录>/pricing-policies.jsonl`）。
pub fn policies_path(usage_dir: &Path) -> PathBuf {
    usage_dir.join(POLICIES_FILE)
}

/// 一份快照文档是不是定价表（Python `load_pricing` 的形状检查：
/// `isinstance(loaded.get("models"), dict)`）。
fn is_snapshot(doc: &Value) -> bool {
    doc.get("models").is_some_and(Value::is_object)
}

/// 从一份快照文档构造定价表；形状不对 ⇒ `None`。
fn pricing_from_snapshot(doc: &Value) -> Option<Pricing> {
    if !is_snapshot(doc) {
        return None;
    }
    Some(Pricing::from_json(doc))
}

/// 从若干行快照里取**最后一份有效快照**构造定价表（纯函数）。
///
/// `rows` 是策略库按文件序解析出来的行。**从后往前找第一份形状正确的快照**
/// ⇒ 末行坏掉时自动退到上一份好快照（安全降级），全坏 ⇒ `None`。
pub fn load_pricing_from_rows(rows: &[Value]) -> Option<Pricing> {
    rows.iter().rev().find_map(pricing_from_snapshot)
}

/// 读策略库里的最新一份快照。
///
/// 读不到（文件不存在 / 空 / 全坏 / IO 失败）⇒ `None`。
pub fn load_pricing(usage_dir: &Path) -> Option<Pricing> {
    let path = policies_path(usage_dir);
    match read_tail_lines(&path) {
        Ok((lines, truncated)) => {
            for line in lines.iter().rev() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(doc) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(pricing) = pricing_from_snapshot(&doc) {
                        return Some(pricing);
                    }
                }
            }
            // 末尾这一段没有有效快照：文件被截断在两行之间时才会这样
            // （策略库每轮涨一行，坏行又不多见），退化成全文件扫描。
            if truncated {
                return last_snapshot_in_file(&path);
            }
            None
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            eprintln!("[wb-tasks] 读定价策略库失败: {err}");
            None
        }
    }
}

/// 读文件末尾的行；第二个返回值是「文件比读到的这一段还长」。
///
/// 策略库**每轮追加一行**（见模块文档）⇒ 文件只增不减 ⇒ 只需要末尾几行。
/// 文件比 [`TAIL_LIMIT`] 还大时只读末尾那一段（切在半行中间的那一行丢掉）。
fn read_tail_lines(path: &Path) -> Result<(Vec<String>, bool), std::io::Error> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len <= TAIL_LIMIT {
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        return Ok((text.lines().map(str::to_string).collect(), false));
    }
    file.seek(SeekFrom::Start(len - TAIL_LIMIT))?;
    let mut buf = vec![0u8; TAIL_LIMIT as usize];
    file.read_exact(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // 第一行多半被切在半行中间 ⇒ 丢掉。
    if !lines.is_empty() {
        lines.remove(0);
    }
    Ok((lines, true))
}

/// 全文件扫描，取最后一份有效快照（末尾那一段没命中时的兜底）。
fn last_snapshot_in_file(path: &Path) -> Option<Pricing> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut last: Option<Pricing> = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(doc) = serde_json::from_str::<Value>(trimmed) {
            if let Some(pricing) = pricing_from_snapshot(&doc) {
                last = Some(pricing);
            }
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(price: f64) -> Value {
        json!({
            "meta": {"base_currency": "CNY", "usd_cny": 7.1, "generated_at": "2026-10-22 08:00:00"},
            "models": {
                "m1": {
                    "display": "m1",
                    "source": "openrouter",
                    "currency": "USD",
                    "unit": 1000000,
                    "or_id": "vendor/m1",
                    "flat": {"input_cache_hit": 0.01, "input_cache_miss": 0.03, "output": price}
                }
            }
        })
    }

    #[test]
    fn the_last_row_wins() {
        let rows = vec![snapshot(0.5), snapshot(0.9)];
        let pricing = load_pricing_from_rows(&rows).unwrap();
        let entry = pricing.models.get("m1").unwrap();
        assert_eq!(entry.flat.output, Some(0.9));
    }

    #[test]
    fn a_broken_last_row_falls_back_to_the_previous_one() {
        let rows = vec![snapshot(0.5), json!({"nope": true})];
        let pricing = load_pricing_from_rows(&rows).unwrap();
        assert_eq!(pricing.models.get("m1").unwrap().flat.output, Some(0.5));
    }

    #[test]
    fn no_usable_row_gives_none() {
        assert!(load_pricing_from_rows(&[]).is_none());
        assert!(load_pricing_from_rows(&[json!({"x": 1}), json!([1, 2])]).is_none());
    }
}

use crate::price::{Band, PricingEntry, RateTable};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

static LAST_USAGE_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

fn remember_dir(dir: &Path) {
    if let Ok(mut g) = LAST_USAGE_DIR.lock() {
        *g = Some(dir.to_path_buf());
    }
}

pub fn set_usage_dir(dir: &Path) {
    remember_dir(dir);
}

fn current_usage_dir() -> Option<std::path::PathBuf> {
    LAST_USAGE_DIR.lock().ok().and_then(|g| g.clone())
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub id: String,
    pub model: String,
    pub or_id: String,
    pub currency: String,
    pub unit: f64,
    pub usd_cny: f64,
    pub flat: RateTable,
    pub bands: Vec<Band>,
    pub via: Option<String>,
    pub inherited_from: Option<String>,
    pub override_from: Option<String>,
    pub first_seen: Option<f64>,
}

pub fn load_policies(usage_dir: &Path) -> HashMap<String, Policy> {
    remember_dir(usage_dir);
    let mut out = HashMap::new();
    let path = usage_dir.join(crate::price_refresher::POLICIES_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return out,
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(id) = doc.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let model = doc
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let or_id = doc
            .get("or_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let currency = doc
            .get("currency")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("USD")
            .to_string();
        let unit = doc
            .get("unit")
            .and_then(serde_json::Value::as_f64)
            .filter(|u| *u != 0.0)
            .unwrap_or(1_000_000.0);
        let usd_cny = doc
            .get("usd_cny")
            .and_then(serde_json::Value::as_f64)
            .filter(|v| *v != 0.0)
            .unwrap_or(1.0);
        let flat = doc
            .get("flat")
            .map(RateTable::from_json)
            .unwrap_or_default();
        let bands = doc
            .get("bands")
            .and_then(serde_json::Value::as_array)
            .map(|items| items.iter().filter_map(parse_band_local).collect())
            .unwrap_or_default();
        let via = nonempty_local(doc.get("via"));
        let inherited_from = nonempty_local(doc.get("inherited_from"));
        let override_from = nonempty_local(doc.get("override_from"));
        let first_seen = doc.get("first_seen").and_then(serde_json::Value::as_f64);
        out.insert(
            id.to_string(),
            Policy {
                id: id.to_string(),
                model,
                or_id,
                currency,
                unit,
                usd_cny,
                flat,
                bands,
                via,
                inherited_from,
                override_from,
                first_seen,
            },
        );
    }
    out
}

pub fn load_timeline(usage_dir: &Path) -> Vec<serde_json::Value> {
    remember_dir(usage_dir);
    let path = usage_dir.join(crate::price_refresher::TIMELINE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(doc) = serde_json::from_str::<serde_json::Value>(trimmed) {
            out.push(doc);
        }
    }
    out
}

pub fn policy_ref_at(
    timeline: &[serde_json::Value],
    at: f64,
    model: &str,
) -> (Option<String>, bool) {
    if timeline.is_empty() {
        return (None, false);
    }
    let mut idx: isize = -1;
    for (i, row) in timeline.iter().enumerate() {
        let t = row
            .get("at")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        if t <= at {
            idx = i as isize;
        } else {
            break;
        }
    }
    let index = if idx < 0 { 0usize } else { idx as usize };

    if let Some(rows) = timeline.get(index).and_then(|r| r.get("policies")) {
        if let Some(hit) = rows.get(model).and_then(serde_json::Value::as_str) {
            return (Some(hit.to_string()), false);
        }
    }
    for row in timeline.iter() {
        let t = row
            .get("at")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        if t < at {
            continue;
        }
        if let Some(hit) = row
            .get("policies")
            .and_then(|p| p.get(model))
            .and_then(serde_json::Value::as_str)
        {
            return (Some(hit.to_string()), true);
        }
    }
    let end = (index + 1).min(timeline.len());
    for row in timeline[..end].iter().rev() {
        if let Some(hit) = row
            .get("policies")
            .and_then(|p| p.get(model))
            .and_then(serde_json::Value::as_str)
        {
            return (Some(hit.to_string()), true);
        }
    }
    (None, false)
}

pub fn cost_for_row(
    row: &serde_json::Value,
    policies: &HashMap<String, Policy>,
    fallback: Option<&Pricing>,
    enabled: bool,
) -> serde_json::Value {
    if !enabled {
        let mut cost = serde_json::Map::new();
        cost.insert("cny".into(), json!(0.0));
        cost.insert("known".into(), json!(false));
        cost.insert("band".into(), serde_json::Value::Null);
        cost.insert("source".into(), json!("builtin"));
        cost.insert("source_at".into(), serde_json::Value::Null);
        cost.insert("backfilled".into(), json!(false));
        cost.insert("disabled".into(), json!(true));
        for k in [
            "rates",
            "unit",
            "currency",
            "usd_cny",
            "or_id",
            "via",
            "inherited_from",
            "override_from",
            "band_note",
            "via_derived",
        ] {
            cost.insert(k.into(), serde_json::Value::Null);
        }
        return serde_json::Value::Object(cost);
    }

    let model = row
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    let mut pid: Option<String> = row
        .get("cost_policy")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let mut backfilled = false;
    if pid.is_none() {
        let at = row
            .get("at")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let timeline = current_usage_dir()
            .map(|d| load_timeline(&d))
            .unwrap_or_default();
        let (p, bf) = policy_ref_at(&timeline, at, model);
        pid = p;
        backfilled = bf;
    }

    if let Some(pid) = pid.clone() {
        if let Some(policy) = policies.get(&pid) {
            let mut models = std::collections::HashMap::new();
            models.insert(
                model.to_string(),
                PricingEntry {
                    display: model.to_string(),
                    source: "openrouter".to_string(),
                    currency: policy.currency.clone(),
                    unit: policy.unit,
                    or_id: policy.or_id.clone(),
                    flat: policy.flat,
                    bands: policy.bands.clone(),
                    via: policy.via.clone(),
                    inherited_from: policy.inherited_from.clone(),
                    override_from: policy.override_from.clone(),
                    id: policy.id.clone(),
                    usd_cny: policy.usd_cny,
                    first_seen: policy.first_seen,
                },
            );
            let pricing = Pricing {
                base_currency: "CNY".to_string(),
                usd_cny: policy.usd_cny,
                models,
            };
            let cost_res = crate::price::compute_row(row, &pricing);
            let entry = pricing.models.get(model);
            let band_note = entry.and_then(|e| crate::price::band_note(e, cost_res.band));
            let mut cost = serde_json::Map::new();
            cost.insert("cny".into(), json!(cost_res.cny));
            cost.insert("known".into(), json!(cost_res.known));
            cost.insert(
                "band".into(),
                cost_res
                    .band
                    .map(|b| json!(b))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert("source".into(), json!(pid));
            cost.insert(
                "source_at".into(),
                policy
                    .first_seen
                    .map(|v| json!(v))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert("backfilled".into(), json!(backfilled));
            cost.insert("disabled".into(), json!(false));
            cost.insert(
                "rates".into(),
                cost_res
                    .rates
                    .map(|r| r.to_json())
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert(
                "unit".into(),
                entry
                    .map(|e| json!(e.unit))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert(
                "currency".into(),
                entry
                    .map(|e| json!(e.currency.clone()))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert("usd_cny".into(), json!(policy.usd_cny));
            cost.insert(
                "or_id".into(),
                entry
                    .map(|e| json!(e.or_id.clone()))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert(
                "via".into(),
                json!(entry
                    .and_then(|e| e.via.clone())
                    .unwrap_or_else(|| "direct".to_string())),
            );
            cost.insert(
                "inherited_from".into(),
                entry
                    .and_then(|e| e.inherited_from.clone())
                    .map(|v| json!(v))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert(
                "override_from".into(),
                entry
                    .and_then(|e| e.override_from.clone())
                    .map(|v| json!(v))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert(
                "band_note".into(),
                band_note
                    .map(|v| json!(v))
                    .unwrap_or(serde_json::Value::Null),
            );
            cost.insert("via_derived".into(), json!(false));
            return serde_json::Value::Object(cost);
        }
    }

    if let Some(doc) = fallback {
        let cost_res = crate::price::compute_row(row, doc);
        let entry = doc.models.get(model);
        let band_note = entry.and_then(|e| crate::price::band_note(e, cost_res.band));
        let mut cost = serde_json::Map::new();
        cost.insert("cny".into(), json!(cost_res.cny));
        cost.insert("known".into(), json!(cost_res.known));
        cost.insert(
            "band".into(),
            cost_res
                .band
                .map(|b| json!(b))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert("source".into(), json!("builtin"));
        cost.insert("source_at".into(), serde_json::Value::Null);
        cost.insert("backfilled".into(), json!(false));
        cost.insert("disabled".into(), json!(false));
        cost.insert(
            "rates".into(),
            cost_res
                .rates
                .map(|r| r.to_json())
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert(
            "unit".into(),
            entry
                .map(|e| json!(e.unit))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert(
            "currency".into(),
            entry
                .map(|e| json!(e.currency.clone()))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert("usd_cny".into(), json!(doc.usd_cny));
        cost.insert(
            "or_id".into(),
            entry
                .map(|e| json!(e.or_id.clone()))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert(
            "via".into(),
            json!(entry
                .and_then(|e| e.via.clone())
                .unwrap_or_else(|| "direct".to_string())),
        );
        cost.insert(
            "inherited_from".into(),
            entry
                .and_then(|e| e.inherited_from.clone())
                .map(|v| json!(v))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert(
            "override_from".into(),
            entry
                .and_then(|e| e.override_from.clone())
                .map(|v| json!(v))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert(
            "band_note".into(),
            band_note
                .map(|v| json!(v))
                .unwrap_or(serde_json::Value::Null),
        );
        cost.insert("via_derived".into(), json!(false));
        return serde_json::Value::Object(cost);
    }

    let mut cost = serde_json::Map::new();
    cost.insert("cny".into(), json!(0.0));
    cost.insert("known".into(), json!(false));
    cost.insert("band".into(), serde_json::Value::Null);
    cost.insert("source".into(), json!("builtin"));
    cost.insert("source_at".into(), serde_json::Value::Null);
    cost.insert("backfilled".into(), json!(false));
    cost.insert("disabled".into(), json!(false));
    for k in [
        "rates",
        "unit",
        "currency",
        "usd_cny",
        "or_id",
        "via",
        "inherited_from",
        "override_from",
        "band_note",
        "via_derived",
    ] {
        cost.insert(k.into(), serde_json::Value::Null);
    }
    serde_json::Value::Object(cost)
}

fn parse_band_local(value: &serde_json::Value) -> Option<Band> {
    if !value.is_object() {
        return None;
    }
    let flat = value
        .get("flat")
        .map(RateTable::from_json)
        .unwrap_or_default();
    let min_prompt_tokens = value
        .get("min_prompt_tokens")
        .and_then(serde_json::Value::as_f64);
    let days = value
        .get("days")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
    let start = value
        .get("start")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let end = value
        .get("end")
        .and_then(serde_json::Value::as_f64)
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

fn nonempty_local(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// 当前策略指派表：`(at, assignment)`，at 是时间线最后一行的时刻，
/// assignment 是 `{model: pid}`。时间线为空 ⇒ `(0.0, {})`。
pub fn current_assignment() -> (f64, std::collections::HashMap<String, String>) {
    // 需要 usage_dir；若未设置则返回空
    let dir = match current_usage_dir() {
        Some(d) => d,
        None => return (0.0, std::collections::HashMap::new()),
    };
    let timeline = load_timeline(&dir);
    if timeline.is_empty() {
        return (0.0, std::collections::HashMap::new());
    }
    let last = &timeline[timeline.len() - 1];
    let at = last
        .get("at")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let mut assignment = std::collections::HashMap::new();
    if let Some(pol) = last.get("policies").and_then(serde_json::Value::as_object) {
        for (k, v) in pol {
            if let Some(s) = v.as_str() {
                assignment.insert(k.clone(), s.to_string());
            }
        }
    }
    (at, assignment)
}

/// 模型的当前策略 id：先查 current_assignment，没有则尝试 ensure_policy
/// （需要 OpenRouter 索引）。
pub fn current_policy_id(
    model: &str,
    or_models: Option<&std::collections::HashMap<String, serde_json::Value>>,
) -> Option<String> {
    let (_at, assignment) = current_assignment();
    if let Some(pid) = assignment.get(model).cloned() {
        return Some(pid);
    }
    let or_models = or_models?;
    let usage_dir = current_usage_dir()?;
    ensure_policy(model, or_models, &usage_dir)
}

/// ensure_policy：按 Python `wb_pricing.ensure_policy` 的移植。
///
/// - pricing 未启用 ⇒ None（调用方自行判断；本函数不读 settings，由调用方保证）
/// - or_models 为空 ⇒ None
/// - 解析不到 OpenRouter id ⇒ None
/// - 否则构造单模型快照，merge 写入策略库，返回 assignment 中的 pid
pub fn ensure_policy(
    model: &str,
    or_models: &std::collections::HashMap<String, serde_json::Value>,
    usage_dir: &std::path::Path,
) -> Option<String> {
    use serde_json::json;
    if or_models.is_empty() {
        return None;
    }
    let mid = model.trim().to_string();
    if mid.is_empty() {
        return None;
    }
    let by_norm = crate::price::index_openrouter(or_models);
    let inherited =
        crate::price::inherited_from(&mid, or_models, &by_norm, &std::collections::HashMap::new());
    let (reference, via_override) = crate::price::resolve(
        &mid,
        or_models,
        &by_norm,
        &std::collections::HashMap::new(),
        true,
    );
    let reference = reference?;
    let entry = crate::price::entry_for(
        &mid,
        &reference,
        or_models,
        inherited.as_deref(),
        via_override,
    );
    let doc = json!({
        "meta": {"base_currency": "CNY", "usd_cny": crate::price::USD_CNY},
        "models": { mid.clone(): entry }
    });
    // 落盘（merge=True）。
    if crate::price_refresher::write_snapshot_files(
        usage_dir,
        &doc,
        crate::activity::now_epoch(),
        true,
    )
    .is_err()
    {
        return None;
    }
    // 计算 pid（与 write_snapshot_files 内部一致）。
    let flat = crate::price::RateTable::from_json(
        doc.get("models")
            .and_then(|m| m.get(&mid))
            .and_then(|e| e.get("flat"))
            .unwrap_or(&serde_json::Value::Null),
    );
    let bands: Vec<crate::price::Band> = doc
        .get("models")
        .and_then(|m| m.get(&mid))
        .and_then(|e| e.get("bands"))
        .and_then(serde_json::Value::as_array)
        .map(|items| items.iter().filter_map(crate::price::parse_band).collect())
        .unwrap_or_default();
    let pid = crate::price::policy_id(&mid, &flat, &bands, crate::price::USD_CNY);
    // 记住 usage_dir 供 current_assignment 使用。
    remember_dir(usage_dir);
    Some(pid)
}

/// usage 日志里所有行引用到的策略 id 集合。
pub fn referenced_policy_ids(usage_dir: &Path) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    let path = crate::usage::usage_log_path(usage_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return ids,
    };
    for line in text.lines() {
        if !line.contains("\"cost_policy\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if let Some(pid) = row.get("cost_policy").and_then(serde_json::Value::as_str) {
            if !pid.trim().is_empty() {
                ids.insert(pid.to_string());
            }
        }
    }
    ids
}

/// 丢弃没有任何引用的策略行。
pub fn prune_policies(usage_dir: &Path, protect: &[String]) -> Vec<String> {
    remember_dir(usage_dir);

    let policies = load_policies(usage_dir);
    if policies.is_empty() {
        return Vec::new();
    }

    let (_at, assignment) = current_assignment();
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    for v in assignment.values() {
        keep.insert(v.clone());
    }
    for id in referenced_policy_ids(usage_dir) {
        keep.insert(id);
    }
    for p in protect {
        keep.insert(p.clone());
    }

    let mut by_model: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (pid, policy) in &policies {
        by_model
            .entry(policy.model.clone())
            .or_default()
            .push(pid.clone());
    }
    for ids in by_model.values() {
        if ids.iter().any(|pid| keep.contains(pid)) {
            continue;
        }
        let mut newest: Option<String> = None;
        let mut newest_ts: f64 = f64::MIN;
        for pid in ids {
            let ts = policies.get(pid).and_then(|p| p.first_seen).unwrap_or(0.0);
            if ts >= newest_ts {
                newest_ts = ts;
                newest = Some(pid.clone());
            }
        }
        if let Some(n) = newest {
            keep.insert(n);
        }
    }

    let path = policies_path(usage_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };

    let mut removed: Vec<String> = Vec::new();
    let mut seen_removed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(id) = doc.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !keep.contains(id) && !seen_removed.contains(id) {
            removed.push(id.to_string());
            seen_removed.insert(id.to_string());
        }
    }

    if removed.is_empty() {
        return Vec::new();
    }

    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(id) = doc.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if keep.contains(id) {
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    let tmp = {
        let mut p = path.clone();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        p.set_file_name(format!("{}.tmp", name));
        p
    };
    if std::fs::write(&tmp, &out).is_err() {
        return Vec::new();
    }
    let _ = std::fs::rename(&tmp, &path);
    removed
}
