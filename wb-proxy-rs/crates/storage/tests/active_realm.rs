//! active_realm 容忍额外字段 + 代表账号选择语义。
//!
//! 移植自 Python 行为：
//! - active_realm.json 真实内容含 updated_at/updated_iso（wb_proxy.py:5653-5690），
//!   读取端只取 realm 并校验 ∈ {intl,cn}，容忍额外字段
//! - current_account() = POOL.representative()（wb_accounts.py:3154-3159）：
//!   取第一个有 access_token 的账号，否则第一个，池空则 None

use std::fs;
use std::path::Path;

use wb_storage::{load_snapshot, representative};

fn fixture_dir(realm_doc: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("active_realm.json"), realm_doc).unwrap();
    dir
}

fn write_account(dir: &Path, name: &str, token: &str) {
    let body = serde_json::json!({
        "uid": name, "realm": "intl", "enabled": true,
        "accessToken": token, "cooldownUntil": 0.0, "lastError": ""
    });
    fs::write(dir.join(format!("{name}.json")), body.to_string()).unwrap();
}

#[test]
fn active_realm_tolerates_extra_time_fields() {
    // 真实文件含 updated_at/updated_iso（wb_proxy.py 写入），读取端不得因
    // 多余字段而解析失败或取错 realm。
    let dir = fixture_dir(
        r#"{"realm": "cn", "updated_at": 1792032853.6788254, "updated_iso": "2026-10-15 10:54:13"}"#,
    );
    let snap = load_snapshot(dir.path(), None, false).unwrap();
    assert_eq!(snap.realm, "cn");
}

#[test]
fn active_realm_missing_falls_back_to_intl() {
    let dir = fixture_dir(r#"{"updated_at": 1792032853.0, "updated_iso": "2026-10-15 10:54:13"}"#);
    let snap = load_snapshot(dir.path(), None, false).unwrap();
    assert_eq!(snap.realm, "intl");
}

#[test]
fn representative_picks_first_with_token() {
    // 第一个有 access_token 的账号（按文件枚举顺序，这里 a 有、b 无）。
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a", "");
    write_account(dir.path(), "b", "h.p.s");
    let rep = representative(dir.path(), "intl");
    let rep = rep.expect("representative must be Some when pool has accounts");
    assert_eq!(rep.uid, "b");
}

#[test]
fn representative_falls_back_to_first_when_no_token() {
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a", "");
    let rep = representative(dir.path(), "intl");
    let rep = rep.expect("representative must fall back to first account");
    assert_eq!(rep.uid, "a");
}

#[test]
fn representative_none_when_pool_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(representative(dir.path(), "intl").is_none());
}
