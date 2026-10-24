//! 关卡 4.18（线 B）：刷新类后台任务用到的 settings 键位口径。
//!
//! ## 权威源
//!
//! Python `wb_settings.py`（本文件每条注释都标注了对应的函数名与行号）。
//! Rust 侧的键名、默认值、归一化口径**逐条对齐 Python**，这样同一份
//! `accounts/settings.json` 在 Python 网关与 Rust 网关下读到的是同一个意思。
//!
//! ## 键位一览
//!
//! | 键 | Python 常量/函数 | 默认值 | 归一化 |
//! |---|---|---|---|
//! | `credits_refresh_hours` | `CREDITS_REFRESH_HOURS_KEY`（`wb_settings.py:42`） | `12.0`（`:40`） | 非法 → 默认；负 → 默认；上限 `720`（`:41`） |
//! | `pricing_refresh_minutes` | `PRICING_REFRESH_MINUTES_KEY`（`:30`） | `5`（`:25`） | 非法 → 默认；`<= 0` → `0`（关闭）；旧键 `pricing_refresh_hours` 按 ×60 迁移 |
//! | `pricing_enabled` | `PRICING_ENABLED_KEY`（`:49`） | **开**（缺键 = true） | 只有布尔 `true` 算开 |
//! | `update_check_enabled` | `UPDATE_CHECK_ENABLED_KEY`（`:94`） | **关**（缺键 = false） | 只有布尔 `true` 算开 |
//!
//! ## ⚠️ `pricing_enabled` 的默认值（已核实，勿再猜）
//!
//! Python `wb_settings.pricing_enabled()`（`wb_settings.py:980-989`）：
//!
//! ```python
//! value = load(accounts_dir).get(PRICING_ENABLED_KEY)
//! return True if value is None else value is True
//! ```
//!
//! ⇒ **缺键 = 开**（`True`）。这与 `update_check_enabled` 相反，后者
//! `wb_settings.py:1354-1362` 写的是 `... is True`，**缺键 = 关**。
//! 两个开关的默认值方向不同是 Python 的刻意选择（更新检查会自己向 GitHub
//! 发请求，故默认关；定价开关默认开，老安装行为不变）。
//!
//! ## 归一化细节（与 Python 逐条对齐）
//!
//! - **数字键容忍字符串数字**：Python 用 `float(raw)`（`wb_settings.py:857`），
//!   `float("6")` 合法 ⇒ `"6"` 与 `6` 同义。Rust 侧用同样的宽松解析。
//! - **布尔当数字**：Python `float(True) == 1.0`、`float(False) == 0.0`
//!   ⇒ `credits_refresh_hours: false` 等价于 `0` ⇒ **关闭**刷新。
//! - **非有限值**：Python `float("nan")` 可解析，`min(nan, 720)` 还是 `nan`，
//!   会让「年龄 < TTL」恒为假 ⇒ 每个账号都变成候选（一个静默的病态状态）。
//!   Rust 侧把 `NaN`/`inf` 当非法值回落默认，这是**有意收紧**，见
//!   [`as_f64`] 的注释。
//! - **缺文件/坏 JSON**：Python `load()`（`wb_settings.py:133-161`）对任何
//!   异常都返回空 dict ⇒ 所有键取默认值。Rust 侧 `wb_storage::read_settings`
//!   对缺失/非对象同样返回空对象，行为一致。
//!
//! ## 与 Python 的已知差异（登记在案，不粉饰）
//!
//! 1. **`credits_refresh_hours` 为负**：Python `credits_refresh_hours()`
//!    （`wb_settings.py:860-861`）对负数返回**默认值 12.0**（不是 0）；
//!    本移植照抄 Python（负 → 默认）。注意 Python 的**写**路径
//!    `set_credits_refresh_hours()`（`:865-876`）把负数夹到 `0`，所以
//!    `0`（= 关闭）只能通过写路径达成。
//! 2. **旧键 `pricing_refresh_hours` 的迁移只读不写**：Python
//!    `_migrate_pricing_refresh()`（`:885-909`）读旧键时会把换算结果**写回**
//!    文件并删掉旧键。Rust 侧只做读取换算、不写回：`wb_storage::write_settings`
//!    是顶层浅合并，无法删键（写 `null` 只是置空，不是删）。可观察差异：
//!    Python 迁移后 `settings.json` 里只剩新键，Rust 侧两个键可能并存
//!    （新键优先，行为一致）。
//! 3. **`pricing_refresh_minutes` 用整分钟**：Python 存浮点分钟（默认 `5.0`），
//!    Rust 侧字段是 `i64`（本关卡的接口约定），按截断取整；`0 < v < 1` 的
//!    极小值截断成 `0`（= 关闭），Python 则会当成一个正的刷新间隔。
//!
//! ## 写路径：两套口径，别混用
//!
//! | | `wb_storage::write_settings` | `wb_storage::replace_settings` |
//! |---|---|---|
//! | 口径 | 顶层**浅合并** | **整体替换**（Python `wb_settings.save()`） |
//! | 坏 JSON | 报错、拒绝写 | 宽容读（当空对象）⇒ 用新数据覆盖掉损坏文件 |
//! | 删键 | 做不到 | 可以（读出来 → 删 → 整份写回） |
//!
//! 面板设置这条线（`crate::settings_view`）走的是 **`replace_settings`**：Python
//! 的 `wb_settings.save()` 就是「读出来 → 改 → 整份写回」，而且它对**损坏的
//! JSON 静默当空对象**，于是下一次保存会把损坏的文件整个覆盖掉。Rust 侧保留
//! `write_settings` 的浅合并 + 拒绝写坏文件，是给「只想改一两个键、又不想在
//! 文件损坏时丢数据」的场景用的（例如 `update_checker` 写 `update_check`）。

use std::path::Path;

use serde_json::{json, Value};

use wb_storage::read_settings;

// ---------------------------------------------------------------------------
// 常量表（`wb_settings.py:25-49`）
// ---------------------------------------------------------------------------

/// 积分刷新的默认 TTL（小时）。Python `DEFAULT_CREDITS_REFRESH_HOURS`（`:40`）。
///
/// 12 小时是刻意宽松的：偏好是按天衡量的，半天的漂移在 7 天窗口里最多让
/// 一个账号挪动半天，而更宽的 TTL 能省下不少上游计费调用。
pub const DEFAULT_CREDITS_TTL_HOURS: f64 = 12.0;

/// 积分刷新 TTL 的上限（小时）。Python `MAX_CREDITS_REFRESH_HOURS`（`:41`）= `24 * 30`。
pub const MAX_CREDITS_TTL_HOURS: f64 = 720.0;

/// 定价刷新的默认间隔（分钟）。Python `DEFAULT_PRICING_REFRESH_MINUTES`（`:25`）= `5.0`。
pub const DEFAULT_PRICING_REFRESH_MINUTES: i64 = 5;

/// 定价刷新间隔的上限（分钟）。Python `MAX_PRICING_REFRESH_MINUTES`（`:32`）= `24 * 30 * 60`。
pub const MAX_PRICING_REFRESH_MINUTES: i64 = 24 * 30 * 60;

/// 积分刷新 TTL 的键名。Python `CREDITS_REFRESH_HOURS_KEY`（`:42`）。
pub const CREDITS_REFRESH_HOURS_KEY: &str = "credits_refresh_hours";

/// 定价刷新间隔的键名。Python `PRICING_REFRESH_MINUTES_KEY`（`:30`）。
pub const PRICING_REFRESH_MINUTES_KEY: &str = "pricing_refresh_minutes";

/// 定价刷新间隔的旧键名（单位是小时）。Python
/// `LEGACY_PRICING_REFRESH_HOURS_KEY`（`:29`）。
pub const LEGACY_PRICING_REFRESH_HOURS_KEY: &str = "pricing_refresh_hours";

/// 定价功能总开关的键名。Python `PRICING_ENABLED_KEY`（`:49`）。
pub const PRICING_ENABLED_KEY: &str = "pricing_enabled";

/// 定价「变体后缀继承」开关的键名。Python `PRICING_VARIANT_INHERIT_KEY`
/// （`wb_settings.py:45`）。
///
/// 读的是 **accounts 目录的 settings.json**（与 `pricing_enabled` 同一个文件）。
pub const PRICING_VARIANT_INHERIT_KEY: &str = "pricing_variant_inherit";

/// 每日更新检查开关的键名。Python `UPDATE_CHECK_ENABLED_KEY`（`:94`）。
pub const UPDATE_CHECK_ENABLED_KEY: &str = "update_check_enabled";

// ---------------------------------------------------------------------------
// 解析结果
// ---------------------------------------------------------------------------

/// 刷新类后台任务读到的设置。
///
/// 每个字段的默认值与归一化口径都来自 Python `wb_settings.py`（见模块文档
/// 的对照表）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefreshSettings {
    /// 积分快照的 TTL（小时）。`0` = 关闭积分刷新（Python 同义）。
    pub credits_refresh_hours: f64,
    /// 定价刷新的间隔（分钟）。`0` = 关闭定价刷新（Python 同义）。
    pub pricing_refresh_minutes: i64,
    /// 定价功能总开关。**缺键 = 开**（Python `wb_settings.py:988-989`）。
    pub pricing_enabled: bool,

    /// 每日更新检查开关。**缺键 = 关**（Python `wb_settings.py:1362`）。
    pub update_check_enabled: bool,
}

/// 定价「变体后缀继承」开关。**缺键 = 开**（Python
/// `wb_settings.py:955-968` 的 `pricing_variant_inherit()`）。
///
/// 判定是**严格的 `is True`**：有键时只有布尔 `true` 算开，数字 `1` 与
/// 字符串 `"true"` 都算关（与 Python 逐字一致）。
///
/// 本函数是这把键的**唯一一份定义**（键名 + 语义）——`price_source.rs` 走
/// 它读，不再自带第二份 `match`，免得两边漂移。
pub fn pricing_variant_inherit(data: &Value) -> bool {
    match data.get(PRICING_VARIANT_INHERIT_KEY) {
        None => true,
        Some(value) => value.as_bool() == Some(true),
    }
}

impl Default for RefreshSettings {
    /// 全部取 Python 的缺省口径（等价于一份空的 `settings.json`）。
    fn default() -> Self {
        RefreshSettings {
            credits_refresh_hours: DEFAULT_CREDITS_TTL_HOURS,
            pricing_refresh_minutes: DEFAULT_PRICING_REFRESH_MINUTES,
            pricing_enabled: true,
            update_check_enabled: false,
        }
    }
}

impl RefreshSettings {
    /// 积分刷新是否被关闭（TTL ≤ 0）。
    ///
    /// Python `CreditsRefresher.ttl_seconds()`（`wb_accounts.py:2498-2508`）
    /// 读到的 TTL ≤ 0 时，`refresh_once()` 直接返回 `None`（一个都不刷）。
    pub fn credits_refresh_disabled(&self) -> bool {
        self.credits_refresh_hours <= 0.0
    }

    /// 定价刷新是否被关闭（间隔 ≤ 0）。
    pub fn pricing_refresh_disabled(&self) -> bool {
        self.pricing_refresh_minutes <= 0
    }
}

// ---------------------------------------------------------------------------
// 读取
// ---------------------------------------------------------------------------

/// 从 `accounts/settings.json` 读出刷新类任务用到的设置。
///
/// 读不到（文件缺失 / 不是 JSON 对象 / IO 错误）时**不报错**，全部回落默认值
/// ——与 Python `wb_settings.load()`（`:133-161`）吞掉所有异常、返回空 dict
/// 的行为一致。
pub fn load_refresh_settings(accounts_dir: &Path) -> RefreshSettings {
    let data = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    parse_refresh_settings(&data)
}

/// 从已读入的 settings 对象解析刷新类设置（便于测试与复用）。
pub fn parse_refresh_settings(data: &Value) -> RefreshSettings {
    RefreshSettings {
        credits_refresh_hours: credits_refresh_hours(data),
        pricing_refresh_minutes: pricing_refresh_minutes(data),
        pricing_enabled: pricing_enabled(data),
        update_check_enabled: update_check_enabled(data),
    }
}

/// `credits_refresh_hours`：积分刷新的 TTL（小时）。
///
/// Python `wb_settings.credits_refresh_hours()`（`wb_settings.py:843-862`）：
///
/// ```python
/// try:
///     value = float(raw)
/// except (TypeError, ValueError):
///     return DEFAULT_CREDITS_REFRESH_HOURS
/// if value < 0:
///     return DEFAULT_CREDITS_REFRESH_HOURS
/// return min(value, MAX_CREDITS_REFRESH_HOURS)
/// ```
///
/// ⇒ 缺键 / 非法值 / 负数 ⇒ 默认 `12.0`；超过上限截到 `720`。
pub fn credits_refresh_hours(data: &Value) -> f64 {
    let Some(raw) = data.get(CREDITS_REFRESH_HOURS_KEY) else {
        return DEFAULT_CREDITS_TTL_HOURS;
    };
    let Some(value) = as_f64(raw) else {
        return DEFAULT_CREDITS_TTL_HOURS;
    };
    if value < 0.0 {
        return DEFAULT_CREDITS_TTL_HOURS;
    }
    value.min(MAX_CREDITS_TTL_HOURS)
}

/// `pricing_refresh_minutes`：定价刷新间隔（分钟）。
///
/// Python `wb_settings.pricing_refresh_minutes()`（`wb_settings.py:912-935`）：
///
/// - 键缺失 ⇒ 先看旧键 `pricing_refresh_hours`（`:885-909` 的迁移逻辑）：
///   旧键存在且是数字 ⇒ `hours * 60` 并夹到 `[0, 43200]`；旧键不存在或不是
///   数字 ⇒ 默认 `5`。
/// - 键存在 ⇒ `float(raw)`；不是数字 ⇒ 默认 `5`；`<= 0` ⇒ `0`（关闭刷新）。
pub fn pricing_refresh_minutes(data: &Value) -> i64 {
    match data.get(PRICING_REFRESH_MINUTES_KEY) {
        Some(raw) => match as_f64(raw) {
            // Python: `value if value > 0 else 0.0`
            Some(value) if value > 0.0 => value.trunc() as i64,
            Some(_) => 0,
            None => DEFAULT_PRICING_REFRESH_MINUTES,
        },
        None => {
            // 旧键迁移（只读换算，不写回，见模块文档的差异登记）
            match data.get(LEGACY_PRICING_REFRESH_HOURS_KEY).and_then(as_f64) {
                Some(hours) => {
                    let minutes = (hours * 60.0).clamp(0.0, MAX_PRICING_REFRESH_MINUTES as f64);
                    minutes.trunc() as i64
                }
                None => DEFAULT_PRICING_REFRESH_MINUTES,
            }
        }
    }
}

/// `pricing_enabled`：定价功能总开关。
///
/// Python `wb_settings.pricing_enabled()`（`wb_settings.py:980-989`）：
/// 缺键 ⇒ **开**；有键 ⇒ 只有布尔 `true` 算开（`value is True` 的严格判定：
/// 数字 `1`、字符串 `"true"` 都不算）。
pub fn pricing_enabled(data: &Value) -> bool {
    match data.get(PRICING_ENABLED_KEY) {
        None => true,
        Some(value) => value.as_bool() == Some(true),
    }
}

/// `update_check_enabled`：每日更新检查开关。
///
/// Python `wb_settings.update_check_enabled()`（`wb_settings.py:1354-1362`）：
/// `... is True` ⇒ 缺键 = **关**，只有布尔 `true` 算开。
pub fn update_check_enabled(data: &Value) -> bool {
    match data.get(UPDATE_CHECK_ENABLED_KEY) {
        None => false,
        Some(value) => value.as_bool() == Some(true),
    }
}

// ---------------------------------------------------------------------------
// 写路径的夹取（供设置写入方复用）
// ---------------------------------------------------------------------------

/// 写 `credits_refresh_hours` 前的夹取。
///
/// Python `wb_settings.set_credits_refresh_hours()`（`wb_settings.py:865-876`）：
/// `max(0.0, min(MAX_CREDITS_REFRESH_HOURS, hours))`。
pub fn clamp_credits_refresh_hours(hours: f64) -> f64 {
    if hours.is_nan() {
        return 0.0;
    }
    hours.clamp(0.0, MAX_CREDITS_TTL_HOURS)
}

/// 写 `pricing_refresh_minutes` 前的夹取。
///
/// Python `wb_settings._clamp_refresh_minutes()`（`wb_settings.py:879-882`）：
/// `max(0.0, min(MAX_PRICING_REFRESH_MINUTES, value))`。
pub fn clamp_pricing_refresh_minutes(minutes: f64) -> i64 {
    if minutes.is_nan() {
        return 0;
    }
    minutes
        .clamp(0.0, MAX_PRICING_REFRESH_MINUTES as f64)
        .trunc() as i64
}

/// 写 `pricing_refresh_minutes` 前的夹取（**保留小数**的浮点版本）。
///
/// Python `set_pricing_refresh_minutes()`（`wb_settings.py:938-952`）存的是
/// **浮点**分钟（`2.5` 分钟就是 `2.5`），夹取口径与
/// [`clamp_pricing_refresh_minutes`] 相同，但**不截断成整数**。
///
/// 面板的读视图 / 写回包必须用这个版本：Python 侧这两个出口给的都是浮点
/// （JSON 里是 `5.0` / `2.5`），用整数版会把 `2.5` 变成 `2`。
pub fn clamp_pricing_refresh_minutes_f64(minutes: f64) -> f64 {
    if minutes.is_nan() {
        return 0.0;
    }
    minutes.clamp(0.0, MAX_PRICING_REFRESH_MINUTES as f64)
}

/// `pricing_refresh_minutes` 的**浮点**读出口（保留小数）。
///
/// Python `wb_settings.pricing_refresh_minutes()`（`wb_settings.py:912-935`）
/// 返回的永远是 `float`（默认 `5.0`、迁移值 `hours * 60`、关闭时 `0.0`），
/// JSON 里表现为 `5.0` / `360.0`。
///
/// 整数版 [`pricing_refresh_minutes`] 是给后台刷新循环用的（本关卡的接口约定：
/// 间隔按整分钟计），**不要**拿它填面板的读视图——那会把 `2.5` 分钟显示成 `2`。
pub fn pricing_refresh_minutes_f64(data: &Value) -> f64 {
    match data.get(PRICING_REFRESH_MINUTES_KEY) {
        Some(raw) => match as_f64(raw) {
            // Python: `value if value > 0 else 0.0`
            Some(value) if value > 0.0 => value,
            Some(_) => 0.0,
            None => DEFAULT_PRICING_REFRESH_MINUTES as f64,
        },
        None => {
            // 旧键迁移（只读换算，不写回，见模块文档的差异登记）
            match data.get(LEGACY_PRICING_REFRESH_HOURS_KEY).and_then(as_f64) {
                Some(hours) => clamp_pricing_refresh_minutes_f64(hours * 60.0),
                None => DEFAULT_PRICING_REFRESH_MINUTES as f64,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把一个 JSON 值解析成 Python `float()` 会接受的那种数字。
///
/// Python `float(raw)` 接受：
/// - 数字（int / float）；
/// - 可被 `float()` 解析的字符串（`"6"`、`" 6.0 "`、`"1e3"`、`"inf"`）；
/// - 布尔（`True` → `1.0`，`False` → `0.0`）。
///
/// 拒绝：`None`（`TypeError`）、空串与其它字符串（`ValueError`）、
/// 列表 / 字典。
///
/// **与 Python 的有意差异**：`NaN` / `inf` 在这里被当作**非法值**（返回
/// `None` ⇒ 回落默认）。Python 会让 `nan` 一路流进 TTL 判定，使
/// 「快照年龄 < TTL」恒为假、每个账号都变成候选（一个静默的病态状态）；
/// `inf` 则会被 `min(value, 720)` 夹回 720（无害）。这里把两者一起拒掉。
fn as_f64(value: &Value) -> Option<f64> {
    let parsed = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }?;
    if parsed.is_finite() {
        Some(parsed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let s = parse_refresh_settings(&json!({}));
        assert_eq!(s.credits_refresh_hours, 12.0);
        assert_eq!(s.pricing_refresh_minutes, 5);
        assert!(s.pricing_enabled, "缺键 = 开（wb_settings.py:988-989）");
        assert!(!s.update_check_enabled, "缺键 = 关（wb_settings.py:1362）");
        assert_eq!(s, RefreshSettings::default());
    }

    #[test]
    fn credits_ttl_reads_numbers_and_numeric_strings() {
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": 6})),
            6.0
        );
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": "6"})),
            6.0,
            "Python float(\"6\") 合法"
        );
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": 0})),
            0.0,
            "0 = 关闭刷新"
        );
    }

    #[test]
    fn credits_ttl_falls_back_to_default_on_garbage_and_negative() {
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": "abc"})),
            12.0
        );
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": null})),
            12.0
        );
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": -3})),
            12.0,
            "Python 对负数返回默认值（wb_settings.py:860-861）"
        );
    }

    #[test]
    fn credits_ttl_is_capped_at_720() {
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": 100000})),
            720.0
        );
    }

    #[test]
    fn credits_ttl_bool_is_number_like_python() {
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": false})),
            0.0,
            "float(False) == 0.0 ⇒ 关闭"
        );
        assert_eq!(
            credits_refresh_hours(&json!({"credits_refresh_hours": true})),
            1.0
        );
    }

    #[test]
    fn pricing_minutes_rules() {
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_minutes": 10})),
            10
        );
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_minutes": "10"})),
            10
        );
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_minutes": 0})),
            0,
            "0 = 关闭"
        );
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_minutes": -5})),
            0,
            "负数也是 0（Python: value if value > 0 else 0.0）"
        );
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_minutes": "x"})),
            5,
            "非法 → 默认"
        );
    }

    #[test]
    fn pricing_minutes_legacy_hours_are_migrated_read_only() {
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_hours": 6})),
            360,
            "旧键按 ×60 换算（wb_settings.py:894-899）"
        );
        assert_eq!(
            pricing_refresh_minutes(&json!({"pricing_refresh_hours": "x"})),
            5,
            "旧键非法 → 默认"
        );
        assert_eq!(
            pricing_refresh_minutes(
                &json!({"pricing_refresh_hours": 6, "pricing_refresh_minutes": 3})
            ),
            3,
            "新键优先"
        );
    }

    #[test]
    fn pricing_enabled_only_accepts_boolean_true() {
        assert!(pricing_enabled(&json!({})));
        assert!(pricing_enabled(&json!({"pricing_enabled": true})));
        assert!(!pricing_enabled(&json!({"pricing_enabled": false})));
        assert!(
            !pricing_enabled(&json!({"pricing_enabled": 1})),
            "Python `value is True` 的严格判定"
        );
        assert!(!pricing_enabled(&json!({"pricing_enabled": "true"})));
    }

    #[test]
    fn update_check_only_accepts_boolean_true() {
        assert!(!update_check_enabled(&json!({})));
        assert!(update_check_enabled(&json!({"update_check_enabled": true})));
        assert!(!update_check_enabled(
            &json!({"update_check_enabled": false})
        ));
        assert!(!update_check_enabled(&json!({"update_check_enabled": 1})));
    }

    #[test]
    fn clamps_match_python_setters() {
        assert_eq!(clamp_credits_refresh_hours(-1.0), 0.0);
        assert_eq!(clamp_credits_refresh_hours(1000.0), 720.0);
        assert_eq!(clamp_credits_refresh_hours(3.0), 3.0);
        assert_eq!(clamp_pricing_refresh_minutes(-1.0), 0);
        assert_eq!(clamp_pricing_refresh_minutes(1e9), 43_200);
    }
}
