//! 区域归属：桌面凭据与手动导入的 JSON 都要落到正确的那一边。
//!
//! 直接移植自 tests/_test_desktop_realm.py（Python 权威行为）。

mod common;

use common::jwt;
use wb_identity::realm::{
    desktop_effective_realm, detect_realm_from_token, domain_for_realm, realm_evidence,
};

const CN_ISS: &str = "https://www.workbuddy.cn/auth/realms/copilot";
const CN_ISS_LEGACY: &str = "https://www.codebuddy.cn/auth/realms/copilot";
const CN_ISS_TENCENT: &str = "https://copilot.tencent.com/auth/realms/copilot";
const INTL_ISS: &str = "https://www.workbuddy.ai/auth/realms/copilot";
const OPAQUE_ISS: &str = "https://example.invalid/realms/x"; // 看不出区域

fn cn_token() -> String {
    jwt(CN_ISS, "u-1")
}
fn cn_token_legacy() -> String {
    jwt(CN_ISS_LEGACY, "u-1")
}
fn cn_token_tencent() -> String {
    jwt(CN_ISS_TENCENT, "u-1")
}
fn intl_token() -> String {
    jwt(INTL_ISS, "u-1")
}
fn opaque_token() -> String {
    jwt(OPAQUE_ISS, "u-1")
}

#[test]
fn cn_markers() {
    for (token, domain) in [
        (cn_token(), ""),
        (cn_token_legacy(), ""),
        (cn_token_tencent(), ""),
        (opaque_token(), "www.workbuddy.cn"),
        (opaque_token(), "copilot.tencent.com"),
        (opaque_token(), "www.codebuddy.cn"),
    ] {
        assert_eq!(
            realm_evidence(&token, domain),
            Some("cn"),
            "should read as CN: {domain}"
        );
    }
}

#[test]
fn intl_markers() {
    for (token, domain) in [(intl_token(), ""), (opaque_token(), "www.workbuddy.ai")] {
        assert_eq!(realm_evidence(&token, domain), Some("intl"));
    }
}

#[test]
fn unknown_is_none_not_intl() {
    // 看不出区域要老实返回 None，别默认成 intl（老实现的问题就在这）。
    assert_eq!(realm_evidence(&opaque_token(), ""), None);
    assert_eq!(realm_evidence("", ""), None);
}

#[test]
fn detect_realm_default_unchanged() {
    // 对外的 detect_realm_from_token 仍然把看不出的当国际版（保持兼容）。
    assert_eq!(detect_realm_from_token(&opaque_token(), None), "intl");
    assert_eq!(detect_realm_from_token(&cn_token(), None), "cn");
}

#[test]
fn workbuddy_cn_is_not_confused_with_intl() {
    assert_eq!(
        realm_evidence(&opaque_token(), "www.workbuddy.cn"),
        Some("cn")
    );
    assert_eq!(
        realm_evidence(&opaque_token(), "www.workbuddy.ai"),
        Some("intl")
    );
}

#[test]
fn token_wins_over_filename() {
    // 国内账号登录在国际版那套客户端里（切区登录），文件名叫 -ai.info
    assert_eq!(
        desktop_effective_realm("intl", &cn_token(), Some("www.workbuddy.cn")),
        "cn"
    );
    assert_eq!(
        desktop_effective_realm("cn", &intl_token(), Some("www.workbuddy.ai")),
        "intl"
    );
}

#[test]
fn filename_used_when_token_has_no_evidence() {
    assert_eq!(desktop_effective_realm("cn", &opaque_token(), None), "cn");
    assert_eq!(
        desktop_effective_realm("intl", &opaque_token(), None),
        "intl"
    );
}

#[test]
fn unknown_hint_defaults_to_intl() {
    assert_eq!(desktop_effective_realm("", &opaque_token(), None), "intl");
}

#[test]
fn domain_follows_realm() {
    // 区域定了，X-Domain 就不能还指着另一边。
    assert_eq!(
        domain_for_realm("cn", "www.workbuddy.ai"),
        "copilot.tencent.com"
    );
    assert_eq!(
        domain_for_realm("intl", "www.codebuddy.cn"),
        "www.workbuddy.ai"
    );
}

#[test]
fn matching_domain_is_kept() {
    assert_eq!(
        domain_for_realm("cn", "www.codebuddy.cn"),
        "www.codebuddy.cn"
    );
    assert_eq!(
        domain_for_realm("intl", "www.workbuddy.ai"),
        "www.workbuddy.ai"
    );
}
