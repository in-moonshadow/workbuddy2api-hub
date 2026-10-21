//! M5 接线层的存储侧契约（settings.json 通用读写 + 账号积分快照）。
//!
//! 对齐 Python 侧的两个落点：
//! 1. `wb_settings` 的 settings.json（读写都是**浅合并**，文件缺失用默认值）；
//! 2. `Account.credits`（积分快照存在**账号文件**里，`Account.save()` 原子写回）。

use std::fs;

use serde_json::json;

use wb_storage::{
    pool_accounts, read_account_credits, read_settings, write_account_credits, write_settings,
};

fn write_account(dir: &std::path::Path, name: &str, doc: serde_json::Value) {
    fs::write(dir.join(name), serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// settings.json
// ---------------------------------------------------------------------------

#[test]
fn missing_settings_file_reads_as_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    let doc = read_settings(dir.path()).unwrap();
    assert_eq!(
        doc,
        json!({}),
        "缺 settings.json 时应回落空对象，而不是报错"
    );
}

#[test]
fn settings_write_is_shallow_merge_and_keeps_other_keys() {
    let dir = tempfile::tempdir().unwrap();
    write_account(
        dir.path(),
        "settings.json",
        json!({"ui_language": "zh", "launcher_key": "keep-me"}),
    );
    write_settings(dir.path(), &json!({"credits_refresh_hours": 6.0})).unwrap();

    let doc = read_settings(dir.path()).unwrap();
    assert_eq!(doc["credits_refresh_hours"], json!(6.0));
    assert_eq!(
        doc["ui_language"],
        json!("zh"),
        "未被 patch 提到的键必须原样保留"
    );
    assert_eq!(doc["launcher_key"], json!("keep-me"));
}

#[test]
fn settings_write_is_atomic_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    write_settings(dir.path(), &json!({"pricing_refresh_minutes": 5})).unwrap();

    let names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["settings.json".to_string()],
        "不该留下 .tmp 残留"
    );
    let raw = fs::read_to_string(dir.path().join("settings.json")).unwrap();
    assert!(raw.contains('\n'), "落盘应是缩进后的 JSON");
}

#[test]
fn settings_read_tolerates_a_broken_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("settings.json"), "{ not json").unwrap();
    // 读要宽容：坏文件不能让服务起不来，退回空对象（= 全部走默认值）。
    assert_eq!(read_settings(dir.path()).unwrap(), json!({}));
}

#[test]
fn settings_write_refuses_to_overwrite_a_broken_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("settings.json"), "{ not json").unwrap();
    // 写要严格：坏文件上的写入必须失败，且原文件内容一字不动。
    let err = write_settings(dir.path(), &json!({"update_check_enabled": true}));
    assert!(err.is_err(), "settings.json 已损坏时不能静默覆盖");
    assert_eq!(
        fs::read_to_string(dir.path().join("settings.json")).unwrap(),
        "{ not json"
    );
}

// ---------------------------------------------------------------------------
// 账号积分快照
// ---------------------------------------------------------------------------

#[test]
fn account_without_credits_key_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    write_account(
        dir.path(),
        "a.json",
        json!({"uid": "u1", "enabled": true, "accessToken": "t"}),
    );
    assert_eq!(read_account_credits(dir.path(), "a.json").unwrap(), None);
}

#[test]
fn write_account_credits_preserves_other_keys() {
    let dir = tempfile::tempdir().unwrap();
    write_account(
        dir.path(),
        "a.json",
        json!({"uid": "u1", "enabled": true, "accessToken": "t", "note": "keep"}),
    );
    let credits = json!({
        "remain": 100.0,
        "used": 0.0,
        "size": 100.0,
        "used_percent": "0.00%",
        "remain_percent": "100.00%",
        "is_paid_user": false,
        "checkin": null,
        "earliest_expiring": null,
        "packages": [],
        "updated_at": 1760000000.0,
        "updated_iso": "2026-10-21 08:00:00",
    });
    write_account_credits(dir.path(), "a.json", &credits).unwrap();

    assert_eq!(
        read_account_credits(dir.path(), "a.json").unwrap(),
        Some(credits)
    );

    let raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("a.json")).unwrap()).unwrap();
    assert_eq!(raw["uid"], json!("u1"));
    assert_eq!(raw["note"], json!("keep"));
    assert_eq!(raw["credits"]["updated_at"], json!(1760000000.0));
}

#[test]
fn write_account_credits_strips_runtime_only_fields() {
    let dir = tempfile::tempdir().unwrap();
    write_account(
        dir.path(),
        "a.json",
        json!({"uid": "u1", "lastError": "boom", "cooldownUntil": 123.0}),
    );
    write_account_credits(dir.path(), "a.json", &json!({"remain": 1.0})).unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("a.json")).unwrap()).unwrap();
    assert!(
        raw.get("lastError").is_none(),
        "lastError 是运行时字段，落盘前要剥离"
    );
    assert!(raw.get("cooldownUntil").is_none());
    assert_eq!(raw["credits"]["remain"], json!(1.0));
}

#[test]
fn write_account_credits_to_missing_account_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = write_account_credits(dir.path(), "missing.json", &json!({}));
    assert!(
        err.is_err(),
        "账号文件不存在时必须报错，不能静默新建空壳文件"
    );
}

#[test]
fn account_name_with_path_separator_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_account_credits(dir.path(), "../evil.json").is_err());
    assert!(write_account_credits(dir.path(), "../evil.json", &json!({})).is_err());
}

// ---------------------------------------------------------------------------
// 池视图扩展（file_name / enabled / credits）
// ---------------------------------------------------------------------------

#[test]
fn pool_accounts_exposes_file_name_enabled_and_credits() {
    let dir = tempfile::tempdir().unwrap();
    write_account(
        dir.path(),
        "b.json",
        json!({
            "uid": "u2", "realm": "intl", "enabled": true, "accessToken": "t2",
            "credits": {"remain": 30.0, "updated_at": 1760000000.0},
        }),
    );
    write_account(
        dir.path(),
        "a.json",
        json!({"uid": "u1", "realm": "cn", "enabled": false, "accessToken": "t1"}),
    );

    let pool = pool_accounts(dir.path()).unwrap();
    assert_eq!(pool.len(), 2);
    // 枚举按文件名升序（G0-4）。
    assert_eq!(pool[0].file_name, "a.json");
    assert!(!pool[0].enabled);
    assert_eq!(pool[0].credits, None);
    assert_eq!(pool[1].file_name, "b.json");
    assert!(pool[1].enabled);
    assert_eq!(
        pool[1].credits.as_ref().unwrap()["updated_at"],
        json!(1760000000.0)
    );
}
