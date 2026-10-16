//! 导出文档形状，移植自 wb_accounts.py build_export_document（:3505-3534）
//! 与 account_to_export（:3490-3502）。要点：format/version/exportedAt/count/
//! accounts、portable 只留 PORTABLE_EXPORT_FIELDS、include_secrets 剥离 token。

use wb_identity::export::{account_to_export, build_export_document, PORTABLE_EXPORT_FIELDS};
use wb_identity::import::AccountFields;

fn sample_fields(uid: &str) -> AccountFields {
    AccountFields {
        uid: uid.to_string(),
        nickname: format!("nick-{uid}"),
        domain: "www.workbuddy.ai".to_string(),
        realm: "intl".to_string(),
        platform: "CLI".to_string(),
        enterprise_id: "".to_string(),
        access_token: "h.p.s".to_string(),
        refresh_token: "r.r.r".to_string(),
        expires_at: 4102444800,
        source: "import".to_string(),
        enabled: true,
        last_error: String::new(),
        cooldown_until: 0.0,
        product: String::new(),
        added_at: 0,
        weight: 1,
        note: String::new(),
    }
}

#[test]
fn export_document_has_expected_envelope() {
    let doc = build_export_document(&[sample_fields("u1")], false);
    assert_eq!(doc["format"], "workbuddy-accounts");
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["count"], 1);
    // exportedAt 是 "%Y-%m-%d %H:%M:%S" 形状（本地时间）
    let at = doc["exportedAt"].as_str().unwrap();
    assert_eq!(at.len(), 19);
    assert_eq!(at.as_bytes()[4], b'-');
    assert_eq!(at.as_bytes()[7], b'-');
    assert_eq!(at.as_bytes()[10], b' ');
    assert_eq!(at.as_bytes()[13], b':');
    assert_eq!(doc["accounts"][0]["uid"], "u1");
}

#[test]
fn include_secrets_false_strips_tokens() {
    // include_secrets=false 时导出行不得含 token（对齐 build_export_document）。
    let doc = build_export_document(&[sample_fields("u1")], false);
    let row = &doc["accounts"][0];
    assert!(row.get("accessToken").is_none());
    assert!(row.get("refreshToken").is_none());
    // 对照：include_secrets=true 时保留。
    let doc_full = build_export_document(&[sample_fields("u1")], true);
    assert_eq!(doc_full["accounts"][0]["accessToken"], "h.p.s");
    assert_eq!(doc_full["accounts"][0]["refreshToken"], "r.r.r");
}

#[test]
fn portable_keeps_only_portable_fields() {
    let portable = account_to_export(&sample_fields("u1"), true);
    let obj = portable.as_object().unwrap();
    // 非 portable 字段（lastError/cooldownUntil）必须不在
    assert!(!obj.contains_key("lastError"));
    assert!(!obj.contains_key("cooldownUntil"));
    // portable 字段必须全在
    for key in PORTABLE_EXPORT_FIELDS {
        assert!(obj.contains_key(key), "portable export must keep {key}");
    }
}

#[test]
fn build_export_document_respects_count() {
    let doc = build_export_document(&[sample_fields("u1"), sample_fields("u2")], false);
    assert_eq!(doc["count"], 2);
    assert_eq!(doc["accounts"].as_array().unwrap().len(), 2);
}

#[test]
fn empty_export_is_valid() {
    let doc = build_export_document(&[], false);
    assert_eq!(doc["count"], 0);
    assert!(doc["accounts"].as_array().unwrap().is_empty());
}

#[test]
fn envelope_shape_json_round_trips() {
    // 序列化再反序列化不丢键（falsification：确保导出文档是标准 JSON）。
    let doc = build_export_document(&[sample_fields("u1")], false);
    let text = doc.to_string();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["format"], "workbuddy-accounts");
    assert_eq!(parsed["accounts"][0]["uid"], "u1");
}
