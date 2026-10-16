//! 导入行归一化，移植自 tests/_test_cockpit_import.py 的纯函数断言。
//!
//! 只测 normalise_import_row（返回 AccountFields），不测 AccountPool.import_rows
//! 的写池路径（那些涉及 get/add/apply_proxy_slots，属 M2-b/M5）。

mod common;

use common::jwt;
use serde_json::json;
use wb_identity::import::normalise_import_row;

const INTL_ISS: &str = "https://www.workbuddy.ai";
const CN_ISS: &str = "https://copilot.tencent.com";

/// 构造 cockpit 形状的行（snake_case + auth 层），与 Python cockpit_row() 同构。
fn cockpit_row(uid: &str, issuer: &str, domain: &str, nickname: &str) -> serde_json::Value {
    json!({
        "id": "c1",
        "email": "user@example.com",
        "uid": uid,
        "nickname": nickname,
        "access_token": jwt(issuer, uid),
        "refresh_token": "refresh-token",
        "token_type": "Bearer",
        "expires_at": 4102444800000_u64, // 毫秒
        "domain": domain,
    })
}

#[test]
fn snake_case_row_maps_to_account_fields() {
    let out = normalise_import_row(
        &cockpit_row("uid-cockpit", INTL_ISS, "www.workbuddy.ai", ""),
        None,
    )
    .unwrap();
    assert_eq!(out.uid, "uid-cockpit");
    assert_eq!(out.realm, "intl");
    assert_eq!(out.source, "cockpit");
    assert_eq!(out.refresh_token, "refresh-token");
    // expires_at 毫秒 → 秒
    assert_eq!(out.expires_at, 4102444800);
}

#[test]
fn nickname_falls_back_to_email() {
    // 空 nickname 时用 email；非空则保留。
    let row = json!({
        "uid": "u1", "access_token": jwt(INTL_ISS, "u1"),
        "refresh_token": "r", "nickname": "", "email": "user@example.com",
    });
    let out = normalise_import_row(&row, None).unwrap();
    assert_eq!(out.nickname, "user@example.com");

    let row_named = json!({
        "uid": "u1", "access_token": jwt(INTL_ISS, "u1"),
        "refresh_token": "r", "nickname": "Named", "email": "user@example.com",
    });
    assert_eq!(
        normalise_import_row(&row_named, None).unwrap().nickname,
        "Named"
    );
}

#[test]
fn cn_domain_stays_cn() {
    let out =
        normalise_import_row(&cockpit_row("u1", CN_ISS, "copilot.tencent.com", ""), None).unwrap();
    assert_eq!(out.realm, "cn");
}

#[test]
fn missing_token_is_rejected() {
    let mut row = cockpit_row("u1", INTL_ISS, "www.workbuddy.ai", "");
    row["access_token"] = json!("");
    let err = normalise_import_row(&row, None).unwrap_err();
    assert!(err.contains("no accessToken"));
}

#[test]
fn non_jwt_token_is_rejected() {
    // 点段数 != 2 → 「not a JWT」
    let mut row = cockpit_row("u1", INTL_ISS, "www.workbuddy.ai", "");
    row["access_token"] = json!("just-one-dot");
    let err = normalise_import_row(&row, None).unwrap_err();
    assert!(err.contains("not a JWT"));
}
