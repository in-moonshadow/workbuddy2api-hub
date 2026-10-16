//! 导入行归一化：把各种来源的行转成统一的账号字段集。
//!
//! 移植自 wb_accounts.py `_coerce_account_rows`（:3537-3572）与
//! `normalise_import_row`（:3575-3650）。纯函数、无副作用——不写任何文件，
//! 符合 M2-a「影子只读 / dry-run」范围。AccountPool.import_rows 的写池路径
//! （get/add/apply_proxy_slots）属 M2-b/M5，不在本模块。

use crate::jwt::{jwt_exp, jwt_uid, normalize_epoch};
use crate::realm::{detect_realm_from_token, domain_for_realm, realm_evidence};
use serde_json::Value;

/// 归一化后的账号字段（对齐 Python normalise_import_row 的返回 dict）。
#[derive(Debug, Clone, PartialEq)]
pub struct AccountFields {
    pub uid: String,
    pub nickname: String,
    pub domain: String,
    pub realm: String,
    pub platform: String,
    pub enterprise_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub source: String,
    pub enabled: bool,
    /// volatile 状态有意重置（对齐 VOLATILE_FIELDS 语义）。
    pub last_error: String,
    pub cooldown_until: f64,
    /// portable 导出需要的补充字段（product/addedAt/weight/note）。
    pub product: String,
    pub added_at: i64,
    pub weight: i64,
    pub note: String,
}

/// 从行的任意层读字段（扁平 / auth / account 三层），空值跳过。
fn pick<'a>(
    row: &'a Value,
    auth: Option<&'a Value>,
    profile: Option<&'a Value>,
    key: &str,
) -> Option<String> {
    for layer in [Some(row), auth, profile].into_iter().flatten() {
        if let Some(v) = layer.get(key) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// 把 cockpit 的 snake_case 行映射到扁平形状（对齐 :3585-3595）。
fn map_cockpit_row(row: &Value) -> Value {
    let mut mapped = serde_json::Map::new();
    for (dst, src) in [
        ("uid", "uid"),
        ("nickname", "nickname"),
        ("domain", "domain"),
        ("accessToken", "access_token"),
        ("refreshToken", "refresh_token"),
        ("expiresAt", "expires_at"),
    ] {
        if let Some(v) = row.get(src) {
            mapped.insert(dst.to_string(), v.clone());
        }
    }
    if row.get("email").is_some()
        && mapped
            .get("nickname")
            .is_none_or(|v| v.as_str().is_none_or(str::is_empty))
    {
        mapped.insert(
            "nickname".to_string(),
            row.get("email").cloned().unwrap_or(Value::Null),
        );
    }
    mapped.insert("source".to_string(), Value::String("cockpit".to_string()));
    Value::Object(mapped)
}

/// 归一化一行导入数据，返回账号字段或错误信息。
pub fn normalise_import_row(row: &Value, realm: Option<&str>) -> Result<AccountFields, String> {
    // cockpit 裸数组行的 snake_case 映射
    let row = if row.get("access_token").is_some() && row.get("accessToken").is_none() {
        map_cockpit_row(row)
    } else {
        row.clone()
    };

    let auth = row.get("auth").and_then(|v| v.as_object().map(|_| v));
    let profile = row.get("account").and_then(|v| v.as_object().map(|_| v));

    let token = pick(&row, auth, profile, "accessToken")
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("no accessToken".to_string());
    }
    if token.matches('.').count() != 2 {
        return Err("accessToken is not a JWT".to_string());
    }

    // realm 优先级：强制 > token 证据 > 行内 realm > 兜底（对齐 :3617-3627）
    let forced = realm.unwrap_or("").trim().to_ascii_lowercase();
    let evidence = realm_evidence(
        &token,
        pick(&row, auth, profile, "domain")
            .unwrap_or_default()
            .as_str(),
    );
    let row_realm = pick(&row, auth, profile, "realm")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let detected = if forced == "intl" || forced == "cn" {
        forced
    } else if let Some(ev) = evidence {
        ev.to_string()
    } else if row_realm == "intl" || row_realm == "cn" {
        row_realm
    } else {
        detect_realm_from_token(&token, pick(&row, auth, profile, "domain").as_deref()).to_string()
    };

    let raw_uid = pick(&row, auth, profile, "uid")
        .unwrap_or_default()
        .trim()
        .to_string();
    let uid = if raw_uid.is_empty() {
        jwt_uid(&token)
    } else {
        raw_uid
    };
    // uid 净化：非 [A-Za-z0-9_-] → _，再去首尾 _ 与空白
    let uid: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(|c: char| c == '_' || c.is_whitespace())
        .to_string();
    if uid.is_empty() {
        return Err("cannot determine uid (no uid field and no sub claim)".to_string());
    }

    let expires = normalize_epoch(
        pick(&row, auth, profile, "expiresAt")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0),
    );
    let expires_at = if expires != 0 {
        expires
    } else {
        jwt_exp(&token)
    };

    Ok(AccountFields {
        uid,
        nickname: pick(&row, auth, profile, "nickname").unwrap_or_default(),
        domain: domain_for_realm(
            &detected,
            pick(&row, auth, profile, "domain")
                .unwrap_or_default()
                .as_str(),
        ),
        realm: detected,
        platform: pick(&row, auth, profile, "platform").unwrap_or_else(|| "CLI".to_string()),
        enterprise_id: pick(&row, auth, profile, "enterpriseId").unwrap_or_default(),
        access_token: token,
        refresh_token: pick(&row, auth, profile, "refreshToken").unwrap_or_default(),
        expires_at,
        source: pick(&row, auth, profile, "source").unwrap_or_else(|| "import".to_string()),
        enabled: true,
        last_error: String::new(),
        cooldown_until: 0.0,
        product: String::new(),
        added_at: 0,
        weight: 1,
        note: String::new(),
    })
}
