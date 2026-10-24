//! 面板设置的**读视图**与**保存**（Python `wb_proxy.runtime_settings_view()` 与
//! `wb_proxy._handle_settings_save()` 的等价物）。
//!
//! ## 权威源
//!
//! - `wb_proxy.py:9661-9730` —— `runtime_settings_view()`：GET /settings 的 33 个顶层键。
//! - `wb_proxy.py:16401-16725` —— `_handle_settings_save()`：POST /settings/save 的全部分支。
//! - `wb_settings.py` —— 各个 getter/setter 的默认值与归一化口径。
//!
//! ## 与 Python 的三处**有意**差异（其余逐字对齐）
//!
//! 1. **不支持的键 ⇒ 400**（Python 是静默忽略）。Python 的保存处理器只认它
//!    认识的键，其余键原样丢掉；面板前端若拼错键名，用户会以为改了其实没改。
//!    本实现对**顶层**未支持的键一律 400（见 [`SUPPORTED_PATCH_KEYS`]）。
//! 2. **读视图不写盘**（Python 的 `limits_data()` / `pricing_refresh_minutes()`
//!    在读的时候会顺手把旧版扁平键迁移进文件）。Rust 侧的读视图是**纯读**，
//!    迁移只发生在内存里；写盘迁移发生在保存路径上。
//! 3. **API key 只能是标量**（Python 用 `str()` 把任意对象转成字符串存进去）。
//!    本实现对数组/对象报 400。
//!
//! ## 键序（响应字节的一部分）
//!
//! `serde_json` 开了 `preserve_order`，所以这里全部用 `json!` **显式写出键的
//! 顺序**（不用 `derive(Serialize)`）。读视图的键序与 Python
//! `runtime_settings_view()` 的字面量顺序逐个对齐。
//!
//! ## 保存的运行时副作用
//!
//! Python 的保存处理器有 12 处运行时副作用（`PRICING.wake()` / `set_interval` /
//! `run_once` / `CREDITS_REFRESHER.wake()` / `UPDATES.wake()` / `SCHEDULER.stop+start`
//! / 改全局 `API_KEY` …）。**本模块不碰后台线程**：它只负责落盘 + 校验 +
//! 算出生效值，把「本次动作的增量键」放进返回值，由调用方（HTTP 接线层）
//! 按键触发对应副作用。详见 [`save_settings`] 的文档。

use std::path::Path;

use serde_json::{json, Map, Value};

use wb_storage::{read_settings, replace_settings};

use crate::settings::{
    clamp_credits_refresh_hours, clamp_pricing_refresh_minutes_f64, credits_refresh_hours,
    pricing_enabled, pricing_refresh_minutes_f64, pricing_variant_inherit, update_check_enabled,
};
use crate::update_checker::HUB_VERSION;

// ---------------------------------------------------------------------------
// 常量表（`wb_settings.py:20-108`）
// ---------------------------------------------------------------------------

/// 五个限额守卫的键名。Python `wb_settings.LIMIT_KEYS`（`:575-577`）。
pub const LIMIT_KEYS: [&str; 5] = [
    "reserve_credits",
    "daily_token_limit",
    "daily_credit_limit",
    "model_daily_token_limit",
    "expiring_window_days",
];

/// 两个出口 realm。Python `wb_settings.LIMIT_REALMS`（`:578`）。
pub const LIMIT_REALMS: [&str; 2] = ["intl", "cn"];

/// 三个作用域。Python `wb_settings.LIMIT_SCOPES`（`:579`）= `("global",) + LIMIT_REALMS`。
pub const LIMIT_SCOPES: [&str; 3] = ["global", "intl", "cn"];

/// 分组限额在 settings.json 里的键名。Python `wb_settings.LIMITS_KEY`（`:580`）。
pub const LIMITS_KEY: &str = "limits";

/// 默认值不是 0 的守卫。Python `wb_settings.LIMIT_DEFAULTS`（`:585`）。
const LIMIT_DEFAULTS: [(&str, i64); 1] = [("expiring_window_days", 7)];

/// 实例级默认界面语言的三个取值。Python `wb_settings.UI_LANGUAGE_VALUES`（`:89`）。
pub const UI_LANGUAGE_VALUES: [&str; 3] = ["zh", "zh-Hant", "en"];

/// 界面语言的默认值。Python `wb_settings.UI_LANGUAGE_DEFAULT`（`:88`）。
pub const UI_LANGUAGE_DEFAULT: &str = "zh";

/// 永远不会被隐藏的那一个页面（切换器自己所在的页）。Python
/// `wb_settings.PAGE_KEEP_VISIBLE`（`:73`）。
pub const PAGE_KEEP_VISIBLE: &str = "settings";

/// 隐藏页面列表的长度上限。Python `wb_settings.MAX_HIDDEN_PAGES`（`:76`）。
pub const MAX_HIDDEN_PAGES: usize = 32;

/// 上游配置的四项默认值。Python `wb_settings.UPSTREAM_DEFAULTS`（`:1021-1026`）。
///
/// 用函数而不是 `const`：`json!` 展开出来的 `Value` 不是编译期常量。
fn upstream_defaults() -> [(&'static str, Value); 4] {
    [
        ("header_timeout_seconds", json!(120)),
        ("idle_timeout_seconds", json!(300)),
        ("device_token", json!("")),
        ("device_token_file", json!("")),
    ]
}

/// 系统提示词配置的两项默认值。Python `wb_settings.PROMPT_DEFAULTS`（`:1076-1080`）。
const PROMPT_DEFAULTS: [(&str, &str); 2] = [("mode", "passthrough"), ("file", "")];

/// 面板保存接口**已支持**的顶层键。
///
/// 未列出的键（`api_keys` / `upstream` / `prompt` / `auth_disabled` /
/// `restart_scheduler` / 顶层的扁平限额键 …）一律 400，**不静默忽略**。
///
/// ⚠️ 与 Python 的差异：Python 的保存处理器**接受**这些键（`api_keys` 数组、
/// `upstream` / `prompt` 对象、`auth_disabled`、扁平限额键、`restart_scheduler`）。
/// 本移植只做面板前端**实际会发**的那一批，其余如实报错。
pub const SUPPORTED_PATCH_KEYS: [&str; 17] = [
    "accounts_collapsed",
    "hidden_pages",
    "accounts_separate_tab",
    "ui_language",
    "limits",
    "credits_refresh_hours",
    "pricing_enabled",
    "pricing_refresh_minutes",
    // 旧名（单位是小时），保存时按 ×60 换算（Python `wb_proxy.py:16514-16538`）
    "pricing_refresh_hours",
    "pricing_variant_inherit",
    "auto_switch_product",
    "daily_chat_web",
    "local_web_tools",
    "remaining_priority_enabled",
    "egress_backoff_enabled",
    "update_check_enabled",
    "api_key",
];

// ---------------------------------------------------------------------------
// 读视图
// ---------------------------------------------------------------------------

/// 面板设置读视图（Python `wb_proxy.runtime_settings_view()` 的等价物）。
///
/// `accounts_dir` = 账号目录；`usage_dir` = 用量目录；`api_key` = 生效中的 API key
/// （`None` ⇒ 未设置）。
///
/// 返回的 JSON **顶层键与顺序**与 Python 的字面量逐个对齐（33 个键）。
pub fn runtime_settings_view(
    accounts_dir: &Path,
    usage_dir: &Path,
    api_key: Option<&str>,
) -> Value {
    let doc = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    let doc = doc.as_object().cloned().unwrap_or_default();
    // `settings.rs` 里的几个 getter 收的是整个 settings 文档（`&Value`）。
    let doc_value = Value::Object(doc.clone());
    let key = api_key.unwrap_or("");
    let limits = limits_from_doc(&doc);

    json!({
        "panel_password_is_default": panel_password_is_default(&doc),
        "api_key_set": !key.is_empty(),
        "api_key_set_by_panel": api_key_set_by_panel(&doc),
        "api_key_masked": mask_key(key),
        "auth_required": auth_required(&doc, key),
        "api_keys": api_keys_view(&doc),
        "deleted_api_keys": deleted_api_keys_view(&doc),
        "limits": limits,
        "reserve_credits": limit_value(&limits, "reserve_credits", None),
        "daily_token_limit": limit_value(&limits, "daily_token_limit", None),
        "daily_credit_limit": limit_value(&limits, "daily_credit_limit", None),
        "model_daily_token_limit": limit_value(&limits, "model_daily_token_limit", None),
        "pricing_refresh_minutes": pricing_refresh_minutes_f64(&doc_value),
        "credits_refresh_hours": credits_refresh_hours(&doc_value),
        "ui_language": ui_language(&doc),
        "pricing_variant_inherit": pricing_variant_inherit(&doc_value),
        "pricing_enabled": pricing_enabled(&doc_value),
        "auto_switch_product": auto_switch_product(&doc),
        "daily_chat_web": daily_chat_web(&doc),
        "local_web_tools": local_web_tools(&doc),
        "remaining_priority_enabled": remaining_priority_enabled(&doc),
        "egress_backoff_enabled": egress_backoff_enabled(&doc),
        "accounts_collapsed": accounts_collapsed(&doc),
        "accounts_separate_tab": accounts_separate_tab(&doc),
        "key_before_hidden": key_before_hidden(&doc),
        "hidden_pages": hidden_pages(&doc),
        "update_check_enabled": update_check_enabled(&doc_value),
        "upstream": upstream_config(&doc),
        "prompt": prompt_config(&doc),
        "accounts_dir": accounts_dir.to_string_lossy(),
        "usage_dir": usage_dir.to_string_lossy(),
        "settings_file": settings_file_path(accounts_dir),
        "version": HUB_VERSION,
    })
}

/// `settings.json` 的完整路径。Python `wb_settings.settings_path()`（`:112-113`）。
fn settings_file_path(accounts_dir: &Path) -> String {
    accounts_dir
        .join("settings.json")
        .to_string_lossy()
        .into_owned()
}

/// 口令是否还是默认值。Python `wb_settings.panel_password_is_default()`（`:196-200`）。
///
/// settings.json 里没有（空的）`panel_password_hash` ⇒ 从没改过 ⇒ 仍是默认值；
/// 改过之后只有 `panel_password_default` 恰为布尔 `true` 才算默认值。
fn panel_password_is_default(doc: &Map<String, Value>) -> bool {
    if !truthy_opt(doc.get("panel_password_hash")) {
        return true;
    }
    doc.get("panel_password_default").and_then(Value::as_bool) == Some(true)
}

/// 生效的 key 是否由面板写入。Python `wb_proxy.API_KEY_FILE_SET`（`:412`）。
///
/// Python 侧它是**运行时**全局量：启动时若 `wb_settings.api_key_override()` 说
/// 「key 来自面板」就置真（`wb_proxy.py:18735-18738`），面板保存 key 时也置真
/// （`:16715`）。Rust 侧没有这个运行时量，文件侧的等价物就是 `api_key_set` 键
/// （`wb_settings.set_api_key()` 会写它）——这里按它判定。
fn api_key_set_by_panel(doc: &Map<String, Value>) -> bool {
    truthy_opt(doc.get("api_key_set"))
}

/// 是否要求 /v1 调用带上 key。Python `wb_proxy.auth_required()`（`:420-426`）。
///
/// 顺序与 Python 一致：`auth_disabled` 为布尔 `true` ⇒ 不要求；否则面板里有一
/// 条**启用**的 key ⇒ 要求；否则看生效的 `api_key`。
fn auth_required(doc: &Map<String, Value>, api_key: &str) -> bool {
    if doc.get("auth_disabled").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if api_keys_entries(doc, false)
        .iter()
        .any(|entry| entry.enabled)
    {
        return true;
    }
    !api_key.is_empty()
}

/// 面板可见的 key 列表（不含已删除的）。Python `runtime_settings_view()` 的
/// `keys` 分支（`wb_proxy.py:9668-9680`）。
fn api_keys_view(doc: &Map<String, Value>) -> Vec<Value> {
    api_keys_entries(doc, false)
        .into_iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "name": entry.name,
                "realm": entry.realm,
                "models": entry.models,
                "enabled": entry.enabled,
                "masked": mask_key(&entry.key),
                "source": "panel",
                "created_at": entry.created_at,
            })
        })
        .collect()
}

/// 已删除的 key（只读历史：密钥已消失，面板只能列出名字与日期）。
/// Python `runtime_settings_view()` 的 `deleted_keys` 分支（`wb_proxy.py:9683-9693`）。
fn deleted_api_keys_view(doc: &Map<String, Value>) -> Vec<Value> {
    api_keys_entries(doc, true)
        .into_iter()
        .filter(|entry| !entry.deleted_at.is_empty())
        .map(|entry| {
            json!({
                "id": entry.id,
                "name": entry.name,
                "realm": entry.realm,
                "created_at": entry.created_at,
                "deleted_at": entry.deleted_at,
            })
        })
        .collect()
}

/// 一条 key 条目（归一化后的形状）。
#[derive(Debug, Clone)]
struct KeyEntryView {
    id: String,
    name: String,
    key: String,
    realm: String,
    models: Vec<String>,
    enabled: bool,
    created_at: String,
    deleted_at: String,
}

/// 归一化后的 key 条目集合。Python `wb_settings.api_keys()`（`:381-431`）。
///
/// - `api_keys` 是数组 ⇒ 逐条归一化（`_clean_key_entry`），同密钥去重、id 去重；
/// - `api_keys` 不存在而是旧版 `api_key_set` + `api_key` ⇒ 合成一条 legacy 条目；
/// - `include_deleted` 为 false ⇒ 过滤掉已删除的条目。
fn api_keys_entries(doc: &Map<String, Value>, include_deleted: bool) -> Vec<KeyEntryView> {
    let mut out: Vec<KeyEntryView> = Vec::new();
    match doc.get("api_keys") {
        Some(Value::Array(stored)) => {
            let mut seen_secrets: Vec<String> = Vec::new();
            let mut seen_ids: Vec<String> = Vec::new();
            for raw in stored {
                let Some(mut entry) = clean_key_entry(raw) else {
                    continue;
                };
                if !entry.key.is_empty() {
                    if seen_secrets.contains(&entry.key) {
                        continue;
                    }
                    seen_secrets.push(entry.key.clone());
                }
                entry.id = unique_key_id(&entry.id, &seen_ids);
                seen_ids.push(entry.id.clone());
                out.push(entry);
            }
            if !include_deleted {
                out.retain(|entry| entry.deleted_at.is_empty());
            }
        }
        _ => {
            // 旧版单 key 形态：合成一条（Python `wb_settings.py:418-430`）
            if truthy_opt(doc.get("api_key_set")) {
                let legacy = py_str_or_empty(doc.get("api_key"));
                let legacy = legacy.trim();
                if !legacy.is_empty() {
                    out.push(KeyEntryView {
                        id: "legacy".to_string(),
                        name: "默认（跟随面板切换）".to_string(),
                        key: legacy.to_string(),
                        realm: String::new(),
                        models: Vec::new(),
                        enabled: true,
                        created_at: String::new(),
                        deleted_at: String::new(),
                    });
                }
            }
        }
    }
    out
}

/// 归一化一条存下来的 key 条目。Python `wb_settings._clean_key_entry()`（`:326-353`）。
fn clean_key_entry(raw: &Value) -> Option<KeyEntryView> {
    let obj = raw.as_object()?;
    let key = py_str_or_empty(obj.get("key")).trim().to_string();
    let deleted_at = py_str_or_empty(obj.get("deleted_at")).trim().to_string();
    if key.is_empty() && deleted_at.is_empty() {
        return None;
    }
    let mut realm = py_str_or_empty(obj.get("realm")).trim().to_lowercase();
    if !["", "intl", "cn"].contains(&realm.as_str()) {
        realm = String::new();
    }
    let name = py_str_or_empty(obj.get("name")).trim().to_string();
    let created_at = match obj.get("created_at") {
        Some(value) if truthy(value) => py_str_or_empty(Some(value)),
        _ => now_stamp(),
    };
    Some(KeyEntryView {
        id: py_str_or_empty(obj.get("id")),
        name: if name.is_empty() {
            "未命名".to_string()
        } else {
            name
        },
        key,
        realm,
        models: clean_model_patterns(obj.get("models")),
        // Python: `False if deleted_at else entry.get("enabled", True) is not False`
        enabled: if deleted_at.is_empty() {
            obj.get("enabled").and_then(Value::as_bool) != Some(false)
        } else {
            false
        },
        created_at,
        deleted_at,
    })
}

/// 模型白名单的归一化。Python `wb_settings._clean_model_patterns()`（`:283-304`）。
///
/// 字符串按 `,` / `;` / 换行切开；列表逐项取字符串；其它形态 ⇒ 空列表。
/// 每项 trim + 转小写 + 去重。
fn clean_model_patterns(value: Option<&Value>) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    match value {
        Some(Value::String(s)) => {
            raw.extend(s.split([',', ';', '\n']).map(str::to_string));
        }
        Some(Value::Array(items)) => {
            for item in items {
                raw.push(py_str_or_empty(Some(item)));
            }
        }
        _ => return Vec::new(),
    }
    let mut out: Vec<String> = Vec::new();
    for item in raw {
        let pattern = item.trim().to_lowercase();
        if !pattern.is_empty() && !out.contains(&pattern) {
            out.push(pattern);
        }
    }
    out
}

/// 让 id 在集合内唯一。Python `wb_settings._unique_key_id()`（`:355-378`）。
///
/// 重复的 id 追加 `-2` / `-3` …；空 id 保持为空（Python 会随机铸一个六位十六进制
/// id —— 那是**非确定性**的，读视图里会让同一个条目两次读出不同的 id，故此处
/// 留空并在报告里登记为有意差异）。
fn unique_key_id(candidate: &str, used: &[String]) -> String {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return String::new();
    }
    if !used.iter().any(|id| id == candidate) {
        return candidate.to_string();
    }
    let mut suffix = 2;
    loop {
        let alt = format!("{candidate}-{suffix}");
        if !used.contains(&alt) {
            return alt;
        }
        suffix += 1;
    }
}

/// 密钥打码。Python `runtime_settings_view()` 的 `masked`（`wb_proxy.py:9663-9667`）。
///
/// 长度 > 8 ⇒ 前 4 + 6 个 `*` + 后 4；否则整串都是 `*`。
fn mask_key(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}{}{tail}", "*".repeat(6))
    } else {
        "*".repeat(chars.len())
    }
}

/// 界面语言。Python `wb_settings.ui_language()`（`:1002-1007`）。
fn ui_language(doc: &Map<String, Value>) -> String {
    match doc.get("ui_language").and_then(Value::as_str) {
        Some(lang) if UI_LANGUAGE_VALUES.contains(&lang) => lang.to_string(),
        _ => UI_LANGUAGE_DEFAULT.to_string(),
    }
}

/// `auto_switch_product`。Python `wb_settings.auto_switch_product()`（`:1131-1139`）：
/// 缺键 ⇒ 关；只有布尔 `true` 算开。
fn auto_switch_product(doc: &Map<String, Value>) -> bool {
    doc.get("auto_switch_product").and_then(Value::as_bool) == Some(true)
}

/// `daily_chat_web`。Python `wb_settings.daily_chat_web()`（`:1200-1211`）：
/// 缺键 ⇒ **开**；有键 ⇒ 只有布尔 `true` 算开。
fn daily_chat_web(doc: &Map<String, Value>) -> bool {
    match doc.get("daily_chat_web") {
        None => true,
        Some(value) => value.as_bool() == Some(true),
    }
}

/// `local_web_tools`。Python `wb_settings.local_web_tools()`（`:1222-1233`）：缺键 ⇒ 关。
fn local_web_tools(doc: &Map<String, Value>) -> bool {
    doc.get("local_web_tools").and_then(Value::as_bool) == Some(true)
}

/// `remaining_priority_enabled`。Python `wb_settings.remaining_priority_enabled()`
/// （`:1152-1163`）：缺键 ⇒ 关。
fn remaining_priority_enabled(doc: &Map<String, Value>) -> bool {
    doc.get("remaining_priority_enabled")
        .and_then(Value::as_bool)
        == Some(true)
}

/// `egress_backoff_enabled`。Python `wb_settings.egress_backoff_enabled()`
/// （`:1176-1187`）：缺键 ⇒ 关。
fn egress_backoff_enabled(doc: &Map<String, Value>) -> bool {
    doc.get("egress_backoff_enabled").and_then(Value::as_bool) == Some(true)
}

/// `accounts_collapsed`。Python `wb_settings.accounts_collapsed()`（`:1246-1254`）：
/// 缺键 ⇒ 展开（关）。
fn accounts_collapsed(doc: &Map<String, Value>) -> bool {
    doc.get("accounts_collapsed").and_then(Value::as_bool) == Some(true)
}

/// `accounts_separate_tab`。Python `wb_settings.accounts_separate_tab()`（`:1333-1341`）：
/// 缺键 ⇒ 关。
fn accounts_separate_tab(doc: &Map<String, Value>) -> bool {
    doc.get("accounts_separate_tab").and_then(Value::as_bool) == Some(true)
}

/// `key_before_hidden`。Python `wb_settings.key_before_hidden()`（`:1267-1275`）：
/// 缺键 ⇒ 显示（关）。
fn key_before_hidden(doc: &Map<String, Value>) -> bool {
    doc.get("key_before_hidden").and_then(Value::as_bool) == Some(true)
}

/// 隐藏的页面列表（读时归一化）。Python `wb_settings.hidden_pages()`（`:1313-1320`）。
fn hidden_pages(doc: &Map<String, Value>) -> Vec<String> {
    clean_page_keys(doc.get("hidden_pages"))
}

/// 页面键的归一化。Python `wb_settings._clean_page_keys()`（`:1288-1310`）。
///
/// - 非列表 ⇒ 空列表；
/// - 非字符串项 ⇒ 跳过；
/// - `trim` 后必须匹配 `^[a-z][a-z0-9_-]{0,31}$`（Python `PAGE_KEY_RE`）；
/// - `"settings"` 永远被丢掉（切换器自己所在的页，藏起来就回不去了）；
/// - 去重；最多 32 条。
fn clean_page_keys(value: Option<&Value>) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else {
            continue;
        };
        let key = text.trim();
        if !page_key_ok(key) || key == PAGE_KEEP_VISIBLE || out.iter().any(|k| k == key) {
            continue;
        }
        out.push(key.to_string());
        if out.len() >= MAX_HIDDEN_PAGES {
            break;
        }
    }
    out
}

/// 页面键是否合法。Python `wb_settings.PAGE_KEY_RE`（`:68`）：
/// `^[a-z][a-z0-9_-]{0,31}$`（首字符小写字母，之后最多 31 个小写字母/数字/`_`/`-`）。
fn page_key_ok(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    let rest: Vec<char> = chars.collect();
    if rest.len() > 31 {
        return false;
    }
    rest.iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
}

/// 上游配置。Python `wb_settings.upstream_config()`（`:1048-1060`）。
fn upstream_config(doc: &Map<String, Value>) -> Value {
    let stored = doc.get("upstream").and_then(Value::as_object).cloned();
    let mut out = Map::new();
    for (key, default) in upstream_defaults().iter() {
        let value = stored.as_ref().and_then(|s| s.get(*key));
        let normalized = match value {
            None => default.clone(),
            Some(value) => {
                if *key == "header_timeout_seconds" || *key == "idle_timeout_seconds" {
                    match value.as_i64() {
                        Some(seconds) if seconds >= 1 => json!(seconds),
                        _ => default.clone(),
                    }
                } else {
                    match value.as_str() {
                        Some(text) => json!(text.trim()),
                        None => default.clone(),
                    }
                }
            }
        };
        out.insert((*key).to_string(), normalized);
    }
    Value::Object(out)
}

/// 系统提示词配置。Python `wb_settings.prompt_config()`（`:1102-1115`）。
fn prompt_config(doc: &Map<String, Value>) -> Value {
    let stored = doc.get("prompt").and_then(Value::as_object).cloned();
    let mode = match stored.as_ref().and_then(|s| s.get("mode")) {
        Some(Value::String(text)) => {
            let lowered = text.trim().to_lowercase();
            if ["passthrough", "custom", "append"].contains(&lowered.as_str()) {
                lowered
            } else {
                PROMPT_DEFAULTS[0].1.to_string()
            }
        }
        _ => PROMPT_DEFAULTS[0].1.to_string(),
    };
    let file = match stored.as_ref().and_then(|s| s.get("file")) {
        Some(Value::String(text)) => text.trim().to_string(),
        _ => PROMPT_DEFAULTS[1].1.to_string(),
    };
    json!({ "mode": mode, "file": file })
}

// ---------------------------------------------------------------------------
// 限额（limits）
// ---------------------------------------------------------------------------

/// 读出分组限额（**纯读**：旧版扁平键只在内存里折叠，不写回）。
///
/// Python `wb_settings.limits_data()`（`:663-684`）：
/// 文件里已有 `limits` 对象 ⇒ 归一化它；否则把旧版的五个扁平键折叠进来
/// （Python 会顺手写回文件，本移植**不写**——见模块文档的差异登记）。
fn limits_from_doc(doc: &Map<String, Value>) -> Value {
    match doc.get(LIMITS_KEY) {
        Some(raw) if raw.is_object() => normalize_limits(raw),
        _ => {
            let mut copy = doc.clone();
            let (_, folded) = fold_legacy_limits(&mut copy);
            folded
        }
    }
}

/// 归一化分组限额：五个键、三个作用域一个都不缺。
/// Python `wb_settings._normalize_limits()`（`:641-660`）。
fn normalize_limits(raw: &Value) -> Value {
    let mut out = Map::new();
    for key in LIMIT_KEYS.iter() {
        let entry = raw
            .get(*key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let global = match entry.get("global") {
            None => json!(default_global(key)),
            Some(value) => json!(coerce_global(value)),
        };
        let intl = coerce_override(entry.get("intl"));
        let cn = coerce_override(entry.get("cn"));
        out.insert(
            (*key).to_string(),
            json!({ "global": global, "intl": intl, "cn": cn }),
        );
    }
    Value::Object(out)
}

/// 把旧版的五个扁平限额键折叠进 `limits`，并**从文档里删掉**它们。
/// Python `wb_settings._fold_legacy_limits()`（`:623-638`）。
///
/// 返回 `(是否折叠过, 折叠后的 limits)`。
fn fold_legacy_limits(doc: &mut Map<String, Value>) -> (bool, Value) {
    let mut limits = Map::new();
    let mut changed = false;
    for key in LIMIT_KEYS.iter() {
        let mut entry = Map::new();
        entry.insert("global".to_string(), json!(default_global(key)));
        entry.insert("intl".to_string(), Value::Null);
        entry.insert("cn".to_string(), Value::Null);
        if let Some(value) = doc.remove(*key) {
            entry.insert("global".to_string(), json!(coerce_global(&value)));
            changed = true;
        }
        limits.insert((*key).to_string(), Value::Object(entry));
    }
    (changed, Value::Object(limits))
}

/// 某个守卫的出厂全局值。Python `wb_settings._default_global()`（`:588-594`）。
fn default_global(key: &str) -> i64 {
    LIMIT_DEFAULTS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
        .unwrap_or(0)
}

/// 全局阈值的归一化。Python `wb_settings._coerce_global()`（`:601-608`）：
/// `int(value or 0)`，失败 ⇒ 0，负数 ⇒ 0。
fn coerce_global(value: &Value) -> i64 {
    if !truthy(value) {
        return 0;
    }
    python_int(value).unwrap_or(0).max(0)
}

/// 单 realm 覆盖值的归一化。Python `wb_settings._coerce_override()`（`:611-620`）：
/// `None` 或空串 ⇒ `None`（= 继承全局）；`int(value)` 失败 ⇒ `None`；负数 ⇒ 0。
fn coerce_override(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    if let Value::String(text) = value {
        if text.is_empty() {
            return Value::Null;
        }
    }
    match python_int(value) {
        Some(number) => json!(number.max(0)),
        None => Value::Null,
    }
}

/// 某个守卫在某个 realm 上的**生效值**。Python `wb_settings.limit_value()`
/// （`:692-705`）。
///
/// realm 为 `None`（或不是 intl/cn）⇒ 全局值；realm 有覆盖 ⇒ 覆盖值；
/// 覆盖为 `None` ⇒ **回落到全局**。
fn limit_value(limits: &Value, key: &str, realm: Option<&str>) -> i64 {
    let entry = limits.get(key).and_then(Value::as_object);
    let global = entry
        .and_then(|e| e.get("global"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if let Some(realm) = realm {
        if LIMIT_REALMS.contains(&realm) {
            if let Some(override_value) = entry.and_then(|e| e.get(realm)) {
                if let Some(number) = override_value.as_i64() {
                    return number;
                }
            }
        }
    }
    global
}

/// 写入一个守卫的一个作用域。Python `wb_settings.set_limit()`（`:738-762`）。
///
/// 文件里还没有 `limits` 对象时，先把旧版扁平键折叠进来（并从文档里删掉它们）。
fn set_limit(doc: &mut Map<String, Value>, key: &str, scope: &str, value: Option<i64>) {
    let mut limits = match doc.get(LIMITS_KEY) {
        Some(raw) if raw.is_object() => normalize_limits(raw),
        _ => {
            let (_, folded) = fold_legacy_limits(doc);
            folded
        }
    };
    if let Some(entry) = limits.get_mut(key).and_then(Value::as_object_mut) {
        if scope == "global" {
            entry.insert("global".to_string(), json!(value.unwrap_or(0)));
        } else {
            entry.insert(
                scope.to_string(),
                match value {
                    Some(number) => json!(number),
                    None => Value::Null,
                },
            );
        }
    }
    doc.insert(LIMITS_KEY.to_string(), limits);
}

// ---------------------------------------------------------------------------
// 保存
// ---------------------------------------------------------------------------

/// 面板设置保存（Python `wb_proxy._handle_settings_save()` 的等价物）。
///
/// `patch` = 请求体（只含要改的键；未出现的键一律不碰）。
///
/// ## 返回值
///
/// `Ok((settings, reply))`：
///
/// - `settings` = **落盘后的完整 settings 文档**（= `wb_settings.load()` 的等价物）。
///   面板接线层可以从中读出新的 API key（Python 侧是写全局 `API_KEY`）等生效值。
/// - `reply` = **增量回包**（本次保存动作产生的额外键）：每个被处理的键回它的
///   **生效值**（`pricing_enabled` 回布尔、`pricing_refresh_minutes` 回**分钟数**
///   （浮点）、`hidden_pages` 回**规范化后**的列表、`limits` 回完整快照），
///   外加 `pricing_refresh_started`（**仅当** `pricing_variant_inherit` 的值确实变了）。
///
/// ## 响应体怎么拼（与 Python 一致）
///
/// Python 的保存处理器最后一行是 `reply.update(runtime_settings_view())`：
/// **视图覆盖增量**，视图里没有的增量键（`pricing_refresh_started`）留下。
/// Rust 侧等价写法：
///
/// ```text
/// let (settings, reply) = wb_tasks::settings_view::save_settings(&accounts_dir, &patch)?;
/// let mut body = wb_tasks::settings_view::runtime_settings_view(&accounts_dir, &usage_dir, api_key);
/// if let (Some(body_map), Some(reply_map)) = (body.as_object_mut(), reply.as_object()) {
///     for (key, value) in reply_map {
///         body_map.insert(key.clone(), value.clone());
///     }
/// }
/// ```
///
/// （`serde_json` 的 `preserve_order` 下，已存在的键**保持原位**，新增的键追加到
/// 末尾 —— 与 Python `dict.update` 的键序行为一致。）
///
/// ## 运行时副作用（本函数**不做**）
///
/// Python 的保存处理器有 12 处副作用（`PRICING.wake()` / `set_interval` /
/// `run_once` / `CREDITS_REFRESHER.wake()` / `UPDATES.wake()` /
/// `SCHEDULER.stop+start` / 改全局 `API_KEY` …）。本移植只负责**落盘 + 校验 +
/// 算出生效值**，由调用方按 `reply` 里的键去触发后台动作。
///
/// ## 失败
///
/// `Err((400, 文案))` —— 对齐 Python 的 400 + `invalid_request_error`。
/// 文案逐条照抄 Python 的 `_handle_settings_save()` 里的原文。
pub fn save_settings(accounts_dir: &Path, patch: &Value) -> Result<(Value, Value), (u16, String)> {
    let patch_map = patch
        .as_object()
        .ok_or((400, "payload must be a JSON object".to_string()))?;

    // ① 不支持的顶层键 ⇒ 400（不静默忽略；Python 是静默忽略，见模块文档）
    for key in patch_map.keys() {
        if !SUPPORTED_PATCH_KEYS.contains(&key.as_str()) {
            return Err((400, format!("unsupported settings key: \"{key}\"")));
        }
    }

    // ② 宽容读（Python `load()`：坏 JSON 当空对象）
    let raw = read_settings(accounts_dir).unwrap_or_else(|_| json!({}));
    let mut doc = raw.as_object().cloned().unwrap_or_default();

    // ③ 旧版扁平限额键的折叠（Python 的 `limits_data()` 在读时顺手写回；
    //    本移植在**写**时折叠，纯读视图不写盘）
    let mut folded = false;
    if !doc.get(LIMITS_KEY).map(Value::is_object).unwrap_or(false) {
        let mut probe = doc.clone();
        let (changed, limits) = fold_legacy_limits(&mut probe);
        if changed {
            doc = probe;
            doc.insert(LIMITS_KEY.to_string(), limits);
            folded = true;
        }
    }

    let mut reply = Map::new();

    // ④ limits（Python `wb_proxy.py:16442-16503`）
    // Python: `limits_payload = payload.get("limits")` —— 值为 JSON null 时
    // `limits_payload is None` ⇒ 整个分支被跳过（当成没传）。
    let limits_payload = patch_map.get("limits").filter(|value| !value.is_null());
    if let Some(value) = limits_payload {
        if !value.is_object() {
            return Err((400, "limits must be an object".to_string()));
        }
    }
    for key in LIMIT_KEYS.iter() {
        let grouped = limits_payload.and_then(|value| value.get(*key));
        let Some(grouped) = grouped.filter(|value| value.is_object()) else {
            // Python 会在这里回落到「顶层扁平键」形态；本移植不支持顶层扁平限额键
            // （见 SUPPORTED_PATCH_KEYS），因此直接跳过（与 Python 的 `continue` 一致）。
            continue;
        };
        for scope in LIMIT_SCOPES.iter() {
            let Some(raw_value) = grouped.get(*scope) else {
                continue;
            };
            let number = parse_limit_number(key, scope, raw_value)?;
            set_limit(&mut doc, key, scope, number);
        }
        let limits = limits_from_doc(&doc);
        reply.insert((*key).to_string(), json!(limit_value(&limits, key, None)));
    }
    if limits_payload.is_some() {
        reply.insert(LIMITS_KEY.to_string(), limits_from_doc(&doc));
    }

    // ⑤ pricing_enabled（Python `wb_proxy.py:16505-16513`）
    if let Some(raw_value) = patch_map.get("pricing_enabled") {
        let value = strict_bool(raw_value, "pricing_enabled")?;
        doc.insert("pricing_enabled".to_string(), json!(value));
        reply.insert("pricing_enabled".to_string(), json!(value));
    }

    // ⑥ pricing_refresh_minutes（旧名 pricing_refresh_hours 按 ×60）
    //    Python `wb_proxy.py:16514-16538`
    let refresh_field = if patch_map.contains_key("pricing_refresh_minutes") {
        "pricing_refresh_minutes"
    } else if patch_map.contains_key("pricing_refresh_hours") {
        "pricing_refresh_hours"
    } else {
        ""
    };
    if !refresh_field.is_empty() {
        let raw_value = patch_map.get(refresh_field).unwrap_or(&Value::Null);
        if raw_value.is_null() || raw_value.is_boolean() {
            return Err((400, format!("{refresh_field} must be a number")));
        }
        let mut minutes = match python_float(raw_value) {
            Some(value) => value,
            None => return Err((400, format!("{refresh_field} must be a number"))),
        };
        if refresh_field == "pricing_refresh_hours" {
            minutes *= 60.0;
        }
        if minutes < 0.0 {
            return Err((
                400,
                "pricing_refresh_minutes cannot be negative".to_string(),
            ));
        }
        let stored = clamp_pricing_refresh_minutes_f64(minutes);
        doc.insert("pricing_refresh_minutes".to_string(), json!(stored));
        // 旧键不再有存在的意义（Python `set_pricing_refresh_minutes()` 会 pop 掉它）
        doc.remove("pricing_refresh_hours");
        reply.insert("pricing_refresh_minutes".to_string(), json!(stored));
    }

    // ⑦ credits_refresh_hours（Python `wb_proxy.py:16539-16557`）
    if let Some(raw_value) = patch_map.get("credits_refresh_hours") {
        if raw_value.is_null() || raw_value.is_boolean() {
            return Err((400, "credits_refresh_hours must be a number".to_string()));
        }
        let hours = match python_float(raw_value) {
            Some(value) => value,
            None => return Err((400, "credits_refresh_hours must be a number".to_string())),
        };
        if hours < 0.0 {
            return Err((400, "credits_refresh_hours cannot be negative".to_string()));
        }
        let stored = clamp_credits_refresh_hours(hours);
        doc.insert("credits_refresh_hours".to_string(), json!(stored));
        reply.insert("credits_refresh_hours".to_string(), json!(stored));
    }

    // ⑧ ui_language（Python `wb_proxy.py:16558-16566`）
    if let Some(raw_value) = patch_map.get("ui_language") {
        let value = match raw_value.as_str() {
            Some(text) if UI_LANGUAGE_VALUES.contains(&text) => text.to_string(),
            _ => return Err((400, "ui_language must be zh, zh-Hant or en".to_string())),
        };
        doc.insert("ui_language".to_string(), json!(value));
        reply.insert("ui_language".to_string(), json!(value));
    }

    // ⑨ pricing_variant_inherit（Python `wb_proxy.py:16567-16583`）
    if let Some(raw_value) = patch_map.get("pricing_variant_inherit") {
        let value = strict_bool(raw_value, "pricing_variant_inherit")?;
        let previous = pricing_variant_inherit(&Value::Object(doc.clone()));
        doc.insert("pricing_variant_inherit".to_string(), json!(value));
        reply.insert("pricing_variant_inherit".to_string(), json!(value));
        if previous != value {
            // 这个开关只在**下一次**取价时生效（刷新才会带上/去掉后缀继承的定价），
            // 所以 Python 立刻起一次刷新线程；本移植只回报这个事实，由调用方触发。
            reply.insert("pricing_refresh_started".to_string(), json!(true));
        }
    }

    // ⑩ 其余布尔开关（Python `wb_proxy.py:16584-16683`）
    for key in [
        "auto_switch_product",
        "daily_chat_web",
        "local_web_tools",
        "remaining_priority_enabled",
        "egress_backoff_enabled",
        "accounts_collapsed",
        "accounts_separate_tab",
    ] {
        if let Some(raw_value) = patch_map.get(key) {
            let value = strict_bool(raw_value, key)?;
            doc.insert(key.to_string(), json!(value));
            reply.insert(key.to_string(), json!(value));
        }
    }

    // ⑪ hidden_pages（Python `wb_proxy.py:16658-16671`）
    if let Some(raw_value) = patch_map.get("hidden_pages") {
        let items = raw_value
            .as_array()
            .ok_or((400, "hidden_pages must be a list of page keys".to_string()))?;
        if items.iter().any(|item| !item.is_string()) {
            return Err((400, "hidden_pages must be a list of page keys".to_string()));
        }
        // 回包给**规范化后**的列表：拼错的键在入库时就被丢掉了。
        let cleaned = clean_page_keys(Some(raw_value));
        doc.insert("hidden_pages".to_string(), json!(cleaned));
        reply.insert("hidden_pages".to_string(), json!(cleaned));
    }

    // ⑫ update_check_enabled（Python `wb_proxy.py:16672-16683`）
    if let Some(raw_value) = patch_map.get("update_check_enabled") {
        let value = strict_bool(raw_value, "update_check_enabled")?;
        doc.insert("update_check_enabled".to_string(), json!(value));
        reply.insert("update_check_enabled".to_string(), json!(value));
    }

    // ⑬ api_key（Python `wb_proxy.py:16706-16716`）
    // Python: `new_key = payload.get("api_key"); if new_key is not None:` ——
    // 值是 JSON null 时整个分支被跳过（= 没传这个键）。
    if let Some(raw_value) = patch_map.get("api_key").filter(|value| !value.is_null()) {
        let new_key = match raw_value {
            Value::String(text) => text.trim().to_string(),
            Value::Number(_) | Value::Bool(_) => {
                py_str(raw_value).unwrap_or_default().trim().to_string()
            }
            _ => return Err((400, "api key must be a string".to_string())),
        };
        if !new_key.is_empty() && new_key.chars().count() < 4 {
            return Err((400, "api key must be at least 4 characters".to_string()));
        }
        doc.insert("api_key".to_string(), json!(new_key));
        doc.insert("api_key_set".to_string(), json!(true));
        reply.insert("api_key_set".to_string(), json!(!new_key.is_empty()));
    }

    // ⑭ 落盘：整体替换（Python `wb_settings.save()`：读出来 → 改 → 整份写回）
    if !patch_map.is_empty() || folded {
        replace_settings(accounts_dir, &Value::Object(doc.clone()))
            .map_err(|err| (500, format!("could not write settings.json: {err}")))?;
    }

    Ok((Value::Object(doc), Value::Object(reply)))
}

/// 严格布尔判定。Python 各分支里的 `isinstance(raw, bool)` 检查。
///
/// 字符串 `"false"`、数字 `1`、数字 `0` 都不是布尔 ⇒ 400。
fn strict_bool(value: &Value, key: &str) -> Result<bool, (u16, String)> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        _ => Err((400, format!("{key} must be true or false"))),
    }
}

/// 解析一个限额值。Python `wb_proxy.py:16457-16488` 的两段校验。
///
/// - `global`：`None` / 布尔 ⇒ 400「must be a whole number」；`int()` 失败 ⇒ 同款；
///   负数 ⇒ 400「cannot be negative」。
/// - `intl` / `cn`：`None` 或空串 ⇒ 清掉覆盖（回落全局）；布尔 ⇒ 400；
///   `int()` 失败 ⇒ 400；负数 ⇒ 400。
fn parse_limit_number(key: &str, scope: &str, raw: &Value) -> Result<Option<i64>, (u16, String)> {
    if scope == "global" {
        if raw.is_null() || raw.is_boolean() {
            return Err((400, format!("{key} must be a whole number")));
        }
        let number = match python_int(raw) {
            Some(number) => number,
            None => return Err((400, format!("{key} must be a whole number"))),
        };
        if number < 0 {
            return Err((400, format!("{key} cannot be negative")));
        }
        return Ok(Some(number));
    }
    if raw.is_null() {
        return Ok(None);
    }
    if let Value::String(text) = raw {
        if text.is_empty() {
            return Ok(None);
        }
    }
    if raw.is_boolean() {
        return Err((400, format!("{key} must be a whole number")));
    }
    let number = match python_int(raw) {
        Some(number) => number,
        None => return Err((400, format!("{key} must be a whole number"))),
    };
    if number < 0 {
        return Err((400, format!("{key} cannot be negative")));
    }
    Ok(Some(number))
}

// ---------------------------------------------------------------------------
// Python 语义工具
// ---------------------------------------------------------------------------

/// Python 的真值判定（`bool(value)`）。
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python 的真值判定（`bool(value)`，值可能缺失 ⇒ 假）。
fn truthy_opt(value: Option<&Value>) -> bool {
    value.map(truthy).unwrap_or(false)
}

/// Python 的 `str(value)`（只覆盖标量；数组/对象返回 `None`）。
fn py_str(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(if *flag { "True" } else { "False" }.to_string()),
        _ => None,
    }
}

/// Python 的 `str(value or "")` —— 假值一律变成空串。
fn py_str_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(value) if truthy(value) => py_str(value).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Python 的 `float(value)`（数字 / 数字字符串 / 布尔）。
///
/// `NaN` / `inf` 视为非法（Python 会让它们一路流进 TTL 判定，是个静默的病态
/// 状态；Rust 侧已有的 `as_f64` 也是这个口径）。
fn python_float(value: &Value) -> Option<f64> {
    let parsed = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        Value::Bool(flag) => {
            if *flag {
                1.0
            } else {
                0.0
            }
        }
        _ => return None,
    };
    if parsed.is_finite() {
        Some(parsed)
    } else {
        None
    }
}

/// Python 的 `int(value)`（数字 / 十进制整数字符串 / 布尔）。
///
/// 浮点按**向零截断**（Python `int(5.9) == 5`）；字符串只接受十进制数字
/// （Python `int("5.5")` 抛 `ValueError`）。
fn python_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Some(value)
            } else if let Some(value) = number.as_u64() {
                i64::try_from(value).ok()
            } else {
                number.as_f64().map(|value| value.trunc() as i64)
            }
        }
        Value::String(text) => {
            let trimmed = text.trim();
            let (sign, digits) = match trimmed.strip_prefix('-') {
                Some(digits) => (-1i64, digits),
                None => (1i64, trimmed.strip_prefix('+').unwrap_or(trimmed)),
            };
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            digits.parse::<i64>().ok().map(|value| sign * value)
        }
        Value::Bool(flag) => Some(if *flag { 1 } else { 0 }),
        _ => None,
    }
}

/// 当前的本地时间戳（`%Y/%m/%d %H:%M`）。Python `time.strftime("%Y/%m/%d %H:%M")`。
fn now_stamp() -> String {
    chrono::Local::now().format("%Y/%m/%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_settings_file(dir: &Path, doc: &Value) {
        fs::write(
            dir.join("settings.json"),
            serde_json::to_string_pretty(doc).unwrap(),
        )
        .unwrap();
    }

    fn read_back(dir: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap()
    }

    // -----------------------------------------------------------------------
    // 读视图
    // -----------------------------------------------------------------------

    #[test]
    fn view_has_the_same_top_level_keys_in_the_same_order_as_python() {
        let dir = temp_dir();
        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), None);
        let keys: Vec<&str> = view
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "panel_password_is_default",
                "api_key_set",
                "api_key_set_by_panel",
                "api_key_masked",
                "auth_required",
                "api_keys",
                "deleted_api_keys",
                "limits",
                "reserve_credits",
                "daily_token_limit",
                "daily_credit_limit",
                "model_daily_token_limit",
                "pricing_refresh_minutes",
                "credits_refresh_hours",
                "ui_language",
                "pricing_variant_inherit",
                "pricing_enabled",
                "auto_switch_product",
                "daily_chat_web",
                "local_web_tools",
                "remaining_priority_enabled",
                "egress_backoff_enabled",
                "accounts_collapsed",
                "accounts_separate_tab",
                "key_before_hidden",
                "hidden_pages",
                "update_check_enabled",
                "upstream",
                "prompt",
                "accounts_dir",
                "usage_dir",
                "settings_file",
                "version",
            ],
            "读视图的顶层键与顺序必须与 Python runtime_settings_view() 一致"
        );
    }

    #[test]
    fn view_defaults_match_python() {
        let dir = temp_dir();
        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), None);
        assert_eq!(view["panel_password_is_default"], json!(true));
        assert_eq!(view["api_key_set"], json!(false));
        assert_eq!(view["api_key_set_by_panel"], json!(false));
        assert_eq!(view["api_key_masked"], json!(""));
        assert_eq!(view["auth_required"], json!(false));
        assert_eq!(view["pricing_refresh_minutes"], json!(5.0));
        assert_eq!(view["credits_refresh_hours"], json!(12.0));
        assert_eq!(view["ui_language"], json!("zh"));
        assert_eq!(view["pricing_variant_inherit"], json!(true));
        assert_eq!(view["pricing_enabled"], json!(true));
        assert_eq!(view["daily_chat_web"], json!(true));
        assert_eq!(view["auto_switch_product"], json!(false));
        assert_eq!(view["local_web_tools"], json!(false));
        assert_eq!(view["remaining_priority_enabled"], json!(false));
        assert_eq!(view["egress_backoff_enabled"], json!(false));
        assert_eq!(view["accounts_collapsed"], json!(false));
        assert_eq!(view["accounts_separate_tab"], json!(false));
        assert_eq!(view["key_before_hidden"], json!(false));
        assert_eq!(view["update_check_enabled"], json!(false));
        assert_eq!(view["hidden_pages"], json!([]));
        assert_eq!(view["version"], json!("1.7.1"));
        assert_eq!(view["upstream"]["header_timeout_seconds"], json!(120));
        assert_eq!(view["upstream"]["idle_timeout_seconds"], json!(300));
        assert_eq!(view["prompt"]["mode"], json!("passthrough"));
        assert_eq!(view["prompt"]["file"], json!(""));
        assert_eq!(
            view["limits"]["expiring_window_days"],
            json!({"global": 7, "intl": null, "cn": null}),
            "出厂默认：到期窗口 7 天，两个 realm 继承全局"
        );
        assert_eq!(
            view["reserve_credits"],
            json!(0),
            "reserve_credits 的出厂全局值是 0（关）"
        );
    }

    #[test]
    fn view_masks_the_api_key() {
        let dir = temp_dir();
        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), Some("abcd1234efgh"));
        assert_eq!(view["api_key_masked"], json!("abcd******efgh"));
        assert_eq!(view["api_key_set"], json!(true));

        let short = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), Some("abc"));
        assert_eq!(short["api_key_masked"], json!("***"));
        assert_eq!(short["api_key_set"], json!(true));
    }

    #[test]
    fn view_folds_legacy_flat_limits_in_memory_only() {
        let dir = temp_dir();
        write_settings_file(
            dir.path(),
            &json!({"daily_token_limit": 1234, "expiring_window_days": 3}),
        );
        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), None);
        assert_eq!(view["daily_token_limit"], json!(1234));
        assert_eq!(view["limits"]["daily_token_limit"]["global"], json!(1234));
        assert_eq!(view["limits"]["expiring_window_days"]["global"], json!(3));
        assert_eq!(view["limits"]["daily_token_limit"]["intl"], json!(null));

        let raw = read_back(dir.path());
        assert_eq!(
            raw["daily_token_limit"],
            json!(1234),
            "读视图是纯读：不把迁移写回文件"
        );
        assert!(raw.get("limits").is_none());
    }

    #[test]
    fn view_normalizes_hidden_pages() {
        let dir = temp_dir();
        write_settings_file(
            dir.path(),
            &json!({"hidden_pages": ["logs", "settings", "Bad-Key", "logs", "tasks"]}),
        );
        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), None);
        assert_eq!(
            view["hidden_pages"],
            json!(["logs", "tasks"]),
            "settings 永远不可隐藏；非法键与重复项被丢掉"
        );
    }

    #[test]
    fn view_lists_api_keys_and_deleted_ones() {
        let dir = temp_dir();
        write_settings_file(
            dir.path(),
            &json!({
                "api_keys": [
                    {"id": "k1", "name": "一号", "key": "sk-1234567890", "realm": "INTL",
                     "models": ["DeepSeek/*"], "enabled": true},
                    {"id": "k2", "name": "已删", "key": "", "deleted_at": "2026/01/01 00:00"}
                ]
            }),
        );
        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), None);
        assert_eq!(view["api_keys"].as_array().unwrap().len(), 1);
        let first = &view["api_keys"][0];
        assert_eq!(first["id"], json!("k1"));
        assert_eq!(first["name"], json!("一号"));
        assert_eq!(first["realm"], json!("intl"));
        assert_eq!(first["models"], json!(["deepseek/*"]));
        assert_eq!(first["enabled"], json!(true));
        assert_eq!(first["masked"], json!("sk-1******7890"));
        assert_eq!(first["source"], json!("panel"));
        assert_eq!(view["deleted_api_keys"].as_array().unwrap().len(), 1);
        assert_eq!(view["deleted_api_keys"][0]["id"], json!("k2"));
        assert_eq!(view["auth_required"], json!(true));
    }

    // -----------------------------------------------------------------------
    // 保存：正常路径
    // -----------------------------------------------------------------------

    #[test]
    fn save_persists_booleans_and_returns_them() {
        let dir = temp_dir();
        let (_, reply) = save_settings(
            dir.path(),
            &json!({
                "accounts_collapsed": true,
                "accounts_separate_tab": true,
                "auto_switch_product": true,
                "daily_chat_web": false,
                "local_web_tools": true,
                "remaining_priority_enabled": true,
                "egress_backoff_enabled": true,
                "update_check_enabled": true,
            }),
        )
        .unwrap();
        assert_eq!(reply["accounts_collapsed"], json!(true));
        assert_eq!(reply["daily_chat_web"], json!(false));

        let doc = read_back(dir.path());
        assert_eq!(doc["accounts_collapsed"], json!(true));
        assert_eq!(doc["daily_chat_web"], json!(false));
        assert_eq!(doc["update_check_enabled"], json!(true));
    }

    #[test]
    fn save_leaves_untouched_keys_alone() {
        let dir = temp_dir();
        write_settings_file(
            dir.path(),
            &json!({"ui_language": "en", "launcher_key": "keep"}),
        );
        save_settings(dir.path(), &json!({"accounts_collapsed": true})).unwrap();
        let doc = read_back(dir.path());
        assert_eq!(doc["ui_language"], json!("en"));
        assert_eq!(doc["launcher_key"], json!("keep"));
    }

    #[test]
    fn save_stores_ui_language() {
        let dir = temp_dir();
        let (_, reply) = save_settings(dir.path(), &json!({"ui_language": "zh-Hant"})).unwrap();
        assert_eq!(reply["ui_language"], json!("zh-Hant"));
        assert_eq!(read_back(dir.path())["ui_language"], json!("zh-Hant"));
    }

    #[test]
    fn save_normalizes_hidden_pages_and_never_hides_settings() {
        let dir = temp_dir();
        let (_, reply) = save_settings(
            dir.path(),
            &json!({"hidden_pages": ["logs", "settings", "tasks", "logs", "9bad"]}),
        )
        .unwrap();
        assert_eq!(
            reply["hidden_pages"],
            json!(["logs", "tasks"]),
            "回包给规范化后的列表"
        );
        assert_eq!(
            read_back(dir.path())["hidden_pages"],
            json!(["logs", "tasks"])
        );
    }

    #[test]
    fn save_stores_pricing_enabled_and_variant_inherit() {
        let dir = temp_dir();
        let (_, reply) = save_settings(
            dir.path(),
            &json!({"pricing_enabled": false, "pricing_variant_inherit": false}),
        )
        .unwrap();
        assert_eq!(reply["pricing_enabled"], json!(false));
        assert_eq!(reply["pricing_variant_inherit"], json!(false));
        assert_eq!(
            reply["pricing_refresh_started"],
            json!(true),
            "出厂默认是开（缺键=开）⇒ 改成关是一次**变化**"
        );

        // 同样的值再存一次 ⇒ 没有变化 ⇒ 不需要触发刷新
        let (_, reply) =
            save_settings(dir.path(), &json!({"pricing_variant_inherit": false})).unwrap();
        assert_eq!(reply["pricing_variant_inherit"], json!(false));
        assert!(
            reply.get("pricing_refresh_started").is_none(),
            "值没变 ⇒ 不需要重启一次刷新"
        );

        let (_, reply) =
            save_settings(dir.path(), &json!({"pricing_variant_inherit": true})).unwrap();
        assert_eq!(reply["pricing_variant_inherit"], json!(true));
        assert_eq!(
            reply["pricing_refresh_started"],
            json!(true),
            "值确实变了 ⇒ 回报需要触发一次刷新"
        );
    }

    #[test]
    fn save_pricing_refresh_minutes_returns_minutes() {
        let dir = temp_dir();
        let (_, reply) =
            save_settings(dir.path(), &json!({"pricing_refresh_minutes": 12})).unwrap();
        assert_eq!(reply["pricing_refresh_minutes"], json!(12.0));
        assert_eq!(
            read_back(dir.path())["pricing_refresh_minutes"],
            json!(12.0)
        );
    }

    #[test]
    fn save_legacy_pricing_refresh_hours_is_multiplied_by_60() {
        let dir = temp_dir();
        let (_, reply) = save_settings(dir.path(), &json!({"pricing_refresh_hours": 6})).unwrap();
        assert_eq!(
            reply["pricing_refresh_minutes"],
            json!(360.0),
            "旧名是小时 ⇒ ×60"
        );
        let doc = read_back(dir.path());
        assert_eq!(doc["pricing_refresh_minutes"], json!(360.0));
        assert!(
            doc.get("pricing_refresh_hours").is_none(),
            "旧键被删掉，只留新键"
        );
    }

    #[test]
    fn save_credits_refresh_hours_is_clamped() {
        let dir = temp_dir();
        let (_, reply) = save_settings(dir.path(), &json!({"credits_refresh_hours": 6})).unwrap();
        assert_eq!(reply["credits_refresh_hours"], json!(6.0));
        let (_, reply) =
            save_settings(dir.path(), &json!({"credits_refresh_hours": 10000})).unwrap();
        assert_eq!(
            reply["credits_refresh_hours"],
            json!(720.0),
            "上限 720 小时"
        );
    }

    #[test]
    fn save_limits_writes_grouped_shape_and_returns_snapshot() {
        let dir = temp_dir();
        let (_, reply) = save_settings(
            dir.path(),
            &json!({"limits": {"daily_token_limit": {"global": 1000, "intl": 2000}}}),
        )
        .unwrap();
        assert_eq!(reply["daily_token_limit"], json!(1000), "回全局值");
        assert_eq!(
            reply["limits"]["daily_token_limit"],
            json!({"global": 1000, "intl": 2000, "cn": null})
        );
        let doc = read_back(dir.path());
        assert_eq!(
            doc["limits"]["daily_token_limit"],
            json!({"global": 1000, "intl": 2000, "cn": null}),
            "落盘的是完整的分组形状"
        );
        assert_eq!(doc["limits"]["expiring_window_days"]["global"], json!(7));
    }

    #[test]
    fn save_limits_per_realm_falls_back_to_global() {
        let dir = temp_dir();
        save_settings(
            dir.path(),
            &json!({"limits": {"daily_credit_limit": {"global": 50}}}),
        )
        .unwrap();
        let doc = read_back(dir.path());
        assert_eq!(
            doc["limits"]["daily_credit_limit"],
            json!({"global": 50, "intl": null, "cn": null}),
            "没有覆盖 ⇒ 存 null（= 继承全局）"
        );

        let limits = limits_from_doc(doc.as_object().unwrap());
        assert_eq!(
            limit_value(&limits, "daily_credit_limit", Some("intl")),
            50,
            "intl 没有覆盖 ⇒ 生效值回落到全局"
        );
        assert_eq!(limit_value(&limits, "daily_credit_limit", None), 50);
    }

    #[test]
    fn save_limits_blank_realm_override_clears_back_to_global() {
        let dir = temp_dir();
        save_settings(
            dir.path(),
            &json!({"limits": {"daily_credit_limit": {"global": 50, "intl": 7}}}),
        )
        .unwrap();
        let (_, reply) = save_settings(
            dir.path(),
            &json!({"limits": {"daily_credit_limit": {"intl": ""}}}),
        )
        .unwrap();
        assert_eq!(
            reply["limits"]["daily_credit_limit"],
            json!({"global": 50, "intl": null, "cn": null}),
            "空串 ⇒ 清掉覆盖，回到继承全局"
        );
    }

    #[test]
    fn save_limits_folds_legacy_flat_keys() {
        let dir = temp_dir();
        write_settings_file(dir.path(), &json!({"daily_token_limit": 1234}));
        save_settings(
            dir.path(),
            &json!({"limits": {"reserve_credits": {"global": 5}}}),
        )
        .unwrap();
        let doc = read_back(dir.path());
        assert!(
            doc.get("daily_token_limit").is_none(),
            "旧版扁平键被折叠进 limits 并从文件里删掉"
        );
        assert_eq!(doc["limits"]["daily_token_limit"]["global"], json!(1234));
        assert_eq!(doc["limits"]["reserve_credits"]["global"], json!(5));
    }

    #[test]
    fn save_api_key_sets_flag_and_reports_it() {
        let dir = temp_dir();
        let (settings, reply) =
            save_settings(dir.path(), &json!({"api_key": "sk-abcdefg"})).unwrap();
        assert_eq!(reply["api_key_set"], json!(true));
        assert_eq!(settings["api_key"], json!("sk-abcdefg"));
        assert_eq!(settings["api_key_set"], json!(true));
    }

    #[test]
    fn null_api_key_and_null_limits_are_treated_as_absent() {
        // Python: `payload.get("api_key")` 为 None ⇒ 整个分支跳过；
        // `payload.get("limits")` 为 None ⇒ 限额分支也跳过。
        let dir = temp_dir();
        let (settings, reply) =
            save_settings(dir.path(), &json!({"api_key": null, "limits": null})).unwrap();
        assert!(settings.get("api_key").is_none(), "null 的 api_key 不写盘");
        assert!(reply.get("api_key_set").is_none());
        assert!(reply.get("limits").is_none());
    }

    #[test]
    fn save_api_key_empty_string_clears_the_key() {
        let dir = temp_dir();
        let (settings, reply) = save_settings(dir.path(), &json!({"api_key": ""})).unwrap();
        assert_eq!(reply["api_key_set"], json!(false));
        assert_eq!(settings["api_key"], json!(""));
        assert_eq!(settings["api_key_set"], json!(true));
    }

    // -----------------------------------------------------------------------
    // 保存：400 校验路径
    // -----------------------------------------------------------------------

    #[test]
    fn unsupported_key_is_rejected_instead_of_silently_ignored() {
        let dir = temp_dir();
        for key in [
            "api_keys",
            "upstream",
            "prompt",
            "auth_disabled",
            "restart_scheduler",
            "daily_token_limit",
            "key_before_hidden",
        ] {
            let (status, message) = save_settings(dir.path(), &json!({ key: 1 })).unwrap_err();
            assert_eq!(status, 400, "「{key}」应当 400");
            assert!(message.contains(key), "错误文案要点名这个键：{message}");
        }
        assert!(
            !dir.path().join("settings.json").exists(),
            "被拒的保存不落盘"
        );
    }

    #[test]
    fn boolean_switches_require_a_real_boolean() {
        let dir = temp_dir();
        for key in [
            "pricing_enabled",
            "pricing_variant_inherit",
            "auto_switch_product",
            "daily_chat_web",
            "local_web_tools",
            "remaining_priority_enabled",
            "egress_backoff_enabled",
            "accounts_collapsed",
            "accounts_separate_tab",
            "update_check_enabled",
        ] {
            for bad in [json!("false"), json!(1), json!(0), json!(null)] {
                let (status, message) =
                    save_settings(dir.path(), &json!({ key: bad.clone() })).unwrap_err();
                assert_eq!(status, 400, "「{key} = {bad}」应当 400");
                assert_eq!(message, format!("{key} must be true or false"));
            }
        }
    }

    #[test]
    fn ui_language_accepts_only_three_values() {
        let dir = temp_dir();
        for bad in [json!("fr"), json!(""), json!(1), json!(null), json!(["zh"])] {
            let (status, message) =
                save_settings(dir.path(), &json!({"ui_language": bad})).unwrap_err();
            assert_eq!(status, 400);
            assert_eq!(message, "ui_language must be zh, zh-Hant or en");
        }
    }

    #[test]
    fn hidden_pages_requires_a_list_of_strings() {
        let dir = temp_dir();
        for bad in [json!("logs"), json!(5), json!({"a": 1}), json!(["logs", 5])] {
            let (status, message) =
                save_settings(dir.path(), &json!({"hidden_pages": bad})).unwrap_err();
            assert_eq!(status, 400);
            assert_eq!(message, "hidden_pages must be a list of page keys");
        }
    }

    #[test]
    fn limits_must_be_an_object() {
        let dir = temp_dir();
        let (status, message) = save_settings(dir.path(), &json!({"limits": 5})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "limits must be an object");
    }

    #[test]
    fn limit_values_reject_booleans_and_negatives() {
        let dir = temp_dir();
        let (status, message) = save_settings(
            dir.path(),
            &json!({"limits": {"daily_token_limit": {"global": true}}}),
        )
        .unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "daily_token_limit must be a whole number");

        let (status, message) = save_settings(
            dir.path(),
            &json!({"limits": {"daily_token_limit": {"global": -1}}}),
        )
        .unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "daily_token_limit cannot be negative");

        let (status, message) = save_settings(
            dir.path(),
            &json!({"limits": {"daily_token_limit": {"intl": true}}}),
        )
        .unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "daily_token_limit must be a whole number");

        let (status, message) = save_settings(
            dir.path(),
            &json!({"limits": {"daily_token_limit": {"intl": -5}}}),
        )
        .unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "daily_token_limit cannot be negative");
    }

    #[test]
    fn pricing_refresh_minutes_rejects_non_numbers_and_negatives() {
        let dir = temp_dir();
        let (status, message) =
            save_settings(dir.path(), &json!({"pricing_refresh_minutes": "abc"})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "pricing_refresh_minutes must be a number");

        let (status, message) =
            save_settings(dir.path(), &json!({"pricing_refresh_minutes": true})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "pricing_refresh_minutes must be a number");

        let (status, message) =
            save_settings(dir.path(), &json!({"pricing_refresh_minutes": -1})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(
            message, "pricing_refresh_minutes cannot be negative",
            "负数的文案用的是新键名（与 Python 一致）"
        );

        let (status, message) =
            save_settings(dir.path(), &json!({"pricing_refresh_hours": "abc"})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(
            message, "pricing_refresh_hours must be a number",
            "旧名的报错文案用旧键名（与 Python 一致）"
        );
    }

    #[test]
    fn credits_refresh_hours_rejects_non_numbers_and_negatives() {
        let dir = temp_dir();
        let (status, message) =
            save_settings(dir.path(), &json!({"credits_refresh_hours": "abc"})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "credits_refresh_hours must be a number");

        let (status, message) =
            save_settings(dir.path(), &json!({"credits_refresh_hours": -1})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "credits_refresh_hours cannot be negative");
    }

    #[test]
    fn api_key_needs_at_least_four_characters() {
        let dir = temp_dir();
        let (status, message) = save_settings(dir.path(), &json!({"api_key": "abc"})).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(message, "api key must be at least 4 characters");
    }

    // -----------------------------------------------------------------------
    // 写入口径
    // -----------------------------------------------------------------------

    #[test]
    fn save_overwrites_a_broken_settings_file_like_python() {
        let dir = temp_dir();
        fs::write(dir.path().join("settings.json"), "{ not json").unwrap();
        save_settings(dir.path(), &json!({"accounts_collapsed": true})).unwrap();
        let doc = read_back(dir.path());
        assert_eq!(doc["accounts_collapsed"], json!(true));
    }

    #[test]
    fn save_can_delete_a_key_by_rewriting_the_whole_document() {
        // 整体替换 ⇒ 能删键（浅合并做不到）。
        let dir = temp_dir();
        write_settings_file(
            dir.path(),
            &json!({"panel_password_hash": "x", "panel_password_salt": "y"}),
        );
        let mut doc = read_back(dir.path());
        let map = doc.as_object_mut().unwrap();
        map.remove("panel_password_salt");
        wb_storage::replace_settings(dir.path(), &doc).unwrap();
        let after = read_back(dir.path());
        assert!(after.get("panel_password_salt").is_none());
        assert_eq!(after["panel_password_hash"], json!("x"));
    }

    /// 与 Python 逐值对拍（fixture 的期望值是跑 Python `wb_settings.py` 得到的）。
    ///
    /// fixture 刻意覆盖了这些坑：旧的扁平限额键与新的 `limits` 对象**同时**存在
    /// （Python：有 `limits` 就**忽略**扁平键）、`realm` 大小写、模型白名单的
    /// 字符串形态、重复的 key id、已删除的 key、`settings` 这个不能藏的页面、
    /// `upstream` / `prompt` 的非法值。
    #[test]
    fn view_matches_the_python_reference_values() {
        let dir = temp_dir();
        write_settings_file(
            dir.path(),
            &json!({
                "api_key_set": true,
                "api_key": "sk-legacy-123456",
                "credits_refresh_hours": 6,
                "pricing_refresh_minutes": 12.5,
                "ui_language": "zh-Hant",
                "daily_chat_web": false,
                "local_web_tools": true,
                "remaining_priority_enabled": true,
                "egress_backoff_enabled": false,
                "accounts_collapsed": true,
                "accounts_separate_tab": false,
                "key_before_hidden": true,
                "hidden_pages": ["logs", "settings", "Bad", "logs", "tasks", "a_b-c9"],
                "update_check_enabled": true,
                "pricing_variant_inherit": false,
                "pricing_enabled": false,
                "auto_switch_product": true,
                "upstream": {
                    "header_timeout_seconds": 45,
                    "idle_timeout_seconds": 0,
                    "device_token": "  tok  ",
                    "device_token_file": 7
                },
                "prompt": {"mode": "CUSTOM", "file": " /tmp/p.txt "},
                "api_keys": [
                    {"id": "k1", "name": "一号", "key": "sk-1234567890", "realm": "INTL",
                     "models": ["DeepSeek/*", "gpt-4"], "enabled": true,
                     "created_at": "2026/01/01 10:00"},
                    {"id": "k2", "name": "二号", "key": "sk-duplicate", "realm": "xx",
                     "models": "a,b;c", "enabled": false},
                    {"id": "k3", "name": "已删", "key": "", "deleted_at": "2026/02/02 11:00"},
                    {"id": "k1", "name": "重复 id", "key": "sk-other", "realm": "",
                     "models": [], "enabled": true}
                ],
                "limits": {
                    "daily_token_limit": {"global": 1000, "intl": 2000},
                    "reserve_credits": {"global": "50"}
                },
                "daily_credit_limit": 77,
                "panel_password_hash": "abc",
                "panel_password_default": true
            }),
        );

        let view = runtime_settings_view(dir.path(), Path::new("/tmp/usage"), Some("sk-abcdefgh"));

        assert_eq!(view["panel_password_is_default"], json!(true));
        assert_eq!(view["api_key_set"], json!(true));
        assert_eq!(view["api_key_set_by_panel"], json!(true));
        assert_eq!(view["api_key_masked"], json!("sk-a******efgh"));
        assert_eq!(view["auth_required"], json!(true));

        // Python: limits_snapshot()
        assert_eq!(
            view["limits"],
            json!({
                "reserve_credits": {"global": 50, "intl": null, "cn": null},
                "daily_token_limit": {"global": 1000, "intl": 2000, "cn": null},
                "daily_credit_limit": {"global": 0, "intl": null, "cn": null},
                "model_daily_token_limit": {"global": 0, "intl": null, "cn": null},
                "expiring_window_days": {"global": 7, "intl": null, "cn": null},
            }),
            "有 limits 对象 ⇒ 旧的扁平键被忽略（Python 同）"
        );
        assert_eq!(view["reserve_credits"], json!(50));
        assert_eq!(view["daily_token_limit"], json!(1000));
        assert_eq!(view["daily_credit_limit"], json!(0));

        // Python: hidden_pages()
        assert_eq!(view["hidden_pages"], json!(["logs", "tasks", "a_b-c9"]));

        // Python: api_keys()
        assert_eq!(view["api_keys"].as_array().unwrap().len(), 3);
        assert_eq!(view["api_keys"][0]["id"], json!("k1"));
        assert_eq!(view["api_keys"][0]["realm"], json!("intl"));
        assert_eq!(
            view["api_keys"][0]["models"],
            json!(["deepseek/*", "gpt-4"])
        );
        assert_eq!(view["api_keys"][0]["masked"], json!("sk-1******7890"));
        assert_eq!(view["api_keys"][0]["created_at"], json!("2026/01/01 10:00"));
        assert_eq!(view["api_keys"][1]["models"], json!(["a", "b", "c"]));
        assert_eq!(
            view["api_keys"][1]["realm"],
            json!(""),
            "非法 realm 归到空串"
        );
        assert_eq!(
            view["api_keys"][2]["id"],
            json!("k1-2"),
            "重复的 id 追加 -2（Python _unique_key_id）"
        );
        assert_eq!(view["deleted_api_keys"].as_array().unwrap().len(), 1);
        assert_eq!(view["deleted_api_keys"][0]["id"], json!("k3"));

        // Python: upstream_config() / prompt_config()
        assert_eq!(
            view["upstream"],
            json!({
                "header_timeout_seconds": 45,
                "idle_timeout_seconds": 300,
                "device_token": "tok",
                "device_token_file": ""
            }),
            "idle_timeout_seconds=0 非法 ⇒ 回落默认 300；device_token_file 非字符串 ⇒ 默认"
        );
        assert_eq!(
            view["prompt"],
            json!({"mode": "custom", "file": "/tmp/p.txt"}),
            "mode 转小写、file 去空白"
        );

        // Python: 各 getter
        assert_eq!(view["pricing_refresh_minutes"], json!(12.5));
        assert_eq!(view["credits_refresh_hours"], json!(6.0));
        assert_eq!(view["ui_language"], json!("zh-Hant"));
        assert_eq!(view["pricing_variant_inherit"], json!(false));
        assert_eq!(view["pricing_enabled"], json!(false));
        assert_eq!(view["auto_switch_product"], json!(true));
        assert_eq!(view["daily_chat_web"], json!(false));
        assert_eq!(view["local_web_tools"], json!(true));
        assert_eq!(view["remaining_priority_enabled"], json!(true));
        assert_eq!(view["egress_backoff_enabled"], json!(false));
        assert_eq!(view["accounts_collapsed"], json!(true));
        assert_eq!(view["accounts_separate_tab"], json!(false));
        assert_eq!(view["key_before_hidden"], json!(true));
        assert_eq!(view["update_check_enabled"], json!(true));
    }
}
