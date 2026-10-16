//! 集成测试共享辅助：生成与 Python tests/ 相同形状的合成 JWT。
//!
//! 只用合成值（{iss, sub, exp}），绝不接触真实凭据。签名段是占位 `sig`，
//! 与 wb-identity 一致——本 crate 只解码不验签。

use base64::Engine;

/// 构造 payload 去 padding 的 URL-safe base64 JWT（形状同 Python `jwt()`）。
pub fn jwt(iss: &str, sub: &str) -> String {
    let header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    let claims = serde_json::json!({"iss": iss, "sub": sub, "exp": 4102444800_u64});
    format!("{}.{}.sig", part(header), part(claims))
}

fn part(obj: serde_json::Value) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(obj.to_string().as_bytes())
}
