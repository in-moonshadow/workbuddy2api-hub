//! JWT 解码：只解码、不验签、不判过期；解析失败宽容返回空/0。
//!
//! 移植自 wb_accounts.py:220-247。Python 侧把 payload 段 base64url 解码并补
//! padding（`"=" * (-len(segment) % 4)`）；Rust 侧用 URL_SAFE_NO_PAD 引擎
//! 解码，等价于「去 padding + 补 padding」，语义相同。

use base64::Engine;
use serde_json::Value;

/// 解码 JWT 的 payload 段为 JSON 值；失败返回空对象（宽容，不拒绝账号）。
pub fn jwt_claims(token: &str) -> Value {
    let mut segments = token.split('.');
    let _header = segments.next();
    let payload = match segments.next() {
        Some(seg) => seg,
        None => return Value::Object(Default::default()),
    };
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        Err(_) => Value::Object(Default::default()),
    }
}

fn claim_str(token: &str, key: &str) -> String {
    jwt_claims(token)
        .get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default()
}

fn claim_i64(token: &str, key: &str) -> i64 {
    jwt_claims(token)
        .get(key)
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// `sub` → uid（Python: `str(_jwt_claims(token).get("sub") or "")`）。
pub fn jwt_uid(token: &str) -> String {
    claim_str(token, "sub")
}

/// `iss` → issuer。
pub fn jwt_issuer(token: &str) -> String {
    claim_str(token, "iss")
}

/// `exp` → 过期秒数。
pub fn jwt_exp(token: &str) -> i64 {
    claim_i64(token, "exp")
}

/// 归一化 epoch：>1e11 视为毫秒除以 1000；无法解析返回 0（宽容）。
pub fn normalize_epoch(value: f64) -> i64 {
    if value.is_nan() || value.is_infinite() {
        return 0;
    }
    let mut n = value;
    if n > 1e11 {
        n /= 1000.0;
    }
    n as i64
}
