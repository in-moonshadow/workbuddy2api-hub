//! 只读访问 Python 版本留下的 legacy 数据目录。

/// 一个账号快照的聚合信息（M1 只需要计数与 realm）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub realm: String,
    pub accounts: usize,
    pub accounts_ready: usize,
    pub api_key_required: bool,
}

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 与 Python 侧相同：这几个文件不是账号。
const NON_ACCOUNT_FILES: [&str; 3] = ["settings.json", "model.json", "active_realm.json"];

#[derive(Debug)]
pub enum StorageError {
    Io(PathBuf, std::io::Error),
    Json(PathBuf, serde_json::Error),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Io(path, err) => write!(f, "{}: {err}", path.display()),
            StorageError::Json(path, err) => write!(f, "{}: {err}", path.display()),
        }
    }
}

impl std::error::Error for StorageError {}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>, StorageError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(StorageError::Io(path.to_path_buf(), err)),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| StorageError::Json(path.to_path_buf(), err))
}

/// 实例级默认界面语言，取值与 Python 的 wb_settings.ui_language 一致。
pub fn ui_language(accounts_dir: &Path) -> String {
    const VALID: [&str; 3] = ["zh", "zh-Hant", "en"];
    let value = read_json(&accounts_dir.join("settings.json"))
        .ok()
        .flatten()
        .and_then(|doc| {
            doc.get("ui_language")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    match value {
        Some(lang) if VALID.contains(&lang.as_str()) => lang,
        _ => "zh".to_string(),
    }
}

fn read_realm(accounts_dir: &Path) -> Result<String, StorageError> {
    let doc = read_json(&accounts_dir.join("active_realm.json"))?;
    let realm = doc.and_then(|d| d.get("realm").and_then(|v| v.as_str()).map(str::to_string));
    Ok(match realm.as_deref() {
        Some("cn") => "cn".to_string(),
        _ => "intl".to_string(),
    })
}

/// 落盘的 realm 原值，供 `/realm` GET 展示（Python `persisted_realm()`）。
///
/// 与 `read_realm` 的区别：这里**不套默认值**，文件缺失/值非法时返回 `None`，
/// 让调用方能区分「用户显式选过 intl」与「从未选过」。
pub fn read_active_realm(accounts_dir: &Path) -> Option<String> {
    read_json(&accounts_dir.join("active_realm.json"))
        .ok()
        .flatten()
        .and_then(|doc| {
            doc.get("realm")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .filter(|realm| realm == "cn" || realm == "intl")
}

/// 池装载所需的账号视图（比 `Account` 多 `product` 与 `has_token`）。
///
/// 只在进程内传递 `access_token`，**不得写入日志**。`has_token` 与
/// `load_snapshot` 的 `is_ready` 判定一致（`accessToken` 非空）。
#[derive(Debug, Clone, PartialEq)]
pub struct PoolAccount {
    pub uid: String,
    pub realm: String,
    /// 账号所属产品线（Python `Account.product`；缺省为空串）。
    pub product: String,
    pub has_token: bool,
    pub access_token: String,
}

/// 枚举可装载进池的账号（对齐 Python `AccountPool._load` 的账号清单）。
///
/// **不做 enabled/cooldown 过滤**：那些是池运行期状态（`AccountState` 自己
/// 持有 enabled/cooldown_until），Python 也是全部载入后由 `ready()` 判。
pub fn pool_accounts(accounts_dir: &Path) -> Result<Vec<PoolAccount>, StorageError> {
    let mut out = Vec::new();
    for (_, doc) in account_entries(accounts_dir)? {
        let access_token = doc
            .get("accessToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(PoolAccount {
            uid: doc
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            realm: doc
                .get("realm")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            product: doc
                .get("product")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            has_token: !access_token.is_empty(),
            access_token,
        });
    }
    Ok(out)
}

/// 把上游出口选择落盘到 `<accounts_dir>/active_realm.json`（对齐 Python
/// `save_persisted_realm` :5684-5690，写 `{"realm": <值>}`）。
///
/// 只接受 "cn" / "intl"：其他值一律忽略（Python 的 `_route_realm` 也是
/// `if new_realm in ("intl", "cn")` 才落盘，返回值恒为 200 + persisted=true）。
/// 与 `read_realm` 共用同一个键名，保证往返一致。
///
/// 注意：本函数只写文件，**不改变进程内的 CURRENT_REALM** —— 那与 Python
/// 行为一致（realm 变更需重启进程才生效）。
pub fn save_realm(accounts_dir: &Path, realm: &str) -> Result<bool, StorageError> {
    if realm != "cn" && realm != "intl" {
        return Ok(false);
    }
    let path = accounts_dir.join("active_realm.json");
    let text = serde_json::json!({ "realm": realm }).to_string();
    fs::write(&path, text).map_err(|err| StorageError::Io(path, err))?;
    Ok(true)
}

fn unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// M1 的 ready 判定是 Python `Account.ready()` 的保守近似：已启用 + 有 token +
/// 未在 cooldown + 无残留错误。M2 引入 identity crate 后补齐 park/breaker/degrade
/// 等状态位。
fn is_ready(account: &serde_json::Value, now: f64) -> bool {
    let enabled = account
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let token = account
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let cooldown = account
        .get("cooldownUntil")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let last_error = account
        .get("lastError")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    enabled && !token.is_empty() && cooldown <= now && last_error.is_empty()
}

/// 是否需要 API key（对齐 Python wb_proxy.auth_required :420-426）。
///
/// Python 的 `auth_disabled` 优先，其次面板 api_keys[] 有 enabled 项，最后
/// `bool(API_KEY)`。launcher 模式会把生成 key 持久化进 settings.json 的
/// launcher_key 并在启动时灌入全局 API_KEY —— 影子只读同一份 settings.json，
/// 所以 launcher_key 非空 ⇔ 真实实例此刻要求 key；漏掉它会把「要求 key」的
/// 实例误报为「不要求」。
pub fn auth_required(accounts_dir: &Path, api_key_env: Option<&str>, lan: bool) -> bool {
    if lan || api_key_env.map(|k| !k.is_empty()).unwrap_or(false) {
        return true;
    }
    let doc = match read_json(&accounts_dir.join("settings.json")) {
        Ok(Some(doc)) => doc,
        _ => return false,
    };
    if doc
        .get("auth_disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    if let Some(keys) = doc.get("api_keys").and_then(|v| v.as_array()) {
        if keys
            .iter()
            .any(|k| k.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
        {
            return true;
        }
    }
    if doc
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(|k| !k.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    doc.get("launcher_key")
        .and_then(|v| v.as_str())
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// 汇总 legacy 账号目录：账号数、就绪数、当前 realm、是否需要 API key。
pub fn load_snapshot(
    accounts_dir: &Path,
    api_key_env: Option<&str>,
    lan: bool,
) -> Result<Snapshot, StorageError> {
    let realm = read_realm(accounts_dir)?;
    let now = unix_secs();
    let mut accounts = 0usize;
    let mut accounts_ready = 0usize;
    for (_, doc) in account_entries(accounts_dir)? {
        accounts += 1;
        if is_ready(&doc, now) {
            accounts_ready += 1;
        }
    }
    let api_key_required = auth_required(accounts_dir, api_key_env, lan);
    Ok(Snapshot {
        realm,
        accounts,
        accounts_ready,
        api_key_required,
    })
}

/// 代表账号的视图（对齐 Python `Account.public()` 的身份子集）。
///
/// M2 阶段只暴露 /health 授权时需要的字段；完整 public() 视图（proxy/
/// credits/modelCooldowns 等）随 gateway 里程碑补齐。
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub uid: String,
    pub domain: String,
    pub realm: String,
    pub access_token: String,
    pub expires_at: i64,
    pub credential_file: String,
}

/// 遍历账号目录，返回 (文件名, JSON 文档) 列表；按文件枚举顺序（对齐 Python
/// `AccountPool._load` 的加载顺序，representative 依赖它取「第一个」）。
fn account_entries(accounts_dir: &Path) -> Result<Vec<(String, serde_json::Value)>, StorageError> {
    let entries = fs::read_dir(accounts_dir)
        .map_err(|err| StorageError::Io(accounts_dir.to_path_buf(), err))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| StorageError::Io(accounts_dir.to_path_buf(), err))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if NON_ACCOUNT_FILES.contains(&name.as_str()) {
            continue;
        }
        let doc = match read_json(&path)? {
            Some(doc) if doc.is_object() => doc,
            _ => continue,
        };
        out.push((name, doc));
    }
    Ok(out)
}

/// 代表账号：第一个有 access_token 的账号，否则第一个，池空则 None
/// （对齐 Python `AccountPool.representative()` :3154-3159，供 /health
/// 授权身份字段使用）。
pub fn representative(accounts_dir: &Path, realm: &str) -> Option<Account> {
    let entries = account_entries(accounts_dir).ok()?;
    let mut first_with_token: Option<Account> = None;
    let mut first: Option<Account> = None;
    for (name, doc) in entries {
        let account_realm = doc.get("realm").and_then(|v| v.as_str()).unwrap_or("");
        if !realm.is_empty() && account_realm != realm {
            continue;
        }
        let view = Account {
            uid: doc
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            domain: doc
                .get("domain")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            realm: account_realm.to_string(),
            access_token: doc
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            expires_at: doc.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0),
            credential_file: name,
        };
        if first.is_none() {
            first = Some(view.clone());
        }
        if !view.access_token.is_empty() {
            first_with_token = Some(view);
            break;
        }
    }
    first_with_token.or(first)
}

/// 生效的 API key（对齐 Python wb_proxy.py:18722-18737 的三来源优先级）：
/// ① 环境变量 API_KEY（对应 --api-key）
/// ② 面板保存的 settings.json api_key_set+api_key（胜出于 launcher key）
/// ③ LAN 模式生成的 settings.json launcher_key
///
/// 面板声明了管理权（api_key_set=true）但 api_key 为空时，视为未配置、不回落
/// 到 launcher key（Python 用空串覆盖 API_KEY 的等价语义）。
pub fn effective_api_key(accounts_dir: &Path, env_api_key: Option<&str>) -> Option<String> {
    let env = env_api_key.map(str::trim).unwrap_or("");
    if !env.is_empty() {
        return Some(env.to_string());
    }
    let doc = match read_json(&accounts_dir.join("settings.json")) {
        Ok(Some(doc)) => doc,
        _ => return None,
    };
    if doc
        .get("api_key_set")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let saved = doc
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        return if saved.is_empty() {
            None
        } else {
            Some(saved.to_string())
        };
    }
    let launcher = doc
        .get("launcher_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if launcher.is_empty() {
        None
    } else {
        Some(launcher.to_string())
    }
}

/// 一个 API key 条目（对齐 Python wb_settings._clean_key_entry 的返回 dict）。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEntry {
    pub id: String,
    pub name: String,
    pub key: String,
    pub realm: String,
    pub models: Vec<String>,
    pub enabled: bool,
    /// 非空表示已删除（面板保留条目供 UI 展示，匹配端跳过）。
    pub deleted_at: String,
    /// panel：settings.json api_keys[]（含 legacy 合成条目）；launcher：extra_keys。
    pub source: String,
}

/// 常数时间字符串比较，等价 Python `hmac.compare_digest` 的 ASCII 行为：长度
/// 不同立即 False（不保证常数时间，与 CPython 一致），长度相同则逐字节异或
/// 防御时序侧信道。M2-a 影子实例威胁面低，不引入 subtle 依赖。
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 读 settings.json 的 api_keys[]，兼容旧格式 api_key_set+api_key（合成一个
/// legacy 条目）。对齐 wb_settings.api_keys（:381-424）+ _clean_key_entry
/// （:326-340）：非对象条目跳过；无 key 且无 deleted_at 的条目跳过。
pub fn api_keys(accounts_dir: &Path) -> Vec<KeyEntry> {
    let doc = match read_json(&accounts_dir.join("settings.json")) {
        Ok(Some(doc)) => doc,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let legacy_configured = doc
        .get("api_key_set")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if legacy_configured {
        if let Some(key) = doc.get("api_key").and_then(|v| v.as_str()) {
            out.push(KeyEntry {
                id: "legacy".to_string(),
                name: "Legacy Key".to_string(),
                key: key.to_string(),
                realm: String::new(),
                models: Vec::new(),
                enabled: true,
                deleted_at: String::new(),
                source: "panel".to_string(),
            });
        }
    }
    if let Some(keys) = doc.get("api_keys").and_then(|v| v.as_array()) {
        for entry in keys {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let key = obj
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let deleted_at = obj.get("deleted_at").and_then(|v| v.as_str()).unwrap_or("");
            if key.is_empty() && deleted_at.is_empty() {
                continue;
            }
            let models = obj
                .get("models")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            out.push(KeyEntry {
                id: obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                key,
                realm: obj
                    .get("realm")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                models,
                enabled: obj.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                deleted_at: deleted_at.to_string(),
                source: "panel".to_string(),
            });
        }
    }
    out
}

/// 用提供的 key 在面板 key 表与 extra_keys 中做常数时间匹配。
///
/// 对齐 wb_settings.match_api_key（:526-554）：
/// - supplied 空/纯空白 → None
/// - 面板 api_keys[]：enabled 且未删除且 compare_digest 命中 → source="panel"
/// - extra_keys（配置面板 key 前的 API_KEY 等价物）：compare_digest 命中 →
///   source="launcher" 的合成条目
pub fn match_api_key(
    accounts_dir: &Path,
    supplied_key: &str,
    extra_keys: &[&str],
) -> Option<KeyEntry> {
    let supplied = supplied_key.trim();
    if supplied.is_empty() {
        return None;
    }
    for entry in api_keys(accounts_dir) {
        if !entry.enabled || !entry.deleted_at.is_empty() {
            continue;
        }
        if constant_time_eq(supplied, &entry.key) {
            return Some(entry);
        }
    }
    for key in extra_keys {
        if key.is_empty() {
            continue;
        }
        if constant_time_eq(supplied, key) {
            return Some(KeyEntry {
                id: "launcher".to_string(),
                name: "Launcher Key".to_string(),
                key: (*key).to_string(),
                realm: String::new(),
                models: Vec::new(),
                enabled: true,
                deleted_at: String::new(),
                source: "launcher".to_string(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_account(dir: &Path, name: &str, enabled: bool) {
        let body = serde_json::json!({
            "uid": name, "realm": "cn", "enabled": enabled,
            "accessToken": "h.p.s", "cooldownUntil": 0.0, "lastError": ""
        });
        fs::write(dir.join(format!("{name}.json")), body.to_string()).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("active_realm.json"), r#"{"realm":"cn"}"#).unwrap();
        fs::write(dir.path().join("settings.json"), r#"{"ui_language":"en"}"#).unwrap();
        write_account(dir.path(), "a", true);
        write_account(dir.path(), "b", false);
        dir
    }

    #[test]
    fn snapshot_counts_only_account_files() {
        let dir = fixture();
        let snap = load_snapshot(dir.path(), None, false).unwrap();
        assert_eq!(snap.realm, "cn");
        assert_eq!(snap.accounts, 2);
        assert_eq!(snap.accounts_ready, 1);
        assert!(!snap.api_key_required);
    }

    #[test]
    fn missing_realm_falls_back_to_intl() {
        let dir = tempfile::tempdir().unwrap();
        let snap = load_snapshot(dir.path(), None, false).unwrap();
        assert_eq!(snap.realm, "intl");
        assert_eq!(snap.accounts, 0);
    }

    #[test]
    fn lan_or_env_key_requires_a_key() {
        let dir = fixture();
        assert!(
            load_snapshot(dir.path(), None, true)
                .unwrap()
                .api_key_required
        );
        assert!(
            load_snapshot(dir.path(), Some("k"), false)
                .unwrap()
                .api_key_required
        );
    }

    #[test]
    fn language_defaults_to_zh_when_absent_or_invalid() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ui_language(dir.path()), "zh");
        fs::write(dir.path().join("settings.json"), r#"{"ui_language":"fr"}"#).unwrap();
        assert_eq!(ui_language(dir.path()), "zh");
    }

    #[test]
    fn language_reads_valid_value() {
        let dir = fixture();
        assert_eq!(ui_language(dir.path()), "en");
    }

    #[test]
    fn launcher_key_requires_a_key() {
        // 复刻真实 LAN 实例：settings.json 只有 launcher_key，没有 api_keys/
        // api_key。Python 的 auth_required() 靠启动时灌入的全局 API_KEY 判为
        // true，影子读同一份文件也必须为 true，否则会把「要求 key」误报为
        // 「不要求」。
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("active_realm.json"), r#"{"realm":"intl"}"#).unwrap();
        fs::write(
            dir.path().join("settings.json"),
            r#"{"launcher_key":"wb-REDACTED_SAMPLE"}"#,
        )
        .unwrap();
        let snap = load_snapshot(dir.path(), None, false).unwrap();
        assert!(snap.api_key_required);
    }

    #[test]
    fn auth_disabled_wins_over_launcher_key() {
        // auth_disabled:true 时无论 launcher_key 是否存在都不要求 key，与 Python
        // auth_required() 先判 auth_disabled 的顺序一致。
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("settings.json"),
            r#"{"auth_disabled":true,"launcher_key":"wb-REDACTED_SAMPLE"}"#,
        )
        .unwrap();
        let snap = load_snapshot(dir.path(), None, false).unwrap();
        assert!(!snap.api_key_required);
    }
}
