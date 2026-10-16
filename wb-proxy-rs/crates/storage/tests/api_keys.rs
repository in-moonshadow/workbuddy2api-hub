//! API key 匹配，移植自 wb_settings.py match_api_key（:526-554）、api_keys
//! （:381-424）与 _clean_key_entry（:326-340）。要点：
//! - 面板 api_keys[] 命中返回 source="panel" 的条目
//! - 面板未配置时 extra_keys（如 API_KEY）命中返回 source="launcher"
//! - 常数时间比较（对齐 hmac.compare_digest，长度不同即 False）
//! - 旧格式 api_key_set+api_key 合成 legacy 条目（命中 source 同为 panel）

use std::fs;
use std::path::Path;

use wb_storage::{api_keys, effective_api_key, match_api_key};

fn write_settings(dir: &Path, doc: serde_json::Value) {
    fs::write(dir.join("settings.json"), doc.to_string()).unwrap();
}

fn fixture_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn panel_key_matches() {
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({
            "api_keys": [{"id": "k1", "name": "主 Key", "key": "sk-panel",
                          "realm": "intl", "models": [], "enabled": true}]
        }),
    );
    let entry = match_api_key(dir.path(), "sk-panel", &[]).expect("panel key must match");
    assert_eq!(entry.id, "k1");
    assert_eq!(entry.source, "panel");
    assert_eq!(entry.realm, "intl");
}

#[test]
fn disabled_key_does_not_match() {
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({
            "api_keys": [{"id": "k1", "name": "停用", "key": "sk-old",
                          "realm": "", "models": [], "enabled": false}]
        }),
    );
    assert!(match_api_key(dir.path(), "sk-old", &[]).is_none());
}

#[test]
fn deleted_key_does_not_match() {
    // deleted_at 非空视为已删除：即使用 key 存在也不匹配（对齐 :538）。
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({
            "api_keys": [{"id": "k1", "name": "已删", "key": "sk-gone",
                          "realm": "", "models": [], "enabled": true,
                          "deleted_at": "2026-10-15 10:54:13"}]
        }),
    );
    assert!(match_api_key(dir.path(), "sk-gone", &[]).is_none());
}

#[test]
fn launcher_key_fallback() {
    // 面板未配置 key 时，extra_keys（如 API_KEY）命中返回 launcher 条目。
    let dir = fixture_dir();
    write_settings(dir.path(), serde_json::json!({"ui_language": "zh"}));
    let entry = match_api_key(dir.path(), "sk-launcher", &["sk-launcher"])
        .expect("launcher key must match when no panel keys");
    assert_eq!(entry.id, "launcher");
    assert_eq!(entry.source, "launcher");
}

#[test]
fn wrong_key_is_none() {
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({
            "api_keys": [{"id": "k1", "name": "主", "key": "sk-right",
                          "realm": "", "models": [], "enabled": true}]
        }),
    );
    assert!(match_api_key(dir.path(), "sk-wrong", &[]).is_none());
}

#[test]
fn empty_supplied_is_none() {
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({
            "api_keys": [{"id": "k1", "name": "主", "key": "sk-x",
                          "realm": "", "models": [], "enabled": true}]
        }),
    );
    assert!(match_api_key(dir.path(), "  ", &[]).is_none());
}

#[test]
fn legacy_api_key_shape_surfaces_as_panel_entry() {
    // 旧格式 settings.json 只有 api_key_set+api_key（:413-423）。命中 source
    // 同为 "panel"（match_api_key 对 api_keys() 全部条目统一打 panel 标记）。
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({"api_key_set": true, "api_key": "sk-legacy"}),
    );
    let keys = api_keys(dir.path());
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, "legacy");
    assert_eq!(keys[0].key, "sk-legacy");
    assert!(keys[0].enabled);
    let entry = match_api_key(dir.path(), "sk-legacy", &[]).expect("legacy key must match");
    assert_eq!(entry.source, "panel");
}

#[test]
fn api_keys_reads_list_shape() {
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({
            "api_keys": [
                {"id": "k1", "name": "A", "key": "sk-a", "realm": "", "models": [], "enabled": true},
                {"id": "k2", "name": "B", "key": "sk-b", "realm": "cn", "models": [], "enabled": true},
            ]
        }),
    );
    let keys = api_keys(dir.path());
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].id, "k1");
    assert_eq!(keys[1].realm, "cn");
}

// ---- effective_api_key（Python API_KEY 三来源：--api-key > 面板 api_key_set
// > launcher_key；对齐 wb_proxy.py:18722-18737）----

#[test]
fn env_key_beats_file_sources() {
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({"api_key_set": true, "api_key": "sk-panel", "launcher_key": "sk-lan"}),
    );
    let key = effective_api_key(dir.path(), Some("sk-env")).expect("env must win");
    assert_eq!(key, "sk-env");
}

#[test]
fn panel_saved_key_beats_launcher() {
    // 面板保存的 key 胜出（wb_proxy.py 注释：A key saved from the panel wins
    // over an auto-generated LAN key）
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({"api_key_set": true, "api_key": "sk-panel", "launcher_key": "sk-lan"}),
    );
    let key = effective_api_key(dir.path(), None).expect("panel key must win");
    assert_eq!(key, "sk-panel");
}

#[test]
fn effective_launcher_key_fallback() {
    let dir = fixture_dir();
    write_settings(dir.path(), serde_json::json!({"launcher_key": "sk-lan"}));
    let key = effective_api_key(dir.path(), None).expect("launcher key must fall back");
    assert_eq!(key, "sk-lan");
}

#[test]
fn api_key_set_but_empty_disables_launcher() {
    // api_key_set=true 但 api_key 为空 → 面板声明了管理权，launcher 不再兜底
    // （Python：API_KEY 被覆盖为空字符串）
    let dir = fixture_dir();
    write_settings(
        dir.path(),
        serde_json::json!({"api_key_set": true, "api_key": "", "launcher_key": "sk-lan"}),
    );
    assert!(effective_api_key(dir.path(), None).is_none());
}

#[test]
fn no_key_sources_is_none() {
    let dir = fixture_dir();
    write_settings(dir.path(), serde_json::json!({"ui_language": "zh"}));
    assert!(effective_api_key(dir.path(), None).is_none());
}
