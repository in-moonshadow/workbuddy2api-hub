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
