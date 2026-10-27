//! Proxy slots 能力层（Python `wb_proxy.py` / `wb_settings.py` 的 proxy slots 面）。
//!
//! ## 权威源
//!
//! - 视图 / 探测：`wb_proxy.proxy_slots_view`（wb_proxy.py:5984）、
//!   `wb_proxy.probe_proxy_intel`（:5932）；路由语义见
//!   `wb_proxy._handle_proxy_slots`（:16725-16803）。
//! - 存储：`wb_settings._clean_slot_entry`（:1422）、`set_proxy_slots`（:1492）、
//!   `update_proxy_slot`（:1533）、`find_proxy_slot`（:1588）、
//!   `wb_settings.proxy_slots`（:1475，读 + 清洗 + legacy id）。
//!
//! 存储对齐 Python：读/写 `accounts/settings.json` 的 `proxy_slots` 数组，外加
//! 自增计数器 `proxy_slot_seq`。写路径用 [`wb_storage::replace_settings`]
//! （= Python `wb_settings.save()` 的整体替换口径，原子 temp+rename 落盘）。
//!
//! ## 分层边界
//!
//! 本模块是**能力层**：只读写 settings.json、做纯数据清洗与归一化，不碰
//! POOL / 网关 / 路由。
//!
//! - `bound`（每个 slot 绑定的启用账号数）需要 POOL，由接线层在视图返回体上补
//!   （Python 里是 `proxy_slots_view` 用 `POOL.accounts` 数出来的）。
//! - `drop_missing_bindings(POOL, saved)`（wb_settings.py:1567）同样归接线层。
//! - [`probe_proxy`] 是 Python `probe_proxy_intel` 的**简化版**：真正的网络探测
//!   注入在 `probe_fn` 里，本函数只做十键回复的归一化。生产侧的「经代理
//!   GET https://api.ipify.org」简化探测由接线层提供 `probe_fn` 实现。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{json, Value};
use wb_storage::{read_settings, replace_settings};

/// settings.json 里 proxy slots 的数组键（Python `wb_settings.py` 同名键）。
const SLOTS_KEY: &str = "proxy_slots";
/// settings.json 里 slot id 自增计数器的键（Python `proxy_slot_seq`）。
const SLOT_SEQ_KEY: &str = "proxy_slot_seq";

/// 写路径互斥（对齐 Python `wb_settings._lock`，wb_settings.py:109）。
///
/// `set_proxy_slots` / `update_proxy_slot` 都是「读 settings → 改 → 整份写回」，
/// 不加锁会让并发保存互相覆盖（丢失更新）。Python 靠 RLock 串行化，Rust 侧
/// 用同一个全局锁对齐。读路径（视图 / 查找）不加锁——落盘是原子 rename，
/// 读到的是某一完整版本。
static SLOT_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// 视图（Python proxy_slots_view，wb_proxy.py:5984-5998）
// ---------------------------------------------------------------------------

/// 视图：从 settings.json 读 slots 数组并归一化（Python `proxy_slots_view()`）。
///
/// 返回 `{"slots": [...]}`，每条为 [`clean_slot_entry`] 清洗后的条目，并补上
/// 纯由条目派生的 `label`（Python `slot_label`）。**不补 `bound`**——绑定计数
/// 需要 POOL，归接线层。
///
/// 读侧与 Python `wb_settings.proxy_slots()`（:1475-1489）一致：
/// 按 url 去重（保留首次出现）；缺 `id` 的 legacy 条目补 `slot-N` id
/// （只影响本次读到的结果，不落盘）。
pub fn proxy_slots_view(accounts_dir: &Path) -> Value {
    let data = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    let items: Vec<Value> = read_cleaned_slots(&data)
        .into_iter()
        .map(|mut item| {
            let label = slot_label(&item);
            item["label"] = json!(label);
            item
        })
        .collect();
    json!({ "slots": items })
}

// ---------------------------------------------------------------------------
// 单条 slot 清洗（Python _clean_slot_entry，wb_settings.py:1422-1454）
// ---------------------------------------------------------------------------

/// 规范化单条 slot；不可用（非对象 / url 为空）时返回 `None`。
///
/// 字段口径逐字对齐 Python `_clean_slot_entry`：
///
/// - `url` **必填**（清洗后非空，否则整条丢弃）；
/// - `id` 缺省留空——「补 id」在 [`set_proxy_slots`] / 读侧视图里按各自口径做
///   （Python 同样不在 `_clean_slot_entry` 里补 id）；
/// - `name` 留空 = 「按出口自动命名」，这里**不**偷偷换成 id
///   （Python :1425-1428 的注释：否则操作员分不出自动命名与手动改名）；
/// - `enabled` 缺省开，只有布尔 `false` 算关（Python `... is not False`）；
/// - `country_code` 大写、`ip_type` 小写；
/// - `probed_at` 按 `int(... or 0)` 宽松解析（字符串数字合法，非法回 0）。
pub fn clean_slot_entry(item: &Value) -> Option<Value> {
    let entry = item.as_object()?;
    let url = clean_str(entry.get("url"));
    if url.is_empty() {
        return None;
    }
    let enabled = entry
        .get("enabled")
        .map(|v| v.as_bool() != Some(false))
        .unwrap_or(true);
    Some(json!({
        "id": clean_str(entry.get("id")),
        "name": clean_str(entry.get("name")),
        "url": url,
        "enabled": enabled,
        "ip": clean_str(entry.get("ip")),
        "country": clean_str(entry.get("country")),
        "country_code": clean_str(entry.get("country_code")).to_uppercase(),
        "ip_type": clean_str(entry.get("ip_type")).to_lowercase(),
        "isp": clean_str(entry.get("isp")),
        "asn": clean_str(entry.get("asn")),
        "probed_at": probed_at_of(entry.get("probed_at")),
    }))
}

// ---------------------------------------------------------------------------
// 保存（Python set_proxy_slots，wb_settings.py:1492-1530）
// ---------------------------------------------------------------------------

/// 整体替换 settings.json 的 `proxy_slots` 数组（Python `set_proxy_slots`），
/// 原子写盘。返回 `Ok(())`；写盘失败返回 `Err(String)`。
///
/// id 口径与 Python 一致：
///
/// - `proxy_slot_seq` 计数器从旧列表与入参两边的 `slot-N` id 播种（只增不减，
///   Python :1504-1512）——回收一个 freed id 会把已有账号静默重指向新出口；
/// - 按 url 去重（保留首次出现）；
/// - 缺 id 或与已入列条目 id 重复 → 取下一个自增 id（Python :1523-1525）。
///
/// 本函数只落盘。接线层需要「已保存的列表」时（Python 用它做
/// `drop_missing_bindings` / `apply_proxy_slots`），可在此后调
/// [`proxy_slots_view`] 读回（视图返回的就是刚保存的清洗形态）。
pub fn set_proxy_slots(accounts_dir: &Path, slots: &[Value]) -> Result<(), String> {
    let _guard = SLOT_LOCK.lock().map_err(|e| e.to_string())?;
    let data = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    let mut seq = slot_seq(&data);
    let stored: Vec<Value> = match data.get(SLOTS_KEY) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut map = match data {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };

    // 计数器从旧列表 + 入参两边的 slot-N id 播种；还要压过 legacy 无 id 列表
    // 显示出的 slot-1..slot-N（Python :1513-1515）。
    for entry in stored.iter().chain(slots.iter()) {
        if let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .and_then(parse_slot_seq_id)
        {
            seq = seq.max(id);
        }
    }
    seq = seq.max(stored.len() as i64);

    let mut cleaned: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in slots {
        let Some(mut entry) = clean_slot_entry(raw) else {
            continue;
        };
        let url = entry["url"].as_str().unwrap_or("").to_string();
        if !seen.insert(url) {
            continue;
        }
        let entry_id = entry["id"].as_str().unwrap_or("").to_string();
        let id_is_dup = cleaned
            .iter()
            .any(|e| e.get("id").and_then(Value::as_str) == Some(entry_id.as_str()));
        if entry_id.is_empty() || id_is_dup {
            seq += 1;
            entry["id"] = json!(format!("slot-{seq}"));
        }
        cleaned.push(entry);
    }

    map.insert(SLOTS_KEY.to_string(), Value::Array(cleaned));
    map.insert(SLOT_SEQ_KEY.to_string(), json!(seq));
    replace_settings(accounts_dir, &Value::Object(map))
        .map_err(|e| format!("write settings.json: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 单条 slot 合并更新（Python update_proxy_slot，wb_settings.py:1533-1564）
// ---------------------------------------------------------------------------

/// 把 `fields` 合并进指定 id 的 slot 并整份写回；返回合并后的条目。
///
/// `Ok(Some(entry))` = 找到并落盘；`Ok(None)` = 该 id 不存在（不写盘，
/// 对齐 Python 返回 `None` 的语义）；`Err(String)` = 写盘失败。
/// 读写都在 [`SLOT_LOCK`] 下完成（Python :1537-1540 的注释：不锁的话会和
/// 面板保存竞态，把已落盘的列表回退成旧版本）。
///
/// 对齐 Python 的 `defaults` 参数未移植：`_handle_proxy_slots` 的探测回写路径
/// 不传 defaults（wb_proxy.py:16768-16781），需要时再加。
pub fn update_proxy_slot(
    accounts_dir: &Path,
    slot_id: &str,
    fields: &Value,
) -> Result<Option<Value>, String> {
    let _guard = SLOT_LOCK.lock().map_err(|e| e.to_string())?;
    let id = slot_id.trim();
    if id.is_empty() {
        return Ok(None);
    }
    let data = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    let slots = read_cleaned_slots(&data);

    let mut found: Option<Value> = None;
    let mut out: Vec<Value> = Vec::new();
    for entry in slots {
        if found.is_none() && entry.get("id").and_then(Value::as_str) == Some(id) {
            let mut merged = entry.clone();
            if let Some(fields_map) = fields.as_object() {
                for (key, value) in fields_map {
                    merged[key] = value.clone();
                }
            }
            found = Some(merged.clone());
            out.push(merged);
        } else {
            out.push(entry);
        }
    }
    // 未找到 → Ok(None)，不写盘（Python update_proxy_slot 返回 None，:1560）。
    let Some(merged) = found else {
        return Ok(None);
    };

    let mut map = match data {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    map.insert(SLOTS_KEY.to_string(), Value::Array(out));
    replace_settings(accounts_dir, &Value::Object(map))
        .map_err(|e| format!("write settings.json: {e}"))?;
    Ok(Some(merged))
}

// ---------------------------------------------------------------------------
// 查找（Python find_proxy_slot，wb_settings.py:1588-1595）
// ---------------------------------------------------------------------------

/// 按 id 找 slot；不存在 / id 为空时返回 `None`（Python `find_proxy_slot`）。
///
/// 走的是读侧清洗口径（[`read_cleaned_slots`]），所以 legacy 无 id 条目也会
/// 以 `slot-N` 形态参与查找。
pub fn find_proxy_slot(accounts_dir: &Path, slot_id: &str) -> Option<Value> {
    let id = slot_id.trim();
    if id.is_empty() {
        return None;
    }
    let data = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    read_cleaned_slots(&data)
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
}

// ---------------------------------------------------------------------------
// 探测（Python probe_proxy_intel，wb_proxy.py:5932-5960；简化版）
// ---------------------------------------------------------------------------

/// 探测代理出口（Python `probe_proxy_intel`，wb_proxy.py:5932-5960）。
///
/// ⚠️ **本关是简化探测**：不保证与 Python 的「经代理 GET api.ipify.org 探测
/// 出口 IP + 另起一路 ip-api.com 地理查全 + 中文化」两段式完全一致。真正走
/// 网络的步骤全部注入在 `probe_fn` 里（测试传 mock，生产由接线层提供）。
///
/// `probe_fn` 应返回一个含下列**任选键**的对象：
///
/// - `exit_ip`（出口 IP，失败为空串）、`error`（失败原因，成功为空串/缺省）、
///   `latency_ms`（代理探测耗时，Python 里只算 IP 探测那一段）；
/// - `country` / `country_code` / `ip_type` / `isp` / `asn`（出口情报，可为空）。
///
/// 本函数把它归一化/补齐成 Python 的十键回复：`ok`、`exit_ip`、`latency_ms`、
/// `error`、`intel_ok` 与五个 intel 字段。口径与 Python 逐条对齐：
///
/// - `ok` = `error` 为空（Python `not error`，:5950）；
/// - 无 `exit_ip` 时五个 intel 字段全部置空（Python :5942 只在有 IP 时查情报）；
/// - `intel_ok` = 五个 intel 字段是否任一非空（Python :5948）。
pub fn probe_proxy(proxy_url: &str, probe_fn: impl Fn(&str) -> Value) -> Value {
    let raw = probe_fn(proxy_url);
    let error = clean_str(raw.get("error"));
    let exit_ip = clean_str(raw.get("exit_ip"));
    let ok = error.is_empty();
    let latency_ms = match raw.get("latency_ms") {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    // 只有探测到出口 IP 才算有可查的情报（Python :5942）。
    let (country, country_code, ip_type, isp, asn) = if exit_ip.is_empty() {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    } else {
        (
            clean_str(raw.get("country")),
            clean_str(raw.get("country_code")),
            clean_str(raw.get("ip_type")),
            clean_str(raw.get("isp")),
            clean_str(raw.get("asn")),
        )
    };
    let intel_ok = !(country.is_empty()
        && country_code.is_empty()
        && ip_type.is_empty()
        && isp.is_empty()
        && asn.is_empty());
    json!({
        "ok": ok,
        "exit_ip": exit_ip,
        "latency_ms": latency_ms,
        "error": error,
        "intel_ok": intel_ok,
        "country": country,
        "country_code": country_code,
        "ip_type": ip_type,
        "isp": isp,
        "asn": asn,
    })
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

/// 读 settings.json 里的 slots 数组并清洗（Python `wb_settings.proxy_slots()`，
/// :1475-1489）：非数组 → 空；按 url 去重（保留首次出现）；缺 `id` 的 legacy
/// 条目按 `slot-N` 补 id（不落盘，只影响本次读到的结果）。
fn read_cleaned_slots(data: &Value) -> Vec<Value> {
    let Some(stored) = data.get(SLOTS_KEY).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in stored {
        let Some(mut entry) = clean_slot_entry(raw) else {
            continue;
        };
        let url = entry["url"].as_str().unwrap_or("").to_string();
        if !seen.insert(url) {
            continue;
        }
        if entry["id"].as_str().unwrap_or("").is_empty() {
            entry["id"] = json!(next_slot_id(&out));
        }
        out.push(entry);
    }
    out
}

/// Python `slot_label`（wb_proxy.py:5975-5982）+ `wb_ipintel.slot_name`：
/// 操作员名字 → 出口（国家 [+ 类型]）→ id。
fn slot_label(entry: &Value) -> String {
    let name = clean_str(entry.get("name"));
    if !name.is_empty() {
        return name;
    }
    let country = clean_str(entry.get("country"));
    if country.is_empty() {
        return clean_str(entry.get("id"));
    }
    let kind = type_label(&clean_str(entry.get("ip_type")));
    if kind.is_empty() {
        country
    } else {
        format!("{country} {kind}")
    }
}

/// Python `wb_ipintel.type_label`（wb_ipintel.py:38-40）：住宅 / 机房的中文标签。
fn type_label(ip_type: &str) -> &'static str {
    match ip_type.trim().to_lowercase().as_str() {
        "residential" => "住宅",
        "datacenter" => "机房",
        _ => "",
    }
}

/// 匹配 Python `_SLOT_ID_RE = re.compile(r"^slot-(\d+)$")`（wb_settings.py:1419）。
/// 整串必须是 `slot-` + 至少一位 ASCII 数字。
fn parse_slot_seq_id(id: &str) -> Option<i64> {
    let rest = id.strip_prefix("slot-")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse::<i64>().ok()
}

/// Python `_next_slot_id`（wb_settings.py:1457-1464）：在既有条目里找最大的
/// `slot-N`，返回 `slot-(N+1)`。
fn next_slot_id(existing: &[Value]) -> String {
    let highest = existing
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter_map(parse_slot_seq_id)
        .max()
        .unwrap_or(0);
    format!("slot-{}", highest + 1)
}

/// Python `_slot_seq`（wb_settings.py:1467-1472）：读 `proxy_slot_seq` 计数器，
/// 非法值一律按 0。
fn slot_seq(data: &Value) -> i64 {
    match data.get(SLOT_SEQ_KEY) {
        None => 0,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(b)) => {
            if *b {
                1
            } else {
                0
            }
        }
        Some(_) => 0,
    }
}

/// Python `int(entry.get("probed_at") or 0)`（wb_settings.py:1438-1441）：
/// falsy → 0；字符串数字合法（`int("6")` == 6）；非法 → 0。
fn probed_at_of(v: Option<&Value>) -> i64 {
    match v {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(b)) => {
            if *b {
                1
            } else {
                0
            }
        }
        Some(Value::Array(_)) | Some(Value::Object(_)) => 0,
    }
}

/// Python `str(value or "").strip()` 的近似：falsy（缺键 / null / "" / 0 /
/// false）→ 空串；字符串 → 去首尾空白；`true` → `"True"`（Python `str(True)`）；
/// 其余数字 → 字符串化。数组 / 对象一律空串——Python 的 `str([...])` 能出
/// `"[...]"`，但 slot 字段不会存容器，此处不模拟。
fn clean_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Bool(b)) => {
            if *b {
                "True".to_string()
            } else {
                String::new()
            }
        }
        Some(Value::Number(n)) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Some(Value::Array(_)) | Some(Value::Object(_)) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 单测（probe 全部走 mock，不发任何真实请求）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn clean_slot_entry_normalizes_fields() {
        let item = json!({
            "id": " slot-1 ",
            "name": " 出口A ",
            "url": " http://127.0.0.1:7890 ",
            "enabled": false,
            "ip": "1.2.3.4",
            "country": " 美国 ",
            "country_code": "us",
            "ip_type": "DATACENTER",
            "isp": "ACME",
            "asn": " AS123 ",
            "probed_at": "1234",
        });
        let out = clean_slot_entry(&item).unwrap();
        assert_eq!(out["id"], json!("slot-1"));
        assert_eq!(out["name"], json!("出口A"));
        assert_eq!(out["url"], json!("http://127.0.0.1:7890"));
        assert_eq!(out["enabled"], json!(false));
        assert_eq!(out["country"], json!("美国"));
        assert_eq!(out["country_code"], json!("US"), "country_code 大写");
        assert_eq!(out["ip_type"], json!("datacenter"), "ip_type 小写");
        assert_eq!(out["isp"], json!("ACME"));
        assert_eq!(out["asn"], json!("AS123"));
        assert_eq!(out["probed_at"], json!(1234), "字符串数字按 int() 解析");
    }

    #[test]
    fn clean_slot_entry_defaults_and_rejects() {
        // 缺 enabled → 开；缺 name / ip / intel → 空串；probed_at 非法 → 0。
        let out = clean_slot_entry(&json!({"url": "http://a:1"})).unwrap();
        assert_eq!(out["enabled"], json!(true));
        assert_eq!(out["name"], json!(""));
        assert_eq!(out["ip"], json!(""));
        assert_eq!(out["probed_at"], json!(0));
        assert_eq!(out["country_code"], json!(""));
        assert_eq!(out["ip_type"], json!(""));

        // 只有布尔 false 算关（Python `... is not False`）。
        for v in [json!(null), json!(0), json!("x"), json!(true)] {
            assert_eq!(
                clean_slot_entry(&json!({"url": "http://a:1", "enabled": v})).unwrap()["enabled"],
                json!(true)
            );
        }

        // url 缺失/空 → 整条丢弃；非对象 → 丢弃。
        assert!(clean_slot_entry(&json!({"url": "  "})).is_none());
        assert!(clean_slot_entry(&json!({})).is_none());
        assert!(clean_slot_entry(&json!("x")).is_none());
        assert!(clean_slot_entry(&json!([1, 2])).is_none());
    }

    #[test]
    fn clean_slot_entry_probed_at_loose_parse() {
        assert_eq!(
            clean_slot_entry(&json!({"url": "http://a:1", "probed_at": 5})).unwrap()["probed_at"],
            json!(5)
        );
        assert_eq!(
            clean_slot_entry(&json!({"url": "http://a:1", "probed_at": 1.9})).unwrap()["probed_at"],
            json!(1),
            "int(1.9) == 1"
        );
        assert_eq!(
            clean_slot_entry(&json!({"url": "http://a:1", "probed_at": true})).unwrap()
                ["probed_at"],
            json!(1),
            "int(True) == 1"
        );
        assert_eq!(
            clean_slot_entry(&json!({"url": "http://a:1", "probed_at": "abc"})).unwrap()
                ["probed_at"],
            json!(0)
        );
        assert_eq!(
            clean_slot_entry(&json!({"url": "http://a:1", "probed_at": false})).unwrap()
                ["probed_at"],
            json!(0)
        );
    }

    #[test]
    fn set_and_view_roundtrip_in_settings_json() {
        let dir = tempdir().unwrap();
        let slots = vec![
            json!({"id": "slot-7", "name": "A", "url": "http://a:1"}),
            json!({"url": "http://b:2"}),
            json!({"url": "http://a:1"}), // 重复 url → 丢弃
        ];
        set_proxy_slots(dir.path(), &slots).unwrap();

        let view = proxy_slots_view(dir.path());
        let arr = view["slots"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], json!("slot-7"), "已存在的 id 保留");
        assert_eq!(arr[1]["id"], json!("slot-8"), "seq 从 7 播种 → 空 id 取 8");
        assert_eq!(arr[0]["label"], json!("A"), "name 优先做 label");
        assert_eq!(arr[1]["label"], json!("slot-8"), "无 name/出口 → 回退 id");

        // 落盘形态：settings.json 里有 proxy_slots 数组 + proxy_slot_seq。
        let data = read_settings(dir.path()).unwrap();
        assert_eq!(data["proxy_slots"].as_array().unwrap().len(), 2);
        assert!(data["proxy_slot_seq"].as_i64().is_some());
    }

    #[test]
    fn set_preserves_unrelated_settings_keys() {
        let dir = tempdir().unwrap();
        replace_settings(
            dir.path(),
            &json!({"pricing_enabled": false, "other": "keep"}),
        )
        .unwrap();
        set_proxy_slots(dir.path(), &[json!({"url": "http://a:1"})]).unwrap();
        let data = read_settings(dir.path()).unwrap();
        assert_eq!(
            data["pricing_enabled"],
            json!(false),
            "浅合并之外的键原样保留"
        );
        assert_eq!(data["other"], json!("keep"));
        assert_eq!(data["proxy_slots"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn set_assigns_fresh_ids_to_duplicate_or_empty_ids() {
        let dir = tempdir().unwrap();
        let slots = vec![
            json!({"id": "slot-5", "url": "http://a:1"}),
            json!({"id": "slot-5", "url": "http://b:2"}), // 重复 id → 重新分配
            json!({"id": "", "url": "http://c:3"}),       // 空 id → 分配
        ];
        set_proxy_slots(dir.path(), &slots).unwrap();
        let arr = proxy_slots_view(dir.path())["slots"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"], json!("slot-5"));
        assert_eq!(arr[1]["id"], json!("slot-6"));
        assert_eq!(arr[2]["id"], json!("slot-7"));
    }

    #[test]
    fn view_assigns_legacy_ids_for_entries_without_id() {
        let dir = tempdir().unwrap();
        replace_settings(
            dir.path(),
            &json!({"proxy_slots": [
                {"url": "http://a:1"},
                {"url": "http://b:2"},
            ]}),
        )
        .unwrap();
        let arr = proxy_slots_view(dir.path())["slots"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], json!("slot-1"));
        assert_eq!(arr[1]["id"], json!("slot-2"));
    }

    #[test]
    fn view_dedups_by_url_keeping_first() {
        let dir = tempdir().unwrap();
        replace_settings(
            dir.path(),
            &json!({"proxy_slots": [
                {"id": "slot-1", "url": "http://a:1"},
                {"id": "slot-2", "url": "http://a:1"}, // 重复 url → 丢弃
            ]}),
        )
        .unwrap();
        let arr = proxy_slots_view(dir.path())["slots"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], json!("slot-1"));
    }

    #[test]
    fn find_proxy_slot_by_id() {
        let dir = tempdir().unwrap();
        set_proxy_slots(
            dir.path(),
            &[json!({"id": "slot-3", "name": "A", "url": "http://a:1"})],
        )
        .unwrap();
        assert_eq!(
            find_proxy_slot(dir.path(), "slot-3").unwrap()["name"],
            json!("A")
        );
        assert_eq!(
            find_proxy_slot(dir.path(), " slot-3 ").unwrap()["name"],
            json!("A"),
            "id 去空白"
        );
        assert!(find_proxy_slot(dir.path(), "slot-9").is_none());
        assert!(find_proxy_slot(dir.path(), "  ").is_none());
    }

    #[test]
    fn update_proxy_slot_merges_fields_and_persists() {
        let dir = tempdir().unwrap();
        set_proxy_slots(dir.path(), &[json!({"id": "slot-1", "url": "http://a:1"})]).unwrap();

        let merged =
            update_proxy_slot(dir.path(), "slot-1", &json!({"ip": "1.2.3.4", "name": "x"}))
                .unwrap()
                .unwrap();
        assert_eq!(merged["ip"], json!("1.2.3.4"));
        assert_eq!(merged["name"], json!("x"));

        // 落盘确认。
        let view = proxy_slots_view(dir.path());
        assert_eq!(view["slots"][0]["ip"], json!("1.2.3.4"));

        // 不存在 → Ok(None)，且不写盘。
        assert!(
            update_proxy_slot(dir.path(), "slot-99", &json!({"ip": "x"}))
                .unwrap()
                .is_none()
        );
        let view = proxy_slots_view(dir.path());
        assert_eq!(view["slots"][0]["ip"], json!("1.2.3.4"));
    }

    #[test]
    fn probe_proxy_ok_with_intel() {
        let probe = probe_proxy("http://p:1", |url| {
            assert_eq!(url, "http://p:1", "probe_fn 收到的是原始代理 URL");
            json!({
                "ok": true, // 本字段被忽略：ok 只由 error 推导（Python not error）
                "exit_ip": "1.2.3.4",
                "latency_ms": 42,
                "country": "美国",
                "country_code": "US",
                "ip_type": "residential",
                "isp": "ACME",
                "asn": "AS123",
            })
        });
        assert_eq!(probe["ok"], json!(true));
        assert_eq!(probe["exit_ip"], json!("1.2.3.4"));
        assert_eq!(probe["latency_ms"], json!(42));
        assert_eq!(probe["error"], json!(""));
        assert_eq!(probe["intel_ok"], json!(true));
        assert_eq!(probe["country"], json!("美国"));
        assert_eq!(probe["country_code"], json!("US"));
        assert_eq!(probe["ip_type"], json!("residential"));
        assert_eq!(probe["isp"], json!("ACME"));
        assert_eq!(probe["asn"], json!("AS123"));
    }

    #[test]
    fn probe_proxy_error_no_intel() {
        let probe = probe_proxy("http://p:1", |_| {
            json!({
                "exit_ip": "",
                "error": "connect refused",
                "latency_ms": 120,
                "country": "不该出现", // 无 exit_ip → intel 全清（Python :5942）
            })
        });
        assert_eq!(probe["ok"], json!(false));
        assert_eq!(probe["exit_ip"], json!(""));
        assert_eq!(probe["latency_ms"], json!(120));
        assert_eq!(probe["error"], json!("connect refused"));
        assert_eq!(probe["intel_ok"], json!(false));
        assert_eq!(probe["country"], json!(""));
    }

    #[test]
    fn probe_proxy_ok_but_empty_intel() {
        let probe = probe_proxy("http://p:1", |_| json!({"exit_ip": "9.9.9.9"}));
        assert_eq!(probe["ok"], json!(true));
        assert_eq!(probe["exit_ip"], json!("9.9.9.9"));
        assert_eq!(probe["intel_ok"], json!(false), "五个 intel 字段全空");
    }

    #[test]
    fn probe_proxy_intel_ok_requires_any_field_nonempty() {
        let probe = probe_proxy(
            "http://p:1",
            |_| json!({"exit_ip": "9.9.9.9", "country_code": "ZZ"}),
        );
        assert_eq!(probe["intel_ok"], json!(true), "任一 intel 字段非空即真");
    }

    #[test]
    fn slot_label_uses_country_and_kind() {
        assert_eq!(
            slot_label(&json!({"id": "slot-1", "country": "美国", "ip_type": "residential"})),
            "美国 住宅"
        );
        assert_eq!(
            slot_label(&json!({"id": "slot-1", "country": "美国", "ip_type": "DATACENTER"})),
            "美国 机房",
            "type_label 大小写不敏感"
        );
        assert_eq!(
            slot_label(&json!({"id": "slot-1", "country": "美国"})),
            "美国"
        );
        assert_eq!(slot_label(&json!({"id": "slot-1"})), "slot-1");
        assert_eq!(
            slot_label(&json!({"id": "slot-1", "name": " 手动名 "})),
            "手动名",
            "name 优先"
        );
    }
}
