//! 请求鉴权，移植自 wb_proxy._key_ok（:15558-15569）、_supplied_key
//! （:15517-15555）、identify_key（:427-434）与 wb_settings.PanelSessions
//! （:1598-1626）。
//!
//! key_ok 的三条路径与 Python 逐条一致：
//! ① panel 会话 token（X-Panel-Token header，内存表）
//! ② API key 匹配（identify_key：面板 key 表；面板为空时 launcher API_KEY）
//! ③ 无需鉴权（auth_required / auth_disabled）
//!
//! M2-a 为只读影子实例：三条路径均不落盘、不发放真实面板会话（create 仅
//! 供测试与 M5 面板登录接线），与 spec「影子只读 / dry-run」一致。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::header;
use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use wb_storage::{auth_required, match_api_key, KeyEntry};

use crate::Config;

/// 浮点 Unix 秒（对齐 Python time.time()，避免整数秒精度吞掉亚秒 TTL）。
fn unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 面板会话 token 表（对齐 wb_settings.PanelSessions :1598-1626）。
/// token→过期 Unix 秒；不持久化，重启即登出。
pub struct PanelSessions {
    ttl_secs: u64,
    tokens: Mutex<HashMap<String, f64>>,
}

impl PanelSessions {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// 发放会话 token，等价 Python `secrets.token_urlsafe(24)`：24 字节
    /// CSPRNG → base64url 无填充（32 字符），登记过期时间后返回。
    pub fn create(&self) -> String {
        let mut bytes = [0u8; 24];
        getrandom::getrandom(&mut bytes).expect("os CSPRNG unavailable");
        let token = URL_SAFE_NO_PAD.encode(bytes);
        self.tokens
            .lock()
            .expect("panel lock poisoned")
            .insert(token.clone(), unix_secs() + self.ttl_secs as f64);
        token
    }

    /// token 存在且未过期；过期即弹出（对齐 Python：过期才移除）。
    pub fn valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let now = unix_secs();
        let mut tokens = self.tokens.lock().expect("panel lock poisoned");
        match tokens.get(token) {
            Some(&expiry) if expiry >= now => true,
            Some(&expiry) => {
                tokens.remove(token);
                let _ = expiry;
                false
            }
            None => false,
        }
    }
}

/// 提取调用方呈现的 key（对齐 wb_proxy._supplied_key :15517-15555）。
///
/// 顺序与 Python 一致：Authorization（Bearer 大小写不敏感 + 引号容错）→
/// x-api-key / api-key / x-auth-token 头 → 查询串 key / api_key / api-key。
pub fn supplied_key(headers: &HeaderMap, query: &HashMap<String, String>) -> String {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    let mut supplied = String::new();
    if !raw.is_empty() {
        match raw.split_once(' ') {
            Some((scheme, value)) if scheme.trim().eq_ignore_ascii_case("bearer") => {
                supplied = value.trim().to_string();
            }
            _ => {
                // 无 scheme 或非 Bearer：整 header 作为 key（partition(" ")
                // 无空格/非 Bearer 时均返回 header 本身）
                supplied = raw.to_string();
            }
        }
        // 容错引号包裹的凭据（部分 SDK 会加）
        if supplied.len() >= 2 {
            let first = supplied.chars().next().unwrap();
            let last = supplied.chars().last().unwrap();
            if (first == '"' || first == '\'') && first == last {
                supplied = supplied[1..supplied.len() - 1].trim().to_string();
            }
        }
    }
    if !supplied.is_empty() {
        return supplied;
    }
    for name in ["x-api-key", "api-key", "x-auth-token"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    // 浏览器无法在顶层导航设置 header，面板跨设备打开时回退到查询串。
    for name in ["key", "api_key", "api-key"] {
        if let Some(v) = query.get(name).map(|s| s.trim()) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

/// 匹配 key 条目（对齐 wb_proxy.identify_key :427-434）。
///
/// 面板 key 表非空时只认面板 key（防止 launcher .bat 里的 key 在面板锁定后
/// 仍然生效）；面板配置为空时才把环境 API_KEY 作为 launcher key 兜底。
pub fn identify_key(
    accounts_dir: &Path,
    supplied: &str,
    api_key: Option<&str>,
) -> Option<KeyEntry> {
    let configured = wb_storage::api_keys(accounts_dir);
    let extra: Vec<&str> = if configured.is_empty() {
        match api_key {
            Some(k) if !k.is_empty() => vec![k],
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    match_api_key(accounts_dir, supplied, &extra)
}

/// 请求是否授权（对齐 wb_proxy._key_ok :15558-15569 的三条路径）。
pub fn key_ok(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    cfg: &Config,
    panels: &PanelSessions,
) -> bool {
    let panel_token = headers
        .get("x-panel-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if panels.valid(&panel_token) {
        return true;
    }
    let supplied = supplied_key(headers, query);
    if identify_key(&cfg.accounts_dir, &supplied, cfg.api_key.as_deref()).is_some() {
        return true;
    }
    !auth_required(&cfg.accounts_dir, cfg.api_key.as_deref(), cfg.lan)
}
