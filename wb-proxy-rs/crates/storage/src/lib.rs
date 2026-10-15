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

fn api_key_configured(accounts_dir: &Path, api_key_env: Option<&str>, lan: bool) -> bool {
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
    doc.get("api_key")
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
    let entries = fs::read_dir(accounts_dir)
        .map_err(|err| StorageError::Io(accounts_dir.to_path_buf(), err))?;
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
        accounts += 1;
        if is_ready(&doc, now) {
            accounts_ready += 1;
        }
    }
    let api_key_required = api_key_configured(accounts_dir, api_key_env, lan);
    Ok(Snapshot {
        realm,
        accounts,
        accounts_ready,
        api_key_required,
    })
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
}
