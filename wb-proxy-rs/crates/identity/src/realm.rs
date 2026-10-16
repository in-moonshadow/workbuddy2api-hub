//! realm 归属判定：从 token/issuer/域名里认出账号区域。
//!
//! 移植自 wb_accounts.py:249-254（markers）、:305-350（realm_evidence/
//! detect_realm_from_token/domain_for_realm/desktop_effective_realm）与
//! :173-192（REALM_CONFIGS 的 domain）。要点：看不出区域时 realm_evidence
//! 返回 None 不瞎猜；对外的 detect_realm_from_token 兜底 "intl"（保持兼容）。

use crate::jwt::jwt_issuer;

pub const CN_REALM_MARKERS: [&str; 3] = ["copilot.tencent.com", "codebuddy.cn", "workbuddy.cn"];
pub const INTL_REALM_MARKERS: [&str; 2] = ["workbuddy.ai", "codebuddy.ai"];

/// 各 realm 的规范域名（对齐 Python REALM_CONFIGS[realm]["domain"]）。
pub fn canonical_domain(realm: &str) -> &'static str {
    match realm {
        "cn" => "copilot.tencent.com",
        _ => "www.workbuddy.ai",
    }
}

/// 从 token / 域名里看区域，看不出返回 None（不要瞎猜成 intl）。
///
/// 先看 token 自己的 issuer，再看域名：域名是客户端随手记下来的字段，可能过期
/// 或干脆是另一边的（同一台机器切区登录过就会这样），token 才是要拿去请求的
/// 那一串，冲突时以它为准。
pub fn realm_evidence(token: &str, domain: &str) -> Option<&'static str> {
    let iss = jwt_issuer(token).to_ascii_lowercase();
    if CN_REALM_MARKERS.iter().any(|m| iss.contains(m)) {
        return Some("cn");
    }
    if INTL_REALM_MARKERS.iter().any(|m| iss.contains(m)) {
        return Some("intl");
    }
    let dom = domain.to_ascii_lowercase();
    if CN_REALM_MARKERS.iter().any(|m| dom.contains(m)) {
        return Some("cn");
    }
    if INTL_REALM_MARKERS.iter().any(|m| dom.contains(m)) {
        return Some("intl");
    }
    None
}

/// 对外的判定：把看不出的当国际版（保持兼容，对齐 detect_realm_from_token）。
pub fn detect_realm_from_token(token: &str, domain: Option<&str>) -> &'static str {
    realm_evidence(token, domain.unwrap_or("")).unwrap_or("intl")
}

/// 把域名对齐到区域：域名写着另一个区域时换成该区域的规范域名。
///
/// 出站请求的 X-Domain 头跟着它走，区域既然以 token 为准，就不能让一个过期
/// 域名再把请求带回另一边的出口。域名本身看不出区域时原样保留。
pub fn domain_for_realm(realm: &str, domain: &str) -> String {
    let dom = domain.trim();
    let other = if realm == "cn" { "intl" } else { "cn" };
    let other_markers: &[&str] = if other == "cn" {
        &CN_REALM_MARKERS
    } else {
        &INTL_REALM_MARKERS
    };
    if !dom.is_empty()
        && !other_markers
            .iter()
            .any(|m| dom.to_ascii_lowercase().contains(m))
    {
        dom.to_string()
    } else {
        canonical_domain(realm).to_string()
    }
}

/// 桌面凭据归哪个区域：token / 域名说了算，文件名只作兜底。
///
/// hint 是客户端文件名推测（workbuddy-desktop.info → 国内、-ai.info → 国际），
/// 只是默认约定；token 判定优先，hint 不在白名单时兜底 intl。
pub fn desktop_effective_realm(hint: &str, token: &str, domain: Option<&str>) -> String {
    if let Some(evidence) = realm_evidence(token, domain.unwrap_or("")) {
        return evidence.to_string();
    }
    if hint == "cn" || hint == "intl" {
        hint.to_string()
    } else {
        "intl".to_string()
    }
}
