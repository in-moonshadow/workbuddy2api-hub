//! JWT 解码行为，移植自 wb_accounts.py `_jwt_claims`/`jwt_uid`/`jwt_issuer`/
//! `jwt_exp`/`normalize_epoch`（:220-247）。要点：只解码、不验签、不判过期，
//! 解析失败一律返回空/0（宽容，不拒绝账号）。

mod common;

use common::jwt;
use wb_identity::jwt::{jwt_exp, jwt_issuer, jwt_uid, normalize_epoch};

const INTL_ISS: &str = "https://www.workbuddy.ai/auth/realms/copilot";

/// 构造带 exp 的合成 JWT（common::jwt 固定 exp=4102444800）。
fn token_with_sub(sub: &str) -> String {
    jwt(INTL_ISS, sub)
}

#[test]
fn uid_from_sub_claim() {
    assert_eq!(jwt_uid(&token_with_sub("abc-123")), "abc-123");
}

#[test]
fn issuer_from_iss_claim() {
    assert_eq!(jwt_issuer(&token_with_sub("u-1")), INTL_ISS);
}

#[test]
fn exp_from_exp_claim() {
    // common::jwt 把 exp 固定为 4102444800（毫秒）。
    assert_eq!(jwt_exp(&token_with_sub("u-1")), 4102444800);
}

#[test]
fn empty_token_is_tolerant() {
    // 解析失败返回空/0，而不是 panic 或拒收账号。
    assert_eq!(jwt_uid(""), "");
    assert_eq!(jwt_issuer(""), "");
    assert_eq!(jwt_exp(""), 0);
    assert_eq!(jwt_uid("not-a-jwt"), "");
}

#[test]
fn normalize_epoch_converts_millis() {
    // >1e11 视为毫秒，除以 1000。
    assert_eq!(normalize_epoch(4102444800000.0), 4102444800);
}

#[test]
fn normalize_epoch_seconds_unchanged() {
    assert_eq!(normalize_epoch(4102444800.0), 4102444800);
}

#[test]
fn normalize_epoch_zero_for_garbage() {
    // 无法转成数字的输入返回 0（宽容）。
    assert_eq!(normalize_epoch(0.0), 0);
}
