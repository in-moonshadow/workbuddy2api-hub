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

/// `derive_id(uid, "req") + "-" + 6 位纳秒尾数`，对齐 wb_fingerprint.generate_request_id
/// （`:20-24`：`str(time.time_ns() % 1000000).zfill(6)`）。
///
/// 前缀稳定（同一 uid 永远同前缀），后缀每次不同——上游按「稳定前缀 + 变化后缀」
/// 识别同一设备的连续请求。时间源以参数注入，契约测试可据此断言固定值。
pub fn generate_request_id(uid: &str, now_nanos: u128) -> String {
    let suffix = (now_nanos % 1_000_000).to_string();
    format!("{}-{suffix:0>6}", derive_id(uid, "req"))
}

/// 生产用时间源：`SystemTime` 自 UNIX_EPOCH 起的纳秒数。
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// billing 出站头组合的输入，对齐 `Account.headers(purpose="billing")`（:1169-1196）。
pub struct BillingHeadersParams<'a> {
    pub uid: &'a str,
    pub token: &'a str,
    pub realm: &'a str,
    /// 账号自己的 `domain`；空串时回落 `cfg.domain`。
    pub domain: &'a str,
    pub enterprise_id: &'a str,
    pub device_token: Option<&'a str>,
}

/// 对齐 `Account.headers(purpose="billing")`（wb_accounts.py:1169-1196）。
///
/// 计费端点不吃 chat 那套身分头：没有 X-IDE-* / X-Agent-*，User-Agent 是轻量
/// `WorkBuddy/x.y.z`；无企业时打 `X-No-Enterprise-Id: 1`（chat 侧是缺席而非置 1）；
/// `X-Product: SaaS` 只在 cn 出现。`X-Request-ID` 沿用 generate_request_id 的
/// 「稳定前缀 + 变化后缀」，与 chat 侧的随机 32 hex 不同。
pub fn billing_request_headers(p: &BillingHeadersParams) -> HashMap<String, String> {
    let cfg = realm_config(p.realm);
    let mut headers = HashMap::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Accept".into(), "application/json, text/plain, */*".into());
    headers.insert("X-Requested-With".into(), "XMLHttpRequest".into());
    headers.insert("User-Agent".into(), cfg.billing_ua.into());
    headers.insert("Origin".into(), cfg.origin.into());
    headers.insert("Referer".into(), format!("{}/", cfg.origin));
    headers.insert("Authorization".into(), format!("Bearer {}", p.token));
    headers.insert("X-User-Id".into(), p.uid.into());
    headers.insert(
        "X-Domain".into(),
        if p.domain.trim().is_empty() {
            cfg.domain.to_string()
        } else {
            p.domain.trim().to_string()
        },
    );
    headers.insert("X-CodeBuddy-Request".into(), "1".into());
    // Python 是 `"en-US" if self.realm == "intl" else "zh-CN"`：非 intl 一律中文。
    headers.insert(
        "Accept-Language".into(),
        if p.realm == "intl" {
            "en-US".into()
        } else {
            "zh-CN".into()
        },
    );
    headers.insert(
        "X-Request-ID".into(),
        generate_request_id(p.uid, now_nanos()),
    );
    headers.insert("X-Machine-ID".into(), derive_id(p.uid, "machine"));
    headers.insert("X-Session-ID".into(), derive_id(p.uid, "session"));
    if p.enterprise_id.is_empty() {
        headers.insert("X-No-Enterprise-Id".into(), "1".into());
    } else {
        // billing 侧 tenant 恒等于 enterprise（chat 侧才允许 tenant 单独覆盖）。
        headers.insert("X-Enterprise-Id".into(), p.enterprise_id.into());
        headers.insert("X-Tenant-Id".into(), p.enterprise_id.into());
    }
    if p.realm == "cn" {
        headers.insert("X-Product".into(), "SaaS".into());
    }
    if let Some(token) = p.device_token.map(str::trim).filter(|t| !t.is_empty()) {
        headers.insert("X-Device-Token".into(), token.into());
    }
    headers
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

    // ---- billing 头（Account.headers(purpose="billing")） ----

    #[test]
    fn generate_request_id_shape_matches_python() {
        // 前 32 位是 derive_id(uid, "req")，后缀是纳秒数 % 1e6 补零到 6 位
        let id = generate_request_id("u1", 123_456_789);
        assert_eq!(id.len(), 32 + 1 + 6);
        assert_eq!(&id[..32], derive_id("u1", "req"));
        assert_eq!(&id[33..], "456789");
        // 尾数不足 6 位要补零（Python zfill(6)）
        let small = generate_request_id("u1", 42);
        assert_eq!(&small[33..], "000042");
        // 取模：纳秒数溢出百万位只保留后 6 位
        assert_eq!(&generate_request_id("u1", 1_000_042)[33..], "000042");
    }

    fn billing_params<'a>(
        realm: &'a str,
        domain: &'a str,
        enterprise_id: &'a str,
    ) -> BillingHeadersParams<'a> {
        BillingHeadersParams {
            uid: "u1",
            token: "tok",
            realm,
            domain,
            enterprise_id,
            device_token: None,
        }
    }

    #[test]
    fn billing_headers_cn_matches_python() {
        let h = billing_request_headers(&billing_params("cn", "", ""));
        assert_eq!(h["Content-Type"], "application/json");
        assert_eq!(h["Accept"], "application/json, text/plain, */*");
        assert_eq!(h["X-Requested-With"], "XMLHttpRequest");
        // 轻量 UA，不是 chat 那串产品串
        assert_eq!(h["User-Agent"], "WorkBuddy/5.5.6");
        assert_eq!(h["Origin"], "https://www.codebuddy.cn");
        assert_eq!(h["Referer"], "https://www.codebuddy.cn/");
        assert_eq!(h["Authorization"], "Bearer tok");
        assert_eq!(h["X-User-Id"], "u1");
        // domain 空串时回落 cfg.domain
        assert_eq!(h["X-Domain"], "copilot.tencent.com");
        assert_eq!(h["X-CodeBuddy-Request"], "1");
        assert_eq!(h["Accept-Language"], "zh-CN");
        assert_eq!(h["X-Machine-ID"], "1c77b9ac7fdc6e92cc94595f68a05717");
        assert_eq!(h["X-Session-ID"], "d4bf0937989e4311fdd68b54baf87841");
        // 无企业 → X-No-Enterprise-Id，且不下发 Enterprise/Tenant
        assert_eq!(h["X-No-Enterprise-Id"], "1");
        assert!(!h.contains_key("X-Enterprise-Id"));
        assert!(!h.contains_key("X-Tenant-Id"));
        // cn 才带 X-Product: SaaS
        assert_eq!(h["X-Product"], "SaaS");
        // 计费端点不吃 chat 身分头
        assert!(!h.contains_key("X-IDE-Type"));
        assert!(!h.contains_key("X-Agent-Intent"));
        assert!(!h.contains_key("X-Product-Version"));
        // request-id 前缀稳定、后缀 6 位数字
        let rid = &h["X-Request-ID"];
        assert_eq!(&rid[..32], derive_id("u1", "req"));
        assert_eq!(rid.len(), 39);
    }

    #[test]
    fn billing_headers_intl_and_overrides() {
        let h = billing_request_headers(&billing_params("intl", "www.workbuddy.ai", ""));
        assert_eq!(h["User-Agent"], "WorkBuddy/5.5.2");
        assert_eq!(h["Origin"], "https://www.workbuddy.ai");
        assert_eq!(h["Accept-Language"], "en-US");
        // intl 不打 X-Product
        assert!(!h.contains_key("X-Product"));

        // 账号自带 domain 优先
        let d = billing_request_headers(&billing_params("cn", " custom.example.com ", ""));
        assert_eq!(d["X-Domain"], "custom.example.com");

        // 有企业：Enterprise/Tenant 同时下发（billing 侧 tenant 恒等于 enterprise）
        let e = billing_request_headers(&billing_params("cn", "", "ent-1"));
        assert_eq!(e["X-Enterprise-Id"], "ent-1");
        assert_eq!(e["X-Tenant-Id"], "ent-1");
        assert!(!e.contains_key("X-No-Enterprise-Id"));
    }

    #[test]
    fn billing_headers_device_token_optional() {
        let mut p = billing_params("cn", "", "");
        assert!(!billing_request_headers(&p).contains_key("X-Device-Token"));
        p.device_token = Some("  dt-1  ");
        assert_eq!(billing_request_headers(&p)["X-Device-Token"], "dt-1");
    }
}
