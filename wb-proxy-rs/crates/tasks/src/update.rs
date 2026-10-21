//! M5 数据侧：更新发现（**只发现，不安装**）。
//!
//! 权威源：Python `wb_updates.py`。
//!
//! 回答一个问题：官方仓库有没有比当前运行版本更新的稳定版？
//! **不下载、不解包、不替换文件、不重启进程**——那些属于后续阶段。
//!
//! 三条硬约束：
//! 1. **默认关闭**。每日检查是 operator 显式打开的偏好；缺键、false 都是关闭，
//!    关闭时后台一个请求都不发。
//! 2. **失败不致命**。网络超时、HTTP 错误、JSON 坏了都只记一行短原因。
//! 3. **只留安全运维字段**。URL、请求头、响应体一律不落。

use serde_json::{json, Value};

/// 官方仓库的 releases 接口——主路径。
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/ardeyouxipianyi/workbuddy2api-hub/releases";
/// 回退路径：网页端点的 302 Location 末尾就是最新稳定版 tag。
///
/// 它按网页计流量，不受 API 那 60 次/小时/出口 IP 的匿名配额限制。
pub const LATEST_RELEASE_URL: &str =
    "https://github.com/ardeyouxipianyi/workbuddy2api-hub/releases/latest";
/// 可选凭据的环境变量名（只从环境变量读，绝不落盘）。
pub const TOKEN_ENV: &str = "GITHUB_TOKEN";
/// 请求的 User-Agent。
pub const USER_AGENT: &str = "wb2api-hub-update-check";
/// 请求超时（秒）。
pub const REQUEST_TIMEOUT: f64 = 5.0;
/// 响应体上限（一份 releases 文档几十 KB；坏响应不能把内存拉满）。
pub const MAX_BODY: usize = 2 * 1024 * 1024;

/// 每日检查的节流窗口（约 24 小时）。
pub const SCHEDULED_INTERVAL_SECONDS: f64 = 24.0 * 3600.0;
/// 启动后先等一会儿再花掉第一个请求。
pub const FIRST_DELAY_SECONDS: f64 = 30.0;
/// 后台线程醒来的间隔（真正的节流靠 [`due`] 判断）。
pub const POLL_SECONDS: f64 = 300.0;

/// 解析 `vX.Y.Z` / `X.Y.Z` → `(major, minor, patch)`；其它一律 `None`。
///
/// 只认这种能排序的稳定版本号：带后缀的（`-rc1`）、日期 tag、移动分支名
/// 一律跳过——猜一个顺序比不报更糟。
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let mut parts = text.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if major.is_empty()
        || minor.is_empty()
        || patch.is_empty()
        || !major.bytes().all(|b| b.is_ascii_digit())
        || !minor.bytes().all(|b| b.is_ascii_digit())
        || !patch.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((
        major.parse().ok()?,
        minor.parse().ok()?,
        patch.parse().ok()?,
    ))
}

/// `candidate` 是否严格新于 `current`；任一边不可解析就是 `false`。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(left), Some(right)) => left > right,
        _ => false,
    }
}

/// 发布页链接，或空串。
///
/// 这个值来自远端文档，会被面板原样显示：只放行 GitHub 的 https 发布页这一种
/// 形状，别的一律丢掉。
pub fn safe_url(value: &str) -> String {
    let text = value.trim();
    if !text.starts_with("https://github.com/") {
        return String::new();
    }
    text.chars().take(300).collect()
}

/// 从一份 releases 文档里挑出最新的稳定版，没有就返回 `None`。
///
/// 草稿与预发布跳过；tag 不是纯 `vX.Y.Z` 的跳过；取版本号最大的那个而不是
/// 列表里第一个——重新发布的旧 tag 不能靠排序取胜。
pub fn pick_latest(releases: &[Value], current_version: &str) -> Option<Value> {
    let mut best: Option<((u32, u32, u32), &Value)> = None;
    for entry in releases {
        if !entry.is_object() {
            continue;
        }
        if entry.get("draft").and_then(Value::as_bool).unwrap_or(false)
            || entry
                .get("prerelease")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let tag = entry.get("tag_name").and_then(Value::as_str).unwrap_or("");
        let version = match parse_version(tag) {
            Some(v) => v,
            None => continue,
        };
        let replace = match &best {
            Some((v, _)) => version > *v,
            None => true,
        };
        if replace {
            best = Some((version, entry));
        }
    }
    let (version, entry) = best?;
    let text = format!("{}.{}.{}", version.0, version.1, version.2);
    Some(json!({
        "version": text,
        "tag": entry.get("tag_name").and_then(Value::as_str).unwrap_or("").trim().chars().take(64).collect::<String>(),
        "url": safe_url(entry.get("html_url").and_then(Value::as_str).unwrap_or("")),
        "published_at": entry.get("published_at").and_then(Value::as_str).unwrap_or("").trim().chars().take(32).collect::<String>(),
        "newer": is_newer(&text, current_version),
    }))
}

/// 一行能安全外发的失败原因。
///
/// 异常消息里可能带着完整 URL 和响应体，两者都不该进 settings.json 或日志，
/// 所以这里只取类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 某个 HTTP 状态码。
    Http(u16),
    /// 响应不是 JSON。
    InvalidJson,
    /// 响应结构不对。
    UnexpectedDocument,
    /// 网络层失败。
    Network,
    /// 其他。
    Unknown,
}

impl FailureKind {
    /// 可安全外发的短原因。
    pub fn describe(&self) -> String {
        match self {
            FailureKind::Http(code) => format!("HTTP {code}"),
            FailureKind::InvalidJson => "invalid JSON".to_string(),
            FailureKind::UnexpectedDocument => "unexpected release document".to_string(),
            FailureKind::Network => "network error".to_string(),
            FailureKind::Unknown => "unknown error".to_string(),
        }
    }
}

/// 约 24 小时的节流窗口是否已经放行。
pub fn due(now: f64, last_attempt: f64) -> bool {
    if last_attempt == 0.0 {
        return true;
    }
    (now - last_attempt) >= SCHEDULED_INTERVAL_SECONDS
}

/// 上次检查的时刻（本地时间文本）；`0` 表示没查过。
pub fn stamp(value: f64) -> Option<String> {
    if value == 0.0 {
        return None;
    }
    Some(crate::credits::local_text(value))
}

/// 面板要的更新检查状态。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatus {
    pub current_version: String,
    /// 最新稳定版；没查过或没查到时为空。
    pub latest_version: Option<String>,
    pub update_available: bool,
    /// 每日检查的偏好；缺键或 false 都是关闭。
    pub enabled: bool,
    /// 当前是否正在检查。
    pub checking: bool,
    pub last_attempt: Option<String>,
    pub last_success: Option<String>,
    pub last_error: String,
    pub release_url: String,
    pub published_at: String,
}

impl UpdateStatus {
    /// Python `status()` 的键序。
    pub fn to_json(&self) -> Value {
        json!({
            "current_version": self.current_version,
            "latest_version": self.latest_version.clone(),
            "update_available": self.update_available,
            "enabled": self.enabled,
            "checking": self.checking,
            "last_attempt": self.last_attempt.clone(),
            "last_success": self.last_success.clone(),
            "last_error": self.last_error,
            "release_url": self.release_url,
            "published_at": self.published_at,
        })
    }
}

/// 从一次检查的结论构造状态。
pub fn build_status(
    current_version: &str,
    latest: Option<&Value>,
    enabled: bool,
    checking: bool,
    last_attempt: f64,
    last_success: f64,
    last_error: &str,
) -> UpdateStatus {
    let picked = latest.cloned().unwrap_or(json!({}));
    let latest_version = picked
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    UpdateStatus {
        current_version: current_version.to_string(),
        update_available: is_newer(&latest_version, current_version),
        latest_version: if latest_version.is_empty() {
            None
        } else {
            Some(latest_version)
        },
        enabled,
        checking,
        last_attempt: stamp(last_attempt),
        last_success: stamp(last_success),
        last_error: last_error.to_string(),
        release_url: picked
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        published_at: picked
            .get("published_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

/// 从网页端点的最终地址里取出 tag。
///
/// `releases/latest` 会 302 到 `/releases/tag/<tag>`；tag 直接从 URL 里取——
/// 不解析 HTML，页面改版也不会让检查失效。
pub fn tag_from_redirect(final_url: &str) -> Option<String> {
    let marker = "/releases/tag/";
    let idx = final_url.find(marker)?;
    let rest = &final_url[idx + marker.len()..];
    let tag: String = rest
        .chars()
        .take_while(|c| !matches!(c, '/' | '?' | '#'))
        .collect();
    if tag.is_empty() {
        None
    } else {
        Some(tag)
    }
}

/// 由回退路径构造的一条 releases 条目。
pub fn fallback_release(final_url: &str) -> Option<Value> {
    let tag = tag_from_redirect(final_url)?;
    Some(json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": false,
        "html_url": final_url,
        "published_at": "",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_plain_semver() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3-rc1"), None);
        assert_eq!(parse_version("2026-10-20"), None);
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn newer_requires_both_sides_to_parse() {
        assert!(is_newer("v1.2.4", "v1.2.3"));
        assert!(!is_newer("v1.2.3", "v1.2.3"));
        assert!(!is_newer("v1.2.2", "v1.2.3"));
        assert!(!is_newer("v1.2.3", "not-a-version"));
        assert!(!is_newer("not-a-version", "v1.2.3"));
    }

    #[test]
    fn safe_url_only_allows_github_release_pages() {
        assert_eq!(
            safe_url("https://github.com/a/b/releases/tag/v1.0.0"),
            "https://github.com/a/b/releases/tag/v1.0.0"
        );
        assert_eq!(safe_url("http://github.com/a/b"), "");
        assert_eq!(safe_url("https://evil.example/a"), "");
        assert_eq!(safe_url(""), "");
        assert_eq!(
            safe_url(&("https://github.com/".to_string() + &"x".repeat(400))).len(),
            300
        );
    }

    #[test]
    fn pick_latest_skips_drafts_prereleases_and_bad_tags() {
        let releases = vec![
            json!({"tag_name": "v1.0.0", "html_url": "https://github.com/a/b/releases/tag/v1.0.0"}),
            json!({"tag_name": "v2.0.0-rc1", "html_url": "https://github.com/a/b"}),
            json!({"tag_name": "v3.0.0", "draft": true}),
            json!({"tag_name": "nightly", "html_url": "https://github.com/a/b"}),
            json!({"tag_name": "v1.5.0", "html_url": "https://github.com/a/b/releases/tag/v1.5.0",
                   "published_at": "2026-10-01T00:00:00Z"}),
        ];
        let picked = pick_latest(&releases, "v1.0.0").expect("must pick one");
        assert_eq!(picked["version"], "1.5.0");
        assert_eq!(picked["newer"], true);
        assert_eq!(picked["published_at"], "2026-10-01T00:00:00Z");
    }

    #[test]
    fn pick_latest_returns_none_when_nothing_qualifies() {
        let releases = vec![json!({"tag_name": "nightly"}), json!({"draft": true})];
        assert!(pick_latest(&releases, "v1.0.0").is_none());
    }

    #[test]
    fn due_respects_the_24_hour_window() {
        let now = 1_800_000_000.0;
        assert!(due(now, 0.0));
        assert!(!due(now, now - 60.0));
        assert!(due(now, now - 25.0 * 3600.0));
    }

    #[test]
    fn tag_is_read_from_the_redirect_target() {
        assert_eq!(
            tag_from_redirect("https://github.com/a/b/releases/tag/v1.2.3"),
            Some("v1.2.3".to_string())
        );
        assert_eq!(tag_from_redirect("https://github.com/a/b"), None);
        let entry = fallback_release("https://github.com/a/b/releases/tag/v1.2.3").unwrap();
        assert_eq!(entry["tag_name"], "v1.2.3");
        assert_eq!(entry["draft"], false);
    }

    #[test]
    fn status_shape_matches_the_python_keys() {
        let latest = json!({
            "version": "1.5.0", "tag": "v1.5.0",
            "url": "https://github.com/a/b/releases/tag/v1.5.0",
            "published_at": "2026-10-01T00:00:00Z"
        });
        let s = build_status(
            "1.0.0",
            Some(&latest),
            true,
            false,
            1_800_000_000.0,
            0.0,
            "",
        );
        let json = s.to_json();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "current_version",
                "latest_version",
                "update_available",
                "enabled",
                "checking",
                "last_attempt",
                "last_success",
                "last_error",
                "release_url",
                "published_at",
            ]
        );
        assert_eq!(json["update_available"], true);
        assert_eq!(json["latest_version"], "1.5.0");
        assert!(json["last_attempt"].is_string());
        assert!(json["last_success"].is_null());
    }

    #[test]
    fn failure_reasons_are_short_and_safe() {
        assert_eq!(FailureKind::Http(403).describe(), "HTTP 403");
        assert_eq!(FailureKind::InvalidJson.describe(), "invalid JSON");
        assert_eq!(
            FailureKind::UnexpectedDocument.describe(),
            "unexpected release document"
        );
        assert_eq!(FailureKind::Network.describe(), "network error");
    }
}
