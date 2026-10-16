//! 上游错误响应分类（纯函数），移植自 wb_proxy.py：
//! - :12212-12251 SOFT_LIMIT_MARKERS/ACCOUNT_SCOPE_PHRASES/credential_scope_phrase/
//!   rate_limit_code（soft-limit 形状与作用域判定）
//! - :12280-12314 CAP_LIMIT_CODE/rate_limit_is_cap/rate_limit_is_account_level
//! - :12317-12342 RESTRICTED_403_*/restriction_is_account_level
//! - :12345-12374 parse_rate_limit_reset
//!
//! 均为纯函数，不发起网络请求；detail 由调用方截断（Python 读 600 字节）。

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// SOFT_LIMIT_CODES（wb_proxy.py:12214）：仅 intl 实测的 14003。
pub const SOFT_LIMIT_CODES: [i64; 1] = [14003];

/// CAP_LIMIT_CODE（wb_proxy.py:12280）：6004 是唯一带重置墙钟的额度判定。
pub const CAP_LIMIT_CODE: i64 = 6004;

/// RESTRICTED_403_COOLDOWN（wb_proxy.py:12320）：风控限制账号的冷却 7200s。
pub const RESTRICTED_403_COOLDOWN: f64 = 7200.0;

/// SOFT_LIMIT_MARKERS（wb_proxy.py:12212-12213）。
const SOFT_LIMIT_MARKERS: [&str; 3] = [
    "usage exceeds frequency limit",
    "too many requests",
    "请求过于频繁",
];

/// ACCOUNT_SCOPE_PHRASES（wb_proxy.py:12223-12233）：ASCII 短语 `\b` 词界锚定
/// （大小写不敏感），中文短语纯子串。保持原文顺序（首个命中者赢）。
const ACCOUNT_SCOPE_PHRASES: [&str; 31] = [
    "account-level",
    "account level",
    "account-wide",
    "account scope",
    "account quota",
    "account rate limit",
    "account limit",
    "per account",
    "per-account",
    "this account",
    "your account",
    "the account",
    "an account",
    "credential-level",
    "credential level",
    "credential scope",
    "credential limit",
    "per credential",
    "this credential",
    "the credential",
    "your credential",
    "账号级",
    "账户级",
    "该账号",
    "此账号",
    "该账户",
    "此账户",
    "账号限流",
    "账户限流",
    "凭证级",
    "该凭证",
];

/// _RESTRICTION_403_MARKERS（wb_proxy.py:12326）：仅实测用语，不猜。
const RESTRICTION_403_MARKERS: [&str; 3] = ["unusual activity", "账号存在异常行为", "已被限制"];

/// 主匹配（wb_proxy.py:12359-12360）：intl「reset at」/ cn「将在」后跟墙钟。
static RESET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:reset at|将在)\s*(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})").unwrap()
});

/// 时区（wb_proxy.py:12364）：`UTC[+-]HH` 或 `UTC[+-]HH:MM`。
static TZ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"UTC([+-]\d{1,2})(?::?(\d{2}))?").unwrap());

/// 风控 code 正则（wb_proxy.py:12325）：`"code"` 后跟 11140（引号可有可无）。
static RESTRICTED_403_CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""code"\s*:\s*"?11140"?"#).unwrap());

/// rate_limit_code（wb_proxy.py:12254-12260）：JSON 429 正文的 `code` 字段
/// 原值（int 或 str），非 JSON / 非 dict / 无该字段 → None。
pub fn rate_limit_code(detail: &str) -> Option<Value> {
    if detail.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(detail) {
        Ok(Value::Object(obj)) => obj.get("code").cloned(),
        _ => None,
    }
}

/// rate_limit_is_soft_shape（wb_proxy.py:12263-12274）：429 正文是否命名一个
/// 速率限制（不论作用域）：小写子串命中 SOFT_LIMIT_MARKERS，或 code 是整数
/// 14003。**字符串 "14003" 不算**（对齐 Python `in` tuple）。
pub fn rate_limit_is_soft_shape(detail: &str) -> bool {
    let lowered = detail.to_lowercase();
    if SOFT_LIMIT_MARKERS.iter().any(|m| lowered.contains(m)) {
        return true;
    }
    rate_limit_code(detail).and_then(|v| v.as_i64()) == Some(SOFT_LIMIT_CODES[0])
}

/// rate_limit_is_cap（wb_proxy.py:12283-12293）：仅上游额度判定 code 6004。
/// 字符串形式也认（int() 容错）；不可解析 → false（持久化禁用 fail-closed）。
pub fn rate_limit_is_cap(detail: &str) -> bool {
    match rate_limit_code(detail) {
        Some(Value::Number(n)) => n.as_i64() == Some(CAP_LIMIT_CODE),
        Some(Value::String(s)) => s.parse::<i64>().ok() == Some(CAP_LIMIT_CODE),
        _ => false,
    }
}

/// credential_scope_phrase（wb_proxy.py:12236-12251）：正文点名的作用域短语，
/// 无 → None。ASCII 词 `\b` 锚定（`accountId` 字段名不命中），中文纯子串。
pub fn credential_scope_phrase(detail: &str) -> Option<&'static str> {
    let lowered = detail.to_lowercase();
    for phrase in ACCOUNT_SCOPE_PHRASES {
        if phrase.is_ascii() {
            // \b 在非 ASCII 上下文可能错位，但本表只用于 ASCII 短语
            let re = Regex::new(&format!(r"\b{}\b", regex::escape(phrase))).unwrap();
            if re.is_match(&lowered) {
                return Some(phrase);
            }
        } else if lowered.contains(phrase) {
            return Some(phrase);
        }
    }
    None
}

/// rate_limit_is_account_level（wb_proxy.py:12296-12314）：仅当正文**自称**
/// 凭证级才有账号级证据：reset_at 非空 → 模型级；否则需作用域短语。
/// 缺省 reset_at 不是证据（最常见的 429 正文不带时钟）。
pub fn rate_limit_is_account_level(detail: &str, reset_at: Option<f64>) -> bool {
    if reset_at.is_some() {
        return false;
    }
    credential_scope_phrase(detail).is_some()
}

/// restriction_is_account_level（wb_proxy.py:12329-12342）：403 正文是否说
/// 账号被限制（非内容审核）：code 11140（正则匹配，requestId hex 不误撞）或
/// 小写命中 markers。空/None → false。
pub fn restriction_is_account_level(detail: &str) -> bool {
    if detail.is_empty() {
        return false;
    }
    if RESTRICTED_403_CODE_RE.is_match(detail) {
        return true;
    }
    let lowered = detail.to_lowercase();
    RESTRICTION_403_MARKERS.iter().any(|m| lowered.contains(m))
}

/// parse_rate_limit_reset（wb_proxy.py:12345-12374）：从 429 正文抽出重置墙钟。
///
/// 双语言形式：intl「...will reset at 2026-09-19 18:29:03 UTC+8...」、
/// cn「...将在 2026-10-09 14:44:59 UTC+8 重置...」。无命中 / 不可解析 → None
/// （tolerant：正文异常不得打断请求路径）。
///
/// 时区语义：Python 的 `time.mktime(stamp) - time.timezone` 恰好抵消本地时区，
/// 净效果 = 文本墙钟**按 UTC 解析**再减 `UTC±N` 偏移（例：`18:29:03 UTC+8`
/// → `10:29:03 UTC`）。UTC 缺省 → offset 0。负偏移带分钟：
/// `hours*3600 + (minutes*60 if hours>=0 else -minutes*60)`。
pub fn parse_rate_limit_reset(detail: &str) -> Option<f64> {
    if detail.is_empty() {
        return None;
    }
    let stamp = RESET_RE
        .captures(detail)?
        .get(1)?
        .as_str()
        .replace('T', " ");
    let naive = chrono::NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let base = naive.and_utc().timestamp() as f64; // 文本墙钟当 UTC epoch
    let offset = match TZ_RE.captures(detail) {
        Some(caps) => {
            let hours = caps.get(1)?.as_str().parse::<i64>().ok()?;
            // 无分钟段 → 0（对齐 Python `tz.group(2) or 0`）
            let minutes = match caps.get(2) {
                Some(m) => m.as_str().parse::<i64>().ok()?,
                None => 0,
            };
            hours * 3600
                + if hours >= 0 {
                    minutes * 60
                } else {
                    -minutes * 60
                }
        }
        None => 0,
    };
    Some(base - offset as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 参照值来自真实 Python（fetch_upstream_refs.py，2026-10-16）----

    #[test]
    fn parse_reset_matches_python_refs() {
        assert_eq!(
            parse_rate_limit_reset("your usage will reset at 2026-09-19 18:29:03 UTC+8"),
            Some(1_789_813_743.0) // 2026-09-19 10:29:03 UTC
        );
        assert_eq!(
            parse_rate_limit_reset("额度已用完，将在 2026-10-09 14:44:59 UTC+8 重置"),
            Some(1_791_528_299.0) // 2026-10-09 06:44:59 UTC
        );
        // 无时区 → 文本墙钟当 UTC
        assert_eq!(
            parse_rate_limit_reset("reset at 2026-09-19 18:29:03"),
            Some(1_789_842_543.0) // 2026-09-19 18:29:03 UTC
        );
        // 负偏移带分钟：UTC-5:30 → 03:00 + 5.5h = 08:30 UTC
        assert_eq!(
            parse_rate_limit_reset("reset at 2026-01-02 03:00:00 UTC-5:30"),
            Some(1_767_342_600.0)
        );
        // `T` 分隔同样接受
        assert_eq!(
            parse_rate_limit_reset("reset at 2026-09-19T18:29:03 UTC+8"),
            Some(1_789_813_743.0)
        );
        // tolerant：不可解析 / 空 → None
        assert_eq!(parse_rate_limit_reset("throttled: nginx 429"), None);
        assert_eq!(parse_rate_limit_reset(""), None);
    }

    #[test]
    fn rate_limit_code_preserves_type() {
        assert_eq!(
            rate_limit_code(r#"{"code":6004}"#),
            Some(Value::Number(6004.into()))
        );
        assert_eq!(
            rate_limit_code(r#"{"code":"14003"}"#),
            Some(Value::String("14003".into()))
        );
        assert_eq!(rate_limit_code(r#"{"no":"code"}"#), None);
        assert_eq!(rate_limit_code("x"), None);
        assert_eq!(rate_limit_code(""), None);
    }

    #[test]
    fn soft_shape_markers_and_int_code() {
        assert!(rate_limit_is_soft_shape("usage exceeds frequency limit"));
        assert!(rate_limit_is_soft_shape(r#"{"code":14003}"#));
        assert!(!rate_limit_is_soft_shape(r#"{"code":"14003"}"#)); // str 不算
        assert!(!rate_limit_is_soft_shape("some other 429 text"));
        assert!(!rate_limit_is_soft_shape(""));
    }

    #[test]
    fn cap_detects_6004_with_string_tolerance() {
        assert!(rate_limit_is_cap(r#"{"code":6004,"msg":"x"}"#));
        assert!(rate_limit_is_cap(r#"{"code":"6004"}"#));
        assert!(!rate_limit_is_cap(r#"{"code":429}"#));
        assert!(!rate_limit_is_cap("not json"));
        assert!(!rate_limit_is_cap(""));
    }

    #[test]
    fn scope_phrase_word_boundary_and_chinese() {
        assert_eq!(
            credential_scope_phrase("you have exceeded your account rate limit"),
            Some("account rate limit")
        );
        // accountId 是字段名，非词「account」+ 无边界短语 → None
        assert_eq!(
            credential_scope_phrase("accountId abc shows throttle"),
            None
        );
        assert_eq!(
            credential_scope_phrase("该账号已限流，请稍后再试"),
            Some("该账号")
        );
        assert_eq!(
            credential_scope_phrase("your account is fine"),
            Some("your account")
        );
        assert_eq!(credential_scope_phrase("throttled for model gpt-5"), None);
        assert_eq!(credential_scope_phrase(""), None);
    }

    #[test]
    fn account_level_needs_positive_evidence() {
        assert!(rate_limit_is_account_level(
            "your account was rate limited",
            None
        ));
        assert!(!rate_limit_is_account_level(
            "model throttled",
            Some(1000.0)
        ));
        assert!(!rate_limit_is_account_level("model throttled", None));
        assert!(!rate_limit_is_account_level("", None));
    }

    #[test]
    fn restricted_403_recognition() {
        assert!(restriction_is_account_level(
            r#"{"code":11140,"msg":"request illegal"}"#
        ));
        assert!(restriction_is_account_level(
            "Your account has been restricted due to unusual activity"
        ));
        assert!(restriction_is_account_level(
            "账号存在异常行为，已被限制使用"
        ));
        assert!(!restriction_is_account_level("content policy violation"));
        assert!(!restriction_is_account_level(""));
    }
}
