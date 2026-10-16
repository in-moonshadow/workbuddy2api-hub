//! 出站身份头，移植自 wb_identity.build_identity_headers（:76-165）、
//! wb_accounts.Account.headers(purpose="chat")（:1114-1175）与
//! wb_fingerprint.derive_id（:11-17）。
//!
//! 随机 id（X-Conversation-ID/Request-ID/Message-ID）用 getrandom 生成 32 位
//! hex（等价 Python `uuid.uuid4().hex`）；derive_id 用 MD5 稳定派生，与
//! Python hashlib 逐字节一致（上游按设备指纹计风控，幂等是关键）。

use std::collections::HashMap;
use std::fmt::Write;

use md5::{Digest, Md5};

use crate::endpoints::{
    endpoint_for, normalize_product, realm_config, PRODUCT_DESKTOP, PRODUCT_VSCODE,
};

const DEFAULT_IDE_VERSION: &str = "2.117.2";
const DEFAULT_WORKBUDDY_IDE_VERSION: &str = "1.119.0";
const DEFAULT_WORKBUDDY_PRODUCT_VERSION: &str = "4.9.29177644";

/// `md5("<salt>:<uid|anonymous>")` → 32 位小写 hex，幂等。
/// 对齐 wb_fingerprint.derive_id（Python `[:36]` 在 32 位 hexdigest 上原样返回）。
pub fn derive_id(uid: &str, salt: &str) -> String {
    let subject = if uid.is_empty() { "anonymous" } else { uid };
    let digest = Md5::digest(format!("{salt}:{subject}").as_bytes());
    to_hex(&digest)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut out, "{b:02x}").unwrap();
    }
    out
}

/// 32 位随机 hex，等价 `uuid.uuid4().hex`（16 字节 CSPRNG）。
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("os rng must be available");
    to_hex(&bytes)
}

/// build_identity_headers 的输入，字段名对齐 wb_identity.py 参数表。
pub struct IdentityParams<'a> {
    pub product: &'a str,
    pub realm: &'a str,
    pub uid: &'a str,
    pub token: &'a str,
    pub conversation_id: Option<&'a str>,
    pub enterprise_id: &'a str,
    pub tenant_id: &'a str,
    pub department: &'a str,
}

/// 对齐 build_identity_headers（:76-165）：product 三分支，公共尾部追加
/// enterprise / department 头。conversation_id 缺失时随机生成并转大写。
pub fn build_identity_headers(p: &IdentityParams) -> HashMap<String, String> {
    let product = normalize_product(p.product);
    let is_cn = p.realm == "cn";
    let conv = match p.conversation_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c.to_ascii_uppercase(),
        None => random_hex().to_ascii_uppercase(),
    };
    let msg_id = random_hex();
    let uid = if p.uid.is_empty() { "anonymous" } else { p.uid };
    let domain = endpoint_for(p.realm, product).1;

    let mut headers = HashMap::new();
    match product {
        PRODUCT_DESKTOP => {
            let client_ver = if is_cn { "5.5.6" } else { "5.5.2" };
            let chat_ua = if is_cn {
                "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1"
            } else {
                "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2"
            };
            headers.insert("X-Agent-Purpose".into(), "conversation".into());
            headers.insert("X-IDE-Name".into(), "WorkBuddy".into());
            headers.insert("X-IDE-Type".into(), "WorkBuddy".into());
            headers.insert("X-IDE-Version".into(), client_ver.into());
            headers.insert("X-Product".into(), "WorkBuddy".into());
            headers.insert("User-Agent".into(), chat_ua.into());
        }
        PRODUCT_VSCODE => {
            headers.insert("X-Agent-Intent".into(), "craft".into());
            headers.insert("X-IDE-Type".into(), "VSCode".into());
            headers.insert("X-IDE-Name".into(), "VSCode".into());
            headers.insert("X-IDE-Version".into(), DEFAULT_WORKBUDDY_IDE_VERSION.into());
            headers.insert(
                "X-Product-Version".into(),
                DEFAULT_WORKBUDDY_PRODUCT_VERSION.into(),
            );
            headers.insert("X-Env-ID".into(), "production".into());
            headers.insert("X-Product".into(), "SaaS".into());
            headers.insert(
                "User-Agent".into(),
                format!(
                    "VSCode/{DEFAULT_WORKBUDDY_IDE_VERSION} WorkBuddy/{DEFAULT_WORKBUDDY_PRODUCT_VERSION}"
                ),
            );
            headers.insert("X-Conversation-ID".into(), conv);
            headers.insert("X-Conversation-Request-ID".into(), random_hex());
        }
        _ => {
            headers.insert("X-Agent-Intent".into(), "craft".into());
            headers.insert("X-IDE-Type".into(), "CLI".into());
            headers.insert("X-IDE-Name".into(), "CLI".into());
            headers.insert("X-IDE-Version".into(), DEFAULT_IDE_VERSION.into());
            headers.insert("X-Product".into(), "SaaS".into());
            headers.insert(
                "User-Agent".into(),
                format!("CLI/{DEFAULT_IDE_VERSION} CodeBuddy/{DEFAULT_IDE_VERSION}"),
            );
            headers.insert("X-Conversation-ID".into(), conv);
            headers.insert("X-Conversation-Request-ID".into(), random_hex());
        }
    }
    headers.insert("X-Domain".into(), domain.into());
    headers.insert("X-User-Id".into(), uid.into());
    headers.insert("Authorization".into(), format!("Bearer {}", p.token));
    headers.insert("X-Conversation-Message-ID".into(), msg_id.clone());
    headers.insert("X-Request-ID".into(), msg_id);

    if !p.enterprise_id.is_empty() {
        headers.insert("X-Enterprise-Id".into(), p.enterprise_id.into());
        let tenant = if p.tenant_id.is_empty() {
            p.enterprise_id
        } else {
            p.tenant_id
        };
        headers.insert("X-Tenant-Id".into(), tenant.into());
    }
    if !p.department.is_empty() {
        headers.insert("X-Department-Info".into(), p.department.into());
    }
    headers
}

/// chat 出站头组合的输入。
pub struct ChatHeadersParams<'a> {
    pub uid: &'a str,
    pub token: &'a str,
    pub realm: &'a str,
    pub product: &'a str,
    pub conversation_id: Option<&'a str>,
    pub device_token: Option<&'a str>,
    pub enterprise_id: &'a str,
    pub tenant_id: &'a str,
    pub department: &'a str,
}

/// 对齐 Account.headers(purpose="chat")（wb_accounts.py:1114-1175）：
/// 固定头 + build_identity_headers 身分头 + 可选 X-Device-Token。
pub fn chat_request_headers(p: &ChatHeadersParams) -> HashMap<String, String> {
    let cfg = realm_config(p.realm);
    let mut headers = HashMap::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Accept".into(), "application/json, text/plain, */*".into());
    headers.insert("X-Requested-With".into(), "XMLHttpRequest".into());
    headers.insert("Origin".into(), cfg.origin.into());
    headers.insert("Referer".into(), format!("{}/", cfg.origin));
    headers.insert("X-CodeBuddy-Request".into(), "1".into());
    headers.insert(
        "Accept-Language".into(),
        if p.realm == "cn" {
            "zh-CN".into()
        } else {
            "en-US".into()
        },
    );
    headers.insert("X-Machine-ID".into(), derive_id(p.uid, "machine"));
    headers.insert("X-Session-ID".into(), derive_id(p.uid, "session"));
    headers.extend(build_identity_headers(&IdentityParams {
        product: p.product,
        realm: p.realm,
        uid: p.uid,
        token: p.token,
        conversation_id: p.conversation_id,
        enterprise_id: p.enterprise_id,
        tenant_id: p.tenant_id,
        department: p.department,
    }));
    if let Some(token) = p.device_token.map(str::trim).filter(|t| !t.is_empty()) {
        headers.insert("X-Device-Token".into(), token.into());
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::{PRODUCT_CLI, PRODUCT_DESKTOP, PRODUCT_VSCODE};

    #[test]
    fn derive_id_matches_python_hashes() {
        // python3 -c "import hashlib; print(hashlib.md5(f'{salt}:{uid}'.encode()).hexdigest())"
        assert_eq!(
            derive_id("u1", "machine"),
            "1c77b9ac7fdc6e92cc94595f68a05717"
        );
        assert_eq!(
            derive_id("u1", "session"),
            "d4bf0937989e4311fdd68b54baf87841"
        );
        assert_eq!(derive_id("", "machine"), "fc1f3f603c8bcdc0783942ea0169ab1e");
    }

    #[test]
    fn derive_id_is_idempotent() {
        assert_eq!(derive_id("u1", "machine"), derive_id("u1", "machine"));
        assert_eq!(derive_id("u2", "session"), derive_id("u2", "session"));
        // 32 位十六进制
        assert_eq!(derive_id("u1", "machine").len(), 32);
    }

    #[test]
    fn random_hex_is_32_chars() {
        let a = random_hex();
        let b = random_hex();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    fn params<'a>(product: &'a str, realm: &'a str) -> IdentityParams<'a> {
        IdentityParams {
            product,
            realm,
            uid: "u1",
            token: "tok",
            conversation_id: None,
            enterprise_id: "",
            tenant_id: "",
            department: "",
        }
    }

    #[test]
    fn desktop_intl_headers() {
        let h = build_identity_headers(&params(PRODUCT_DESKTOP, "intl"));
        assert_eq!(h["X-Agent-Purpose"], "conversation");
        assert_eq!(h["X-IDE-Type"], "WorkBuddy");
        assert_eq!(h["X-IDE-Version"], "5.5.2");
        assert_eq!(h["X-Product"], "WorkBuddy");
        assert_eq!(
            h["User-Agent"],
            "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2"
        );
        assert_eq!(h["X-Domain"], "www.workbuddy.ai");
        assert_eq!(h["X-User-Id"], "u1");
        assert_eq!(h["Authorization"], "Bearer tok");
        assert!(h.contains_key("X-Request-ID"));
        // desktop 分支没有会话头
        assert!(!h.contains_key("X-Conversation-ID"));
    }

    #[test]
    fn desktop_cn_headers() {
        let h = build_identity_headers(&params(PRODUCT_DESKTOP, "cn"));
        assert_eq!(h["X-IDE-Version"], "5.5.6");
        assert_eq!(h["X-Domain"], "copilot.tencent.com");
        assert_eq!(
            h["User-Agent"],
            "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1"
        );
    }

    #[test]
    fn vscode_headers() {
        let h = build_identity_headers(&params(PRODUCT_VSCODE, "intl"));
        assert_eq!(h["X-IDE-Type"], "VSCode");
        assert_eq!(h["X-Env-ID"], "production");
        assert_eq!(h["X-Product"], "SaaS");
        assert_eq!(h["X-Product-Version"], DEFAULT_WORKBUDDY_PRODUCT_VERSION);
        assert!(h.contains_key("X-Conversation-ID"));
        assert!(h.contains_key("X-Conversation-Request-ID"));
        assert!(h.contains_key("X-Conversation-Message-ID"));
    }

    #[test]
    fn cli_headers() {
        let h = build_identity_headers(&params(PRODUCT_CLI, "intl"));
        assert_eq!(h["X-IDE-Type"], "CLI");
        assert_eq!(h["X-IDE-Version"], "2.117.2");
        assert_eq!(h["User-Agent"], "CLI/2.117.2 CodeBuddy/2.117.2");
    }

    #[test]
    fn provided_conversation_id_is_uppercased() {
        // desktop 分支不带会话头；vscode/cli 才有 X-Conversation-ID
        let mut p = params(PRODUCT_VSCODE, "intl");
        p.conversation_id = Some("conv-abc");
        let h = build_identity_headers(&p);
        assert_eq!(h["X-Conversation-ID"], "CONV-ABC");
    }

    #[test]
    fn anonymous_uid_and_enterprise_headers() {
        let mut p = params(PRODUCT_CLI, "cn");
        p.uid = "";
        p.enterprise_id = "ent-1";
        p.tenant_id = "";
        let h = build_identity_headers(&p);
        assert_eq!(h["X-User-Id"], "anonymous");
        assert_eq!(h["X-Enterprise-Id"], "ent-1");
        // tenant 缺省时兜底 enterprise
        assert_eq!(h["X-Tenant-Id"], "ent-1");
    }

    #[test]
    fn no_enterprise_headers_when_empty() {
        let h = build_identity_headers(&params(PRODUCT_DESKTOP, "intl"));
        assert!(!h.contains_key("X-Enterprise-Id"));
    }

    fn chat_params<'a>(realm: &'a str, device_token: Option<&'a str>) -> ChatHeadersParams<'a> {
        ChatHeadersParams {
            uid: "u1",
            token: "tok",
            realm,
            product: PRODUCT_DESKTOP,
            conversation_id: None,
            device_token,
            enterprise_id: "",
            tenant_id: "",
            department: "",
        }
    }

    #[test]
    fn chat_headers_fixed_fields_and_language() {
        let h = chat_request_headers(&chat_params("intl", None));
        assert_eq!(h["Content-Type"], "application/json");
        assert_eq!(h["Accept"], "application/json, text/plain, */*");
        assert_eq!(h["X-Requested-With"], "XMLHttpRequest");
        assert_eq!(h["Origin"], "https://www.workbuddy.ai");
        assert_eq!(h["Referer"], "https://www.workbuddy.ai/");
        assert_eq!(h["X-CodeBuddy-Request"], "1");
        assert_eq!(h["Accept-Language"], "en-US");
        assert_eq!(h["X-Machine-ID"], "1c77b9ac7fdc6e92cc94595f68a05717");
        assert_eq!(h["X-Session-ID"], "d4bf0937989e4311fdd68b54baf87841");
        assert_eq!(h["X-Domain"], "www.workbuddy.ai");
        let cn = chat_request_headers(&chat_params("cn", None));
        assert_eq!(cn["Accept-Language"], "zh-CN");
        assert_eq!(cn["Origin"], "https://www.codebuddy.cn");
        assert_eq!(cn["X-Domain"], "copilot.tencent.com");
    }

    #[test]
    fn chat_headers_device_token_optional() {
        let none = chat_request_headers(&chat_params("intl", None));
        assert!(!none.contains_key("X-Device-Token"));
        let some = chat_request_headers(&chat_params("intl", Some("  dt-1  ")));
        assert_eq!(some["X-Device-Token"], "dt-1");
    }
}
