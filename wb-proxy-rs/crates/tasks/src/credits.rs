//! M5 数据侧：积分快照（credits）的形状、解析与聚合。
//!
//! 权威源：Python `wb_accounts.py`
//! - `_parse_package_account`（:1739）——资源包字段归一化
//! - `deduction_end_text`（:296）——抵扣截止时间
//! - `_fetch_credits_cn_detailed`（:1850）/ `_fetch_credits_fallback`（:1975）
//!   ——两条聚合口径
//!
//! HTTP 那一半在 `wb_gateway::credits`；本模块是**纯逻辑**（`wb-tasks` 无 HTTP 依赖）。
//!
//! ## 积分是快照，不是实时值
//!
//! Python 侧靠后台刷新器（约 12h 一轮）落盘 `credits`，面板读的是落盘快照。
//! 抓取失败时**保留旧快照**（不要清零）——清零会把「查不到」显示成「没有积分」。

use serde_json::{json, Map, Value};

/// 资源包列表（兜底口径）。
pub const GET_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
/// 套餐概览（国内版详细口径第一步）。
pub const RESOURCE_SUMMARY_PATH: &str = "/billing/meter/get-user-resource-summary";
/// 免费包列表。
pub const RESOURCE_FREE_PACKAGES_PATH: &str = "/billing/meter/get-user-resource-free-packages";
/// 付费包列表。
pub const RESOURCE_PAID_PACKAGES_PATH: &str = "/billing/meter/get-user-resource-paid-packages";
/// 每日签到状态（国内版详细口径第四步）。
pub const CHECKIN_STATUS_PATH: &str = "/v2/billing/meter/checkin-activity-status";

/// 超过这个天数的「到期」按「无期限」处理（Python `EXPIRY_SENTINEL_DAYS`）。
pub const EXPIRY_SENTINEL_DAYS: i64 = 730;

/// `days_left` 缺省时参与排序用的哨兵值（Python 里的 `99999`）。
const DAYS_SENTINEL: f64 = 99999.0;

// ---------------------------------------------------------------------------
// 数值小工具
// ---------------------------------------------------------------------------

/// Python 的 `round(x, 2)`。
///
/// Python 的 `round` 对**精确二进制值**做正确舍入（平局取偶），
/// 不是「先乘 100 再四舍五入」：`round(2.675, 2)` 得 2.67（2.675 的精确值
/// 略小于 2.675）。Rust 的 `format!("{:.2}", x)` 正是同一口径（正确舍入 +
/// 平局取偶），逐位对齐后 parse 回来即可。
///
/// 不用 `f64::round`：那是「五入」，在 `.xx5` 上与 Python 差 0.01，
/// 逐包累加后总额会漂。
pub fn round2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.2}").parse::<f64>().unwrap_or(x)
}

/// Python 的 `float(v)`：数字、布尔、数字字符串都吃；其余返回 `None`。
fn as_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Python `_val_or_none(*keys)`：按顺序取第一个能转成数字的键。
///
/// 「值为 0」与「字段缺失」必须能区分开——原实现找不到时返回 `0.0`，
/// 会让「周期口径」那句恒真、永不回落累计口径。
fn val_or_none(obj: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let Some(v) = obj.get(*key) else { continue };
        if v.is_null() {
            continue;
        }
        if let Value::String(s) = v {
            if s.is_empty() {
                continue;
            }
        }
        if let Some(f) = as_float(v) {
            return Some(f);
        }
    }
    None
}

/// Python `_val(*keys)`：同上，但缺省 `0.0`。
fn val(obj: &Value, keys: &[&str]) -> f64 {
    val_or_none(obj, keys).unwrap_or(0.0)
}

/// Python `time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(ts))`。
pub fn local_text(ts: f64) -> String {
    match chrono::DateTime::from_timestamp(ts as i64, 0) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => String::new(),
    }
}

/// Python `time.mktime(time.strptime(s, "%Y-%m-%d %H:%M:%S"))`：按**本地**时区解释。
fn parse_local(text: &str) -> Option<f64> {
    let clean = text.replacen('T', " ", 1);
    let clean = if clean.len() > 19 {
        clean[..19].to_string()
    } else {
        clean
    };
    let naive = chrono::NaiveDateTime::parse_from_str(&clean, "%Y-%m-%d %H:%M:%S").ok()?;
    let local = naive.and_local_timezone(chrono::Local).single()?;
    Some(local.timestamp() as f64)
}

// ---------------------------------------------------------------------------
// 资源包
// ---------------------------------------------------------------------------

/// 一个资源包（额度包）的归一化形状。
///
/// 21 个键，顺序逐字对齐 Python `_parse_package_account` 的返回 dict。
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    pub package_code: String,
    pub product_name: String,
    pub sub_product_name: String,
    pub grant_reason: String,
    pub resource_id: String,
    pub deal_name: String,
    pub create_time: String,
    pub remain: f64,
    pub used: f64,
    pub size: f64,
    pub unit: String,
    pub in_usage: bool,
    pub auto_renew: bool,
    pub cycle_start_time: String,
    pub cycle_end_time: String,
    pub expire_time: String,
    pub no_expiry: bool,
    pub days_left: Option<f64>,
    pub is_expired: bool,
    /// 上游原样带过来的 `Status`（类型不固定）。
    pub status: Value,
}

impl Package {
    /// Python dict 的逐字形状（键名与顺序）。
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("name".into(), json!(self.name));
        obj.insert("package_code".into(), json!(self.package_code));
        obj.insert("product_name".into(), json!(self.product_name));
        obj.insert("sub_product_name".into(), json!(self.sub_product_name));
        obj.insert("grant_reason".into(), json!(self.grant_reason));
        obj.insert("resource_id".into(), json!(self.resource_id));
        obj.insert("deal_name".into(), json!(self.deal_name));
        obj.insert("create_time".into(), json!(self.create_time));
        obj.insert("remain".into(), json!(self.remain));
        obj.insert("used".into(), json!(self.used));
        obj.insert("size".into(), json!(self.size));
        obj.insert("unit".into(), json!(self.unit));
        obj.insert("in_usage".into(), json!(self.in_usage));
        obj.insert("auto_renew".into(), json!(self.auto_renew));
        obj.insert("cycle_start_time".into(), json!(self.cycle_start_time));
        obj.insert("cycle_end_time".into(), json!(self.cycle_end_time));
        obj.insert("expire_time".into(), json!(self.expire_time));
        obj.insert("no_expiry".into(), json!(self.no_expiry));
        obj.insert("days_left".into(), json!(self.days_left));
        obj.insert("is_expired".into(), json!(self.is_expired));
        obj.insert("status".into(), self.status.clone());
        Value::Object(obj)
    }
}

/// Python `deduction_end_text(acc)`：包的「抵扣截止时间」。
///
/// 取上游的 `DeductionEndTime`（epoch 毫秒）。上游没给就返回空串，
/// 调用方回退到 `CycleEndTime`——免费包/体验版的 `CycleEndTime` 只是每月
/// 计费周期的边界，两者能差 8 年，到期必须认这个字段。
pub fn deduction_end_text(acc: &Value) -> String {
    let raw = match acc.get("DeductionEndTime") {
        None | Some(Value::Null) => return String::new(),
        Some(v) => v,
    };
    let stamp = match raw {
        Value::String(s) if s.is_empty() => return String::new(),
        Value::Number(n) if n.as_f64() == Some(0.0) => return String::new(),
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(f) => f,
            Err(_) => return String::new(),
        },
        other => match as_float(other) {
            Some(f) => f,
            None => return String::new(),
        },
    };
    if stamp == 0.0 {
        return String::new();
    }
    // epoch 毫秒 → 秒
    let stamp = if stamp > 1e11 { stamp / 1000.0 } else { stamp };
    local_text(stamp)
}

/// 归一化一个资源包（Python `_parse_package_account`）。
///
/// `now_epoch` 是当前时间（用于算 `days_left`）——传进来而不是内部取，
/// 这样这条纯函数可以单测。
pub fn parse_package_account(acc: &Value, now_epoch: f64) -> Package {
    let name = acc
        .get("PackageName")
        .filter(|v| !v.is_null())
        .map(python_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Package".to_string());
    let package_code = acc.get("PackageCode").map(python_str).unwrap_or_default();

    // 容量字段有「累计」与「本周期」两套口径，必须整组同维度取值。
    // 只认周期口径会漏掉「一次性赠额未动」的包；只认累计口径会漏掉
    // 「月度包周期扣满」的包。取两者剩余较大的一口径。
    let cumulative_remain = val_or_none(acc, &["CapacityRemainPrecise", "CapacityRemain"]);
    let cycle_remain = val_or_none(acc, &["CycleCapacityRemainPrecise", "CycleCapacityRemain"]);
    let use_cycle =
        cycle_remain.is_some() && (cumulative_remain.is_none() || cycle_remain > cumulative_remain);

    let (size, remain, used) = if use_cycle {
        (
            val(acc, &["CycleCapacitySizePrecise", "CycleCapacitySize"]),
            val(acc, &["CycleCapacityRemainPrecise", "CycleCapacityRemain"]),
            val(acc, &["CycleCapacityUsedPrecise", "CycleCapacityUsed"]),
        )
    } else {
        (
            val(acc, &["CapacitySizePrecise", "CapacitySize"]),
            val(acc, &["CapacityRemainPrecise", "CapacityRemain"]),
            val(acc, &["CapacityUsedPrecise", "CapacityUsed"]),
        )
    };

    let (size, mut remain, mut used) = (size, remain, used);
    if size > 0.0 && used <= 0.0 && remain <= size {
        used = (size - remain).max(0.0);
    } else if size > 0.0 && remain <= 0.0 && used < size {
        remain = (size - used).max(0.0);
    }

    let grant_reason = acc
        .get("AccountAttributes")
        .and_then(Value::as_array)
        .map(|attrs| {
            attrs
                .iter()
                .find(|a| a.get("Key").and_then(Value::as_str) == Some("grantReason"))
                .map(|a| a.get("Value").map(python_str).unwrap_or_default())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let create_time = match acc.get("CreateTime") {
        Some(v) if !v.is_null() && !matches!(v, Value::String(s) if s.is_empty()) => {
            match as_float(v) {
                Some(ts) => {
                    let ts = if ts > 1e11 { ts / 1000.0 } else { ts };
                    local_text(ts)
                }
                None => python_str(v),
            }
        }
        _ => String::new(),
    };

    let end_time_str = acc
        .get("CycleEndTime")
        .map(python_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            acc.get("ExpiredTime")
                .map(python_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    // 到期认「抵扣截止时间」而不是「周期结束时间」：周期结束只是计费周期的
    // 边界，积分未必跟着作废。
    let expire_time_str = {
        let d = deduction_end_text(acc);
        if d.is_empty() {
            end_time_str.clone()
        } else {
            d
        }
    };

    let mut days_left = None;
    let mut is_expired = false;
    let mut no_expiry = false;
    if !expire_time_str.is_empty() {
        if let Some(end_ts) = parse_local(&expire_time_str) {
            let diff_sec = end_ts - now_epoch;
            if diff_sec > EXPIRY_SENTINEL_DAYS as f64 * 86400.0 {
                no_expiry = true;
            } else {
                days_left = Some(round1(diff_sec / 86400.0));
                is_expired = diff_sec < 0.0;
            }
        }
    }

    let mut in_usage = acc.get("InUsage").map(python_truthy).unwrap_or(false);
    if !in_usage && !is_expired && remain > 0.0 && used > 0.0 {
        in_usage = true;
    }

    let auto_renew = acc.get("AutoRenewFlag").map(python_truthy).unwrap_or(false)
        || acc
            .get("SupportAutoRenew")
            .map(python_truthy)
            .unwrap_or(false);

    Package {
        name,
        package_code,
        product_name: acc.get("ProductName").map(python_str).unwrap_or_default(),
        sub_product_name: acc
            .get("SubProductName")
            .map(python_str)
            .unwrap_or_default(),
        grant_reason,
        resource_id: acc.get("ResourceId").map(python_str).unwrap_or_default(),
        deal_name: acc.get("DealName").map(python_str).unwrap_or_default(),
        create_time,
        remain: round2(remain),
        used: round2(used),
        size: round2(size),
        unit: acc
            .get("CapacityUnit")
            .map(python_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "credits".to_string()),
        in_usage,
        auto_renew,
        cycle_start_time: acc
            .get("CycleStartTime")
            .map(python_str)
            .unwrap_or_default(),
        cycle_end_time: end_time_str,
        expire_time: expire_time_str,
        no_expiry,
        days_left,
        is_expired,
        status: acc.get("Status").cloned().unwrap_or_else(|| Value::from(0)),
    }
}

/// Python `round(x, 1)`（`days_left` 用一位小数）。口径同 [`round2`]。
fn round1(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.1}").parse::<f64>().unwrap_or(x)
}

/// Python 的 `str(v)`（`None` → `"None"`、`True` → `"True"`）。
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python 的真值判断。
fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// 聚合
// ---------------------------------------------------------------------------

/// 积分快照（`credits`）。
///
/// 12 个键，顺序逐字对齐 Python `self.credits = {...}`。
#[derive(Debug, Clone, PartialEq)]
pub struct Credits {
    pub remain: f64,
    pub used: f64,
    pub size: f64,
    pub used_percent: String,
    pub remain_percent: String,
    pub is_paid_user: bool,
    /// 签到状态（`today_checked_in` / `streak_days` / …）；拿不到时为 `None`。
    pub checkin: Option<Value>,
    /// 最早到期的有效包；没有则为 `None`。
    pub earliest_expiring: Option<Value>,
    pub packages: Vec<Package>,
    /// 快照时刻（epoch 秒）。
    pub updated_at: f64,
    /// 快照时刻（本地时间文本）。
    pub updated_iso: String,
}

impl Credits {
    /// Python dict 的逐字形状（键名与顺序）。
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("remain".into(), json!(self.remain));
        obj.insert("used".into(), json!(self.used));
        obj.insert("size".into(), json!(self.size));
        obj.insert("used_percent".into(), json!(self.used_percent));
        obj.insert("remain_percent".into(), json!(self.remain_percent));
        obj.insert("is_paid_user".into(), json!(self.is_paid_user));
        obj.insert("checkin".into(), json!(self.checkin));
        obj.insert(
            "earliest_expiring".into(),
            self.earliest_expiring.clone().unwrap_or(Value::Null),
        );
        obj.insert(
            "packages".into(),
            Value::Array(self.packages.iter().map(Package::to_json).collect()),
        );
        obj.insert("updated_at".into(), json!(self.updated_at));
        obj.insert("updated_iso".into(), json!(self.updated_iso));
        Value::Object(obj)
    }
}

/// 把上游的账户条目归一化成资源包列表。
///
/// 去重已经在 HTTP 那一半（`wb_gateway::credits`）做过了——免费包与付费包会
/// 重叠，去重要按 `AccountId`，不是按这里的形状。
pub fn packages_from_accounts(accounts: &[Value], now_epoch: f64) -> Vec<Package> {
    accounts
        .iter()
        .map(|acc| parse_package_account(acc, now_epoch))
        .collect()
}

/// 排序键：使用中优先 → 未过期中按到期时间升序 → 已过期排最后。
///
/// 已过期的 `days_left` 为负，Python 把负数折成 `99999 + abs(days)`，
/// 即「过期越久排得越靠后」——这里照抄。
fn package_sort_key(p: &Package) -> (i64, i64, f64, f64) {
    let in_use_score = if p.in_usage { 0 } else { 1 };
    let expired_score = if p.is_expired { 1 } else { 0 };
    let mut days = p.days_left.unwrap_or(DAYS_SENTINEL);
    if days < 0.0 {
        days = DAYS_SENTINEL + days.abs();
    }
    (in_use_score, expired_score, days, -p.remain)
}

/// 原地排序（Python 的 `list.sort` 是稳定排序，Rust 的 `sort_by` 也是）。
pub fn sort_packages(packages: &mut [Package]) {
    packages.sort_by(|a, b| {
        package_sort_key(a)
            .partial_cmp(&package_sort_key(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// 最早到期的有效包（有剩余积分、未过期、到期时间已知）。
pub fn earliest_expiring(packages: &[Package]) -> Option<Value> {
    let mut active: Vec<&Package> = packages
        .iter()
        .filter(|p| p.remain > 0.0 && !p.is_expired && p.days_left.is_some())
        .collect();
    active.sort_by(|a, b| {
        a.days_left
            .unwrap_or(0.0)
            .partial_cmp(&b.days_left.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let ep = active.first()?;
    Some(json!({
        "name": ep.name,
        "package_code": ep.package_code,
        "remain": ep.remain,
        "cycle_end_time": ep.cycle_end_time,
        "days_left": ep.days_left,
    }))
}

/// 百分比文案（Python 的 `f"{x:.1f}%"`）。
///
/// 总量为 0 时两边的兜底**不一样**（Python 原文）：已用比例回落 `"0.0%"`，
/// 剩余比例回落 `"100.0%"`——「一个额度都没有」不是「额度用光了」。
fn percent(part: f64, total: f64, fallback: &str) -> String {
    if total > 0.0 {
        format!("{:.1}%", part / total * 100.0)
    } else {
        fallback.to_string()
    }
}

/// 兜底口径的聚合（Python `_fetch_credits_fallback`）。
///
/// `checkin` 取**上一次快照**里的值（Python 读 `getattr(self, "credits", {})`）——
/// 兜底路径不查签到接口，不能把上次的签到状态清掉。
pub fn aggregate_fallback(
    mut packages: Vec<Package>,
    previous_checkin: Option<Value>,
    now_epoch: f64,
) -> Credits {
    let tot_remain = round2(packages.iter().map(|p| p.remain).sum());
    let tot_used = round2(packages.iter().map(|p| p.used).sum());
    let tot_size = round2(packages.iter().map(|p| p.size).sum());
    sort_packages(&mut packages);
    let earliest = earliest_expiring(&packages);
    Credits {
        remain: tot_remain,
        used: tot_used,
        size: tot_size,
        used_percent: percent(tot_used, tot_size, "0.0%"),
        remain_percent: percent(tot_remain, tot_size, "100.0%"),
        is_paid_user: false,
        checkin: previous_checkin,
        earliest_expiring: earliest,
        packages,
        updated_at: now_epoch,
        updated_iso: local_text(now_epoch),
    }
}

/// 国内版详细口径的聚合（Python `_fetch_credits_cn_detailed`）。
///
/// 与兜底口径的差别：
/// - `is_paid_user` 来自套餐概览的 `IsPaidUser`；
/// - `checkin` 来自签到状态接口；
/// - 具体包为空但概览里有汇总容量时，用概览的容量补总量（面板不显示 0/0）。
pub fn aggregate_detailed(
    mut packages: Vec<Package>,
    summary_packages: &[Value],
    is_paid_user: bool,
    checkin: Option<Value>,
    now_epoch: f64,
) -> Credits {
    let mut tot_remain = packages.iter().map(|p| p.remain).sum::<f64>();
    let mut tot_used = packages.iter().map(|p| p.used).sum::<f64>();
    let mut tot_size = packages.iter().map(|p| p.size).sum::<f64>();

    if packages.is_empty() && !summary_packages.is_empty() {
        for sp in summary_packages {
            tot_size += val(sp, &["CycleTotalCapacity"]);
            tot_remain += val(sp, &["CycleRemainCapacity"]);
            tot_used += val(sp, &["CycleUsedCapacity"]);
        }
    }

    let tot_remain = round2(tot_remain);
    let tot_used = round2(tot_used);
    let tot_size = round2(tot_size);
    sort_packages(&mut packages);
    let earliest = earliest_expiring(&packages);
    Credits {
        remain: tot_remain,
        used: tot_used,
        size: tot_size,
        used_percent: percent(tot_used, tot_size, "0.0%"),
        remain_percent: percent(tot_remain, tot_size, "100.0%"),
        is_paid_user,
        checkin,
        earliest_expiring: earliest,
        packages,
        updated_at: now_epoch,
        updated_iso: local_text(now_epoch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-20 08:00:00 本地时间对应的 epoch（测试里用固定的 now 传参）。
    const NOW: f64 = 1_800_000_000.0;

    #[test]
    fn round2_matches_python_bankers_rounding() {
        // Python round(2.675, 2) == 2.67（2.675 实际是 2.67499…）
        assert_eq!(round2(2.675), 2.67);
        // Python round(0.125, 2) == 0.12（五成双）
        assert_eq!(round2(0.125), 0.12);
        assert_eq!(round2(0.135), 0.14);
        assert_eq!(round2(1.005), 1.0);
    }

    #[test]
    fn package_uses_cycle_dimension_when_it_is_larger() {
        let acc = json!({
            "PackageName": "月度包",
            "PackageCode": "m1",
            "CapacitySize": 100,
            "CapacityRemain": 100,
            "CycleCapacitySize": 100,
            "CycleCapacityRemain": 10,
            "CycleCapacityUsed": 90
        });
        let pkg = parse_package_account(&acc, NOW);
        // 周期口径剩余更小 ⇒ 取累计口径
        assert_eq!(pkg.remain, 100.0);
        assert_eq!(pkg.size, 100.0);
        assert_eq!(pkg.used, 0.0);
    }

    #[test]
    fn package_uses_cycle_dimension_when_cumulative_is_missing() {
        let acc = json!({
            "PackageName": "月度包",
            "CycleCapacitySize": 100,
            "CycleCapacityRemain": 40,
            "CycleCapacityUsed": 60
        });
        let pkg = parse_package_account(&acc, NOW);
        assert_eq!(pkg.remain, 40.0);
        assert_eq!(pkg.size, 100.0);
        assert_eq!(pkg.used, 60.0);
    }

    #[test]
    fn package_fills_missing_used_from_size_minus_remain() {
        let acc = json!({
            "PackageName": "赠额",
            "CapacitySize": 100,
            "CapacityRemain": 100
        });
        let pkg = parse_package_account(&acc, NOW);
        // size>0 且 used<=0 且 remain<=size ⇒ used = size - remain = 0
        assert_eq!(pkg.used, 0.0);
        assert_eq!(pkg.remain, 100.0);
    }

    #[test]
    fn package_reads_grant_reason_and_unit_defaults() {
        let acc = json!({
            "PackageName": "Bonus Pack",
            "AccountAttributes": [
                {"Key": "other", "Value": "x"},
                {"Key": "grantReason", "Value": "官方活动发放"}
            ],
            "CapacityUnit": ""
        });
        let pkg = parse_package_account(&acc, NOW);
        assert_eq!(pkg.grant_reason, "官方活动发放");
        assert_eq!(pkg.unit, "credits");
        assert_eq!(pkg.name, "Bonus Pack");
    }

    #[test]
    fn deduction_end_prefers_deduction_field_over_cycle_end() {
        let acc = json!({
            "DeductionEndTime": 1_800_000_000_000i64,
            "CycleEndTime": "2036-01-01 00:00:00"
        });
        let text = deduction_end_text(&acc);
        assert!(!text.is_empty());
        assert_ne!(text, "2036-01-01 00:00:00");
    }

    #[test]
    fn deduction_end_is_empty_when_field_absent_or_zero() {
        assert_eq!(deduction_end_text(&json!({})), "");
        assert_eq!(deduction_end_text(&json!({"DeductionEndTime": 0})), "");
        assert_eq!(deduction_end_text(&json!({"DeductionEndTime": ""})), "");
    }

    #[test]
    fn expiry_beyond_sentinel_is_no_expiry() {
        let acc = json!({
            "PackageName": "长期包",
            "CapacitySize": 10,
            "CapacityRemain": 10,
            "DeductionEndTime": (NOW + 800.0 * 86400.0) * 1000.0
        });
        let pkg = parse_package_account(&acc, NOW);
        assert!(pkg.no_expiry);
        assert!(pkg.days_left.is_none());
        assert!(!pkg.is_expired);
    }

    #[test]
    fn expired_package_is_flagged() {
        let acc = json!({
            "PackageName": "过期包",
            "CapacitySize": 10,
            "CapacityRemain": 0,
            "DeductionEndTime": (NOW - 3.0 * 86400.0) * 1000.0
        });
        let pkg = parse_package_account(&acc, NOW);
        assert!(pkg.is_expired);
        assert!(pkg.days_left.is_some());
    }

    #[test]
    fn sort_puts_in_use_first_then_earliest_expiry() {
        let mk =
            |name: &str, remain: f64, expired: bool, in_use: bool, days: Option<f64>| Package {
                name: name.into(),
                package_code: String::new(),
                product_name: String::new(),
                sub_product_name: String::new(),
                grant_reason: String::new(),
                resource_id: String::new(),
                deal_name: String::new(),
                create_time: String::new(),
                remain,
                used: 0.0,
                size: remain,
                unit: "credits".into(),
                in_usage: in_use,
                auto_renew: false,
                cycle_start_time: String::new(),
                cycle_end_time: String::new(),
                expire_time: String::new(),
                no_expiry: false,
                days_left: days,
                is_expired: expired,
                status: Value::from(0),
            };
        let mut pkgs = vec![
            mk("过期包", 0.0, true, false, Some(-1.0)),
            mk("闲置包", 50.0, false, false, Some(10.0)),
            mk("使用中", 20.0, false, true, Some(30.0)),
        ];
        sort_packages(&mut pkgs);
        assert_eq!(pkgs[0].name, "使用中");
        assert_eq!(pkgs[1].name, "闲置包");
        assert_eq!(pkgs[2].name, "过期包");
    }

    #[test]
    fn aggregate_computes_percentages_and_earliest_expiry() {
        let acc = json!({
            "PackageName": "A",
            "CapacitySize": 100,
            "CapacityRemain": 30,
            "DeductionEndTime": (NOW + 5.0 * 86400.0) * 1000.0
        });
        let pkg = parse_package_account(&acc, NOW);
        let credits = aggregate_detailed(vec![pkg], &[], true, None, NOW);
        assert_eq!(credits.remain, 30.0);
        assert_eq!(credits.size, 100.0);
        // used 由 size - remain 推出（30 剩余 ⇒ 已用 70）
        assert_eq!(credits.used, 70.0);
        assert_eq!(credits.used_percent, "70.0%");
        assert_eq!(credits.remain_percent, "30.0%");
        assert!(credits.is_paid_user);
        assert!(credits.earliest_expiring.is_some());
    }

    #[test]
    fn detailed_aggregate_falls_back_to_summary_capacity() {
        let credits = aggregate_detailed(
            vec![],
            &[json!({
                "CycleTotalCapacity": 100,
                "CycleRemainCapacity": 60,
                "CycleUsedCapacity": 40
            })],
            false,
            None,
            NOW,
        );
        assert_eq!(credits.size, 100.0);
        assert_eq!(credits.remain, 60.0);
        assert_eq!(credits.used, 40.0);
    }

    #[test]
    fn fallback_aggregate_keeps_previous_checkin() {
        let credits = aggregate_fallback(
            vec![],
            Some(json!({"today_checked_in": true, "streak_days": 3})),
            NOW,
        );
        assert_eq!(credits.used_percent, "0.0%");
        assert_eq!(credits.remain_percent, "100.0%");
        assert!(!credits.is_paid_user);
        assert_eq!(
            credits
                .checkin
                .and_then(|c| c.get("streak_days").and_then(Value::as_i64)),
            Some(3)
        );
    }

    #[test]
    fn credits_json_has_twelve_keys_in_python_order() {
        let credits = aggregate_fallback(vec![], None, NOW);
        let obj = credits.to_json();
        let keys: Vec<&str> = obj
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "remain",
                "used",
                "size",
                "used_percent",
                "remain_percent",
                "is_paid_user",
                "checkin",
                "earliest_expiring",
                "packages",
                "updated_at",
                "updated_iso",
            ]
        );
    }

    #[test]
    fn package_json_has_twenty_one_keys_in_python_order() {
        let pkg = parse_package_account(&json!({"PackageName": "x"}), NOW);
        let obj = pkg.to_json();
        let keys: Vec<&str> = obj
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 21);
        assert_eq!(keys[0], "name");
        assert_eq!(keys[20], "status");
    }
}
