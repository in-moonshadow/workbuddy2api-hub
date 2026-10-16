//! 上游端点表与 realm/product 规范化，移植自 wb_identity.py（_ENDPOINTS
//! :51-68、normalize_product :41-48、endpoint_for :70-75、domain_for_realm）。
//!
//! 要点：
//! - intl 三 product 全部走 www.workbuddy.ai（CLI 复用 workbuddy 边缘：
//!   www.codebuddy.ai 在实测网络 getaddrinfo 失败，系统解析器回 0.0.0.1 沉洞）
//! - cn desktop/cli → copilot.tencent.com；cn vscode → www.workbuddy.cn
//! - normalize_product 兜底 PRODUCT_DESKTOP（未知输入不报错，与原实现一致）

/// 与 wb_identity 相同的三产品值。
pub const PRODUCT_DESKTOP: &str = "workbuddy";
pub const PRODUCT_VSCODE: &str = "vscode";
pub const PRODUCT_CLI: &str = "cli";

/// 把各种写法收敛成 'workbuddy'/'vscode'/'cli'；未知一律回桌面端。
/// 对齐 wb_identity.normalize_product（:41-48）。
pub fn normalize_product(value: &str) -> &'static str {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "workbuddy" | "wb" | "desktop" => PRODUCT_DESKTOP,
        "vscode" | "vsc" | "ide" => PRODUCT_VSCODE,
        "cli" | "codebuddy" => PRODUCT_CLI,
        _ => PRODUCT_DESKTOP,
    }
}

/// `(realm, product)` → `(chat base URL, X-Domain)`，对齐 wb_identity._ENDPOINTS
/// 与 endpoint_for。realm 非 "cn" 一律视作 "intl"。
pub fn endpoint_for(realm: &str, product: &str) -> (&'static str, &'static str) {
    let is_cn = realm == "cn";
    match (is_cn, normalize_product(product)) {
        (true, PRODUCT_VSCODE) => ("https://www.workbuddy.cn", "www.workbuddy.cn"),
        (true, _) => ("https://copilot.tencent.com", "copilot.tencent.com"),
        (false, _) => ("https://www.workbuddy.ai", "www.workbuddy.ai"),
    }
}

/// X-Domain 的值（预设桌面端），对齐 wb_identity.domain_for_realm。
pub fn domain_for_realm(realm: &str) -> &'static str {
    endpoint_for(realm, PRODUCT_DESKTOP).1
}

/// 账号当前身分应打的上游 chat 端点，对齐 wb_accounts.Account.chat_base_url。
pub fn chat_base_url(realm: &str, product: &str) -> String {
    endpoint_for(realm, product).0.to_string()
}

/// realm 配置的 chat 相关字段，对齐 wb_accounts.REALM_CONFIGS（:173-190）。
/// cn 的 Origin 是 codebuddy.cn 而 X-Domain 是 copilot.tencent.com——两者不同源。
pub struct RealmConfig {
    pub origin: &'static str,
    pub chat_ua: &'static str,
}

/// get_realm_config 的等价：未知 realm 兜底 intl。
pub fn realm_config(realm: &str) -> RealmConfig {
    if realm == "cn" {
        RealmConfig {
            origin: "https://www.codebuddy.cn",
            chat_ua: "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1",
        }
    } else {
        RealmConfig {
            origin: "https://www.workbuddy.ai",
            chat_ua: "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_product_converges() {
        assert_eq!(normalize_product("workbuddy"), PRODUCT_DESKTOP);
        assert_eq!(normalize_product("WB"), PRODUCT_DESKTOP);
        assert_eq!(normalize_product("desktop"), PRODUCT_DESKTOP);
        assert_eq!(normalize_product("vscode"), PRODUCT_VSCODE);
        assert_eq!(normalize_product("VSC"), PRODUCT_VSCODE);
        assert_eq!(normalize_product("ide"), PRODUCT_VSCODE);
        assert_eq!(normalize_product("cli"), PRODUCT_CLI);
        assert_eq!(normalize_product("codebuddy"), PRODUCT_CLI);
        // 未知输入 → 桌面端兜底，不报错
        assert_eq!(normalize_product("whatever"), PRODUCT_DESKTOP);
        assert_eq!(normalize_product(""), PRODUCT_DESKTOP);
    }

    #[test]
    fn endpoint_table_matches_python() {
        // wb_identity._ENDPOINTS 六项
        assert_eq!(
            endpoint_for("intl", PRODUCT_DESKTOP),
            ("https://www.workbuddy.ai", "www.workbuddy.ai")
        );
        assert_eq!(
            endpoint_for("intl", PRODUCT_VSCODE),
            ("https://www.workbuddy.ai", "www.workbuddy.ai")
        );
        assert_eq!(
            endpoint_for("intl", PRODUCT_CLI),
            ("https://www.workbuddy.ai", "www.workbuddy.ai")
        );
        assert_eq!(
            endpoint_for("cn", PRODUCT_DESKTOP),
            ("https://copilot.tencent.com", "copilot.tencent.com")
        );
        assert_eq!(
            endpoint_for("cn", PRODUCT_VSCODE),
            ("https://www.workbuddy.cn", "www.workbuddy.cn")
        );
        assert_eq!(
            endpoint_for("cn", PRODUCT_CLI),
            ("https://copilot.tencent.com", "copilot.tencent.com")
        );
    }

    #[test]
    fn unknown_realm_falls_back_to_intl() {
        assert_eq!(
            endpoint_for("eu", PRODUCT_DESKTOP),
            ("https://www.workbuddy.ai", "www.workbuddy.ai")
        );
    }

    #[test]
    fn domain_for_realm_presets_desktop() {
        assert_eq!(domain_for_realm("cn"), "copilot.tencent.com");
        assert_eq!(domain_for_realm("intl"), "www.workbuddy.ai");
    }

    #[test]
    fn chat_base_url_is_endpoint_host() {
        assert_eq!(chat_base_url("cn", "vscode"), "https://www.workbuddy.cn");
        assert_eq!(chat_base_url("intl", "cli"), "https://www.workbuddy.ai");
    }

    #[test]
    fn realm_config_origin_differs_from_domain_for_cn() {
        // cn 的 Origin=codebuddy.cn 而 X-Domain=copilot.tencent.com（不同源）
        let cfg = realm_config("cn");
        assert_eq!(cfg.origin, "https://www.codebuddy.cn");
        let intl = realm_config("intl");
        assert_eq!(intl.origin, "https://www.workbuddy.ai");
        let unknown = realm_config("eu");
        assert_eq!(unknown.origin, intl.origin);
    }
}