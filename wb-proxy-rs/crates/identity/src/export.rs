//! 导出文档：把账号字段集序列化为 workbuddy-accounts v1 文档。
//!
//! 移植自 wb_accounts.py:3466-3534。常量语义：
//! - VOLATILE_FIELDS：活状态，导出供检查但导入不信任
//! - RUNTIME_ONLY_FIELDS：连本地凭据文件都不落盘
//! - PORTABLE_EXPORT_FIELDS：可移植/仅账号导出保留的字段
//!
//! exportedAt 是 "%Y-%m-%d %H:%M:%S" 本地时间，不是 ISO。

use crate::import::AccountFields;
use serde_json::{json, Value};

pub const EXPORT_FORMAT: &str = "workbuddy-accounts";
pub const EXPORT_VERSION: i64 = 1;

/// 描述活状态而非凭据本身的字段：导出供检查，导入不信任。
pub const VOLATILE_FIELDS: [&str; 7] = [
    "cooldownUntil",
    "lastError",
    "credits",
    "lastCheckin",
    "lastDailyChat",
    "lastActivityReport",
    "modelCaps",
];

/// 可移植/仅账号导出保留的字段。
pub const PORTABLE_EXPORT_FIELDS: [&str; 15] = [
    "uid",
    "nickname",
    "domain",
    "realm",
    "platform",
    "product",
    "enterpriseId",
    "accessToken",
    "refreshToken",
    "expiresAt",
    "addedAt",
    "source",
    "enabled",
    "weight",
    "note",
];

/// RUNTIME_ONLY_FIELDS 是 VOLATILE_FIELDS 的子集：本地凭据文件不落盘。
pub const RUNTIME_ONLY_FIELDS: [&str; 2] = ["lastError", "cooldownUntil"];

/// 把一个账号字段集序列化为导出行。portable=true 时只留 PORTABLE_EXPORT_FIELDS。
pub fn account_to_export(fields: &AccountFields, portable: bool) -> Value {
    let data = json!({
        "uid": fields.uid,
        "nickname": fields.nickname,
        "domain": fields.domain,
        "realm": fields.realm,
        "platform": fields.platform,
        "product": fields.product,
        "enterpriseId": fields.enterprise_id,
        "accessToken": fields.access_token,
        "refreshToken": fields.refresh_token,
        "expiresAt": fields.expires_at,
        "addedAt": fields.added_at,
        "source": fields.source,
        "enabled": fields.enabled,
        "weight": fields.weight,
        "note": fields.note,
        "lastError": fields.last_error,
        "cooldownUntil": fields.cooldown_until,
    });
    if portable {
        let mut obj = serde_json::Map::new();
        for key in PORTABLE_EXPORT_FIELDS {
            if let Some(v) = data.get(key) {
                obj.insert(key.to_string(), v.clone());
            }
        }
        Value::Object(obj)
    } else {
        data
    }
}

/// 把账号字段集包装成自描述的导出文档。
pub fn build_export_document(accounts: &[AccountFields], include_secrets: bool) -> Value {
    let rows: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let mut row = account_to_export(a, false);
            if !include_secrets {
                if let Some(obj) = row.as_object_mut() {
                    obj.remove("accessToken");
                    obj.remove("refreshToken");
                }
            }
            row
        })
        .collect();
    json!({
        "format": EXPORT_FORMAT,
        "version": EXPORT_VERSION,
        "exportedAt": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "count": rows.len(),
        "accounts": rows,
    })
}
