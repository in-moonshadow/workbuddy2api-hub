//! 请求鉴权组合，移植自 wb_proxy._key_ok（:15558-15569）、_supplied_key
//! （:15517-15555）、identify_key（:427-434）与 wb_settings.PanelSessions
//! （:1598-1626）。三条路径：panel 会话 token / API key 匹配 / 无需鉴权。

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::http::header::HeaderName;
use axum::http::{HeaderMap, HeaderValue};
use wb_api::auth::{identify_key, key_ok, supplied_key, PanelSessions};
use wb_api::Config;
use wb_storage::api_keys;

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        let name = HeaderName::from_bytes(name.as_bytes()).unwrap();
        let value: HeaderValue = (*value).parse().unwrap();
        map.insert(name, value);
    }
    map
}

fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn cfg_dir(settings: serde_json::Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("settings.json"), settings.to_string()).unwrap();
    fs::write(dir.path().join("active_realm.json"), r#"{"realm": "intl"}"#).unwrap();
    dir
}

fn config(accounts_dir: &Path, api_key: Option<&str>, lan: bool) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: accounts_dir.to_path_buf(),
        dashboard: Path::new("/nonexistent").to_path_buf(),
        api_key: api_key.map(str::to_string),
        lan,
        upstream_base_url: None,
        realm: "intl".into(),
    })
}

// ---- supplied_key ----

#[test]
fn supplied_key_bearer_variants() {
    let h = headers(&[("Authorization", "Bearer sk-abc")]);
    assert_eq!(supplied_key(&h, &query(&[])), "sk-abc");
    // RFC 7235 大小写不敏感：bearer / BEARER 同样剥离
    let h = headers(&[("Authorization", "bearer  sk-abc ")]);
    assert_eq!(supplied_key(&h, &query(&[])), "sk-abc");
    let h = headers(&[("Authorization", "BEARER sk-abc")]);
    assert_eq!(supplied_key(&h, &query(&[])), "sk-abc");
}

#[test]
fn supplied_key_bare_authorization() {
    // 无 scheme 时整 header 作为 key（partition(" ") 无空格 → header）
    let h = headers(&[("Authorization", "sk-abc")]);
    assert_eq!(supplied_key(&h, &query(&[])), "sk-abc");
    let h = headers(&[("Authorization", "Basic zm9v")]);
    assert_eq!(supplied_key(&h, &query(&[])), "Basic zm9v");
}

#[test]
fn supplied_key_strips_quoted_credential() {
    let h = headers(&[("Authorization", "Bearer \"sk-abc\"")]);
    assert_eq!(supplied_key(&h, &query(&[])), "sk-abc");
    let h = headers(&[("Authorization", "'sk-abc'")]);
    assert_eq!(supplied_key(&h, &query(&[])), "sk-abc");
}

#[test]
fn supplied_key_falls_back_to_header_aliases() {
    for name in ["x-api-key", "api-key", "x-auth-token"] {
        let h = headers(&[(name, " sk-xyz ")]);
        assert_eq!(supplied_key(&h, &query(&[])), "sk-xyz", "{name}");
    }
}

#[test]
fn supplied_key_falls_back_to_query() {
    let q = query(&[("key", "sk-q1")]);
    assert_eq!(supplied_key(&HeaderMap::new(), &q), "sk-q1");
    let q = query(&[("api_key", "sk-q2")]);
    assert_eq!(supplied_key(&HeaderMap::new(), &q), "sk-q2");
    let q = query(&[("api-key", "sk-q3")]);
    assert_eq!(supplied_key(&HeaderMap::new(), &q), "sk-q3");
}

#[test]
fn supplied_key_priority_and_empty() {
    // header 优先于 query
    let h = headers(&[("x-api-key", "sk-h")]);
    let q = query(&[("key", "sk-q")]);
    assert_eq!(supplied_key(&h, &q), "sk-h");
    assert_eq!(supplied_key(&HeaderMap::new(), &query(&[])), "");
}

// ---- PanelSessions ----

#[test]
fn panel_token_valid_roundtrip() {
    let sessions = PanelSessions::new(7 * 24 * 3600);
    let token = sessions.create();
    assert!(!token.is_empty());
    assert!(sessions.valid(&token));
}

#[test]
fn panel_token_invalid_cases() {
    let sessions = PanelSessions::new(7 * 24 * 3600);
    assert!(!sessions.valid(""));
    assert!(!sessions.valid("unknown-token"));
}

#[test]
fn panel_token_expires() {
    let sessions = PanelSessions::new(1); // 1 秒 TTL
    let token = sessions.create();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(!sessions.valid(&token));
}

// ---- identify_key ----

#[test]
fn identify_key_matches_panel_key() {
    let dir = cfg_dir(serde_json::json!({
        "api_keys": [{"id": "k1", "name": "主", "key": "sk-panel",
                      "realm": "intl", "models": [], "enabled": true}]
    }));
    let entry =
        identify_key(dir.path(), "sk-panel", Some("sk-launcher")).expect("panel key must match");
    assert_eq!(entry.id, "k1");
    assert_eq!(entry.source, "panel");
}

#[test]
fn identify_key_launcher_only_when_no_panel_keys() {
    // 面板无 key 时 API_KEY（launcher）才生效（对齐 :433）
    let dir = cfg_dir(serde_json::json!({"ui_language": "zh"}));
    let entry = identify_key(dir.path(), "sk-launcher", Some("sk-launcher"))
        .expect("launcher key must match when no panel keys");
    assert_eq!(entry.source, "launcher");
}

#[test]
fn identify_key_panel_keys_shut_out_launcher() {
    // 面板配置了 key 后 launcher key 不再被接受（对齐 :431 注释）
    let dir = cfg_dir(serde_json::json!({
        "api_keys": [{"id": "k1", "name": "主", "key": "sk-panel",
                      "realm": "", "models": [], "enabled": true}]
    }));
    assert!(identify_key(dir.path(), "sk-launcher", Some("sk-launcher")).is_none());
}

#[test]
fn identify_key_wrong_key_is_none() {
    let dir = cfg_dir(serde_json::json!({
        "api_keys": [{"id": "k1", "name": "主", "key": "sk-right",
                      "realm": "", "models": [], "enabled": true}]
    }));
    assert!(identify_key(dir.path(), "sk-wrong", None).is_none());
}

// ---- key_ok（三条路径组合）----

#[test]
fn key_ok_panel_session_unlocks() {
    let dir = cfg_dir(serde_json::json!({"auth_disabled": false}));
    let cfg = config(dir.path(), None, false);
    let sessions = PanelSessions::new(7 * 24 * 3600);
    let token = sessions.create();
    let h = headers(&[("X-Panel-Token", &token)]);
    assert!(key_ok(&h, &query(&[]), &cfg, &sessions));
}

#[test]
fn key_ok_valid_api_key_unlocks() {
    let dir = cfg_dir(serde_json::json!({
        "api_keys": [{"id": "k1", "name": "主", "key": "sk-ok",
                      "realm": "", "models": [], "enabled": true}]
    }));
    let cfg = config(dir.path(), None, false);
    let h = headers(&[("x-api-key", "sk-ok")]);
    assert!(key_ok(&h, &query(&[]), &cfg, &PanelSessions::new(3600)));
}

#[test]
fn key_ok_no_auth_required_unlocks() {
    let dir = cfg_dir(serde_json::json!({"auth_disabled": true}));
    let cfg = config(dir.path(), None, false);
    assert!(key_ok(
        &HeaderMap::new(),
        &query(&[]),
        &cfg,
        &PanelSessions::new(3600)
    ));
}

#[test]
fn key_ok_wrong_key_rejected_when_auth_required() {
    let dir = cfg_dir(serde_json::json!({
        "api_keys": [{"id": "k1", "name": "主", "key": "sk-ok",
                      "realm": "", "models": [], "enabled": true}]
    }));
    let cfg = config(dir.path(), None, false);
    let h = headers(&[("x-api-key", "sk-wrong")]);
    assert!(!key_ok(&h, &query(&[]), &cfg, &PanelSessions::new(3600)));
}

#[test]
fn api_keys_public_view_reads_settings() {
    // auth 模块依赖 storage.api_keys 的公开视图（configured_keys 语义）
    let dir = cfg_dir(serde_json::json!({
        "api_keys": [{"id": "k1", "name": "A", "key": "sk-a", "realm": "", "models": [], "enabled": true}]
    }));
    let keys = api_keys(dir.path());
    assert_eq!(keys.len(), 1);
}
