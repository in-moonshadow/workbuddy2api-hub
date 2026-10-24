//! 面板口令认证与登录限流，移植自 Python：
//!
//! - `wb_settings.verify_panel_password`（:203-220）
//! - `wb_settings.set_panel_password`（:223-237）
//! - `wb_settings.panel_password_is_default`（:196-201）
//! - `wb_settings._digest`（:116-120，PBKDF2-HMAC-SHA256）
//! - `wb_settings.DEFAULT_PANEL_PASSWORD` / `PBKDF2_ROUNDS`（:20-21）
//! - `wb_proxy.login_rate_limit_key` / `peer_is_trusted` / `_proxy_header_ip`
//!   （:380-397）与 `parse_trusted_proxies`（:324-339）
//! - `wb_proxy._handle_panel`（:17036-17085）里的限流口径（5 次 / 60 秒）
//!
//! 口令摘要格式与 Python 完全一致：settings.json 里存 **hex 文本盐**
//! （16 字节 = 32 个 hex 字符）与 **小写 hex 摘要**，比对走恒定时间比较
//! （对齐 `hmac.compare_digest`）。
//!
//! ## 依赖 storage 的「整体替换」入口
//!
//! 改口令要能**删键**：Python 把口令改回默认值时会 `pop` 掉
//! `panel_password_salt` / `panel_password_rounds`（:225-227），浅合并的
//! `wb_storage::write_settings` 做不到（写 `null` 只是置空），这里用的是
//! `wb_storage::replace_settings`（整体替换，见 crates/storage/src/lib.rs）。
//!
//! ## 限流的请求顺序
//!
//! Python `_handle_panel`（:17040-17047）是「**先**看是否超限 → 再验口令 →
//! 失败才计数」。要逐条复刻这个顺序，调用方应先调
//! [`LoginLimiter::is_blocked`]，口令错误再调 [`LoginLimiter::record_failure`]
//! 并**忽略**它的返回值；只用 `record_failure` 也能限流，但触发点会早一次
//! （第 5 次失败就返回 429，Python 是第 6 次请求才 429）。

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;

use axum::http::HeaderMap;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use serde_json::{Map, Value};
use subtle::Choice;

/// 面板口令（Python `wb_settings.DEFAULT_PANEL_PASSWORD`，:20）。
pub const DEFAULT_PANEL_PASSWORD: &str = "admin";
/// PBKDF2 轮数（Python `wb_settings.PBKDF2_ROUNDS`，:21）。
pub const PBKDF2_ROUNDS: u32 = 120_000;

/// 登录失败计数窗口（秒），对齐 Python `_handle_panel` 里的 `now - t < 60`。
const LOGIN_WINDOW_SECS: f64 = 60.0;
/// 同一 IP 在窗口内允许的失败次数上限（Python `len(attempts) >= 5`）。
const LOGIN_MAX_FAILURES: usize = 5;
/// 代理头里 IP 文本的最大长度（Python `_PROXY_HEADER_IP_MAX`，:368）。
const PROXY_HEADER_IP_MAX: usize = 64;

// ---------------------------------------------------------------------------
// hex 编解码与 PBKDF2
// ---------------------------------------------------------------------------

/// 小写 hex 编码（对齐 Python `bytes.hex()`）。
fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// hex 文本解码，对齐 Python `bytes.fromhex`：
///
/// 只接受 `0-9a-fA-F`，**忽略 ASCII 空白**；奇数个 hex 位或出现非 hex 字符
/// 报错（Python 抛 `ValueError`，Rust 返回 `None`）。
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let mut nibbles: Vec<u8> = Vec::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        let value = match ch {
            '0'..='9' => ch as u8 - b'0',
            'a'..='f' => ch as u8 - b'a' + 10,
            'A'..='F' => ch as u8 - b'A' + 10,
            _ => return None,
        };
        nibbles.push(value);
    }
    if nibbles.len() % 2 != 0 {
        return None;
    }
    Some(
        nibbles
            .chunks(2)
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect(),
    )
}

/// PBKDF2-HMAC-SHA256 摘要（小写 hex），对齐 Python `wb_settings._digest`。
///
/// 盐按 **hex 文本** 传入（Python `bytes.fromhex(salt_hex)`）。
fn pbkdf2_hex(password: &str, salt_hex: &str, rounds: u32) -> Option<String> {
    let salt = decode_hex(salt_hex)?;
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, rounds, &mut out);
    Some(encode_hex(&out))
}

/// 恒定时间比较（对齐 Python `hmac.compare_digest`）。
///
/// 与 CPython 的差别：CPython 在**长度不同**时立即返回 False；这里不提前返回，
/// 而是按最大长度逐字节异或、把长度差也折进结论，避免用响应时间泄露内容。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff = (a.len() ^ b.len()) as u8;
    for index in 0..max_len {
        diff |= a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0);
    }
    // 用 subtle 的 Choice 收敛结论，避免比较结果被当成普通 bool 优化掉。
    Choice::from(u8::from(diff == 0)).unwrap_u8() == 1
}

// ---------------------------------------------------------------------------
// settings.json 读写（口令相关键）
// ---------------------------------------------------------------------------

/// 读 settings.json 顶层键值。
///
/// 文件缺失 / 损坏 / 不是对象时返回空表（Python `wb_settings.load` 同样把读不
/// 出来的 settings 当成空 dict 继续跑，不让服务起不来）。
fn read_settings_map(accounts_dir: &Path) -> Map<String, Value> {
    match wb_storage::read_settings(accounts_dir) {
        Ok(doc) => doc.as_object().cloned().unwrap_or_default(),
        Err(_) => Map::new(),
    }
}

/// **整体替换**写回 settings.json（可删键），对齐 Python `wb_settings.save`。
///
/// 用 storage 侧的 `replace_settings`（整体替换入口）：`wb_storage::write_settings`
/// 是**浅合并**，删不掉 `panel_password_salt` / `panel_password_rounds` 这两个键，
/// 而 Python 在把口令改回默认值时会 `pop` 掉它们（:225-227）。
fn write_settings_map(accounts_dir: &Path, map: &Map<String, Value>) -> Result<(), String> {
    wb_storage::replace_settings(accounts_dir, &Value::Object(map.clone()))
        .map_err(|err| format!("写 settings.json 失败: {err}"))
}

// ---------------------------------------------------------------------------
// 口令校验 / 设置 / 是否默认值
// ---------------------------------------------------------------------------

/// 解析 `panel_password_rounds`，对齐 Python
/// `int(data.get("panel_password_rounds") or PBKDF2_ROUNDS)`。
///
/// Python 语义要点：`or` 把 `None` / `False` / `0` / `0.0` / `""` 都当假值 →
/// 用默认轮数；其余值取整数。**非正轮数**会让 `hashlib.pbkdf2_hmac` 抛错，
/// Python 侧被 `except` 吞掉后校验失败，这里同样返回 `None`（校验失败）。
fn rounds_from(value: Option<&Value>) -> Option<u32> {
    let Some(value) = value else {
        return Some(PBKDF2_ROUNDS);
    };
    let falsy = match value {
        Value::Null => true,
        Value::Bool(flag) => !*flag,
        Value::Number(number) => number.as_f64().map(|value| value == 0.0).unwrap_or(false),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
    };
    if falsy {
        return Some(PBKDF2_ROUNDS);
    }
    let rounded: i64 = match value {
        Value::Bool(flag) => i64::from(*flag),
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                match unsigned.try_into() {
                    Ok(v) => v,
                    Err(_) => return None,
                }
            } else {
                number.as_f64().map(|float| float.trunc() as i64)?
            }
        }
        // Python 的 int("...")：允许首尾空白，不接受小数点。
        Value::String(text) => match text.trim().parse::<i64>() {
            Ok(parsed) => parsed,
            Err(_) => return None,
        },
        _ => return None,
    };
    if rounded <= 0 {
        return None;
    }
    if rounded > i64::from(u32::MAX) {
        return None;
    }
    Some(rounded as u32)
}

/// 校验面板口令。语义逐条对齐 Python `wb_settings.verify_panel_password`
/// （:203-220）。
///
/// - settings.json 里没有（或为空）`panel_password_hash` ⇒ 只认默认口令；
/// - `panel_password_default == true` ⇒ 只认默认口令；
/// - 否则按 `panel_password_salt` + `panel_password_rounds` 算 PBKDF2 摘要，
///   与存下来的 hex 摘要做**恒定时间**比较。
///
/// 盐不是合法 hex、轮数不是正整数等异常数据一律判为「校验不通过」
/// （Python 在这些情况下抛异常 / 返回 False，详见交付说明）。
pub fn verify_panel_password(accounts_dir: &std::path::Path, password: &str) -> bool {
    let data = read_settings_map(accounts_dir);
    let stored = data
        .get("panel_password_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if stored.is_empty() {
        return password == DEFAULT_PANEL_PASSWORD;
    }
    if data.get("panel_password_default").and_then(Value::as_bool) == Some(true) {
        return password == DEFAULT_PANEL_PASSWORD;
    }
    let salt = data
        .get("panel_password_salt")
        .and_then(Value::as_str)
        .unwrap_or("");
    if salt.is_empty() {
        return false;
    }
    let Some(rounds) = rounds_from(data.get("panel_password_rounds")) else {
        return false;
    };
    let Some(given) = pbkdf2_hex(password, salt, rounds) else {
        return false;
    };
    constant_time_eq(given.as_bytes(), stored.as_bytes())
}

/// 设置面板口令。语义逐条对齐 Python `wb_settings.set_panel_password`
/// （:223-237）。
///
/// 口令等于默认值时**删掉** `panel_password_salt` / `panel_password_rounds`，
/// 并把 `panel_password_hash` 写成空串、`panel_password_default` 写成 true；
/// 否则生成 16 字节随机盐（hex 文本）、按 120000 轮算摘要。
///
/// 口令长度不在这里校验（Python 也不校验，长度下限在 HTTP 层）。
pub fn set_panel_password(accounts_dir: &std::path::Path, password: &str) -> Result<(), String> {
    let mut map = read_settings_map(accounts_dir);
    if password == DEFAULT_PANEL_PASSWORD {
        map.remove("panel_password_salt");
        map.remove("panel_password_rounds");
        map.insert(
            "panel_password_hash".to_string(),
            Value::String(String::new()),
        );
        map.insert("panel_password_default".to_string(), Value::Bool(true));
    } else {
        let mut salt_bytes = [0u8; 16];
        getrandom::getrandom(&mut salt_bytes)
            .map_err(|err| format!("生成面板口令盐值失败: {err}"))?;
        let salt = encode_hex(&salt_bytes);
        let digest = pbkdf2_hex(password, &salt, PBKDF2_ROUNDS)
            .ok_or_else(|| "生成面板口令摘要失败".to_string())?;
        map.insert("panel_password_salt".to_string(), Value::String(salt));
        map.insert(
            "panel_password_rounds".to_string(),
            Value::from(PBKDF2_ROUNDS),
        );
        map.insert("panel_password_hash".to_string(), Value::String(digest));
        map.insert("panel_password_default".to_string(), Value::Bool(false));
    }
    write_settings_map(accounts_dir, &map)
}

/// 面板口令是否还是默认值（Python `wb_settings.panel_password_is_default`，
/// :196-201）。
///
/// settings.json 里没有（或为空）`panel_password_hash` ⇒ 从没改过 ⇒ 是默认值；
/// 改过之后只有 `panel_password_default == true` 才算「还是默认」。
pub fn panel_password_is_default(accounts_dir: &std::path::Path) -> bool {
    let data = read_settings_map(accounts_dir);
    let hash = data
        .get("panel_password_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if hash.is_empty() {
        return true;
    }
    data.get("panel_password_default").and_then(Value::as_bool) == Some(true)
}

// ---------------------------------------------------------------------------
// 登录限流
// ---------------------------------------------------------------------------

/// 解析可信代理条目（Python `wb_proxy.parse_trusted_proxies`，:324-339）。
///
/// 逗号分隔的 IP 或 CIDR（如 `10.0.0.1,fd00::/8`）；解析不了的条目直接丢弃
/// （一个笔误不该让网关起不来）。
///
/// 与 Python `ipaddress.ip_network(item, strict=False)` 的对应：
/// - 裸 IP ⇒ 按该地址族的最大前缀长度当成单地址网段；
/// - `地址/前缀` ⇒ **清掉主机位**（strict=False）；
/// - `地址/子网掩码` ⇒ 按掩码反推前缀长度。
pub fn parse_trusted_proxies(raw: &str) -> Vec<IpNet> {
    raw.split(',').filter_map(parse_trusted_entry).collect()
}

/// 解析单个可信代理条目。
fn parse_trusted_entry(item: &str) -> Option<IpNet> {
    let item = item.trim();
    if item.is_empty() {
        return None;
    }
    let (addr_part, prefix_part) = match item.split_once('/') {
        Some((addr, prefix)) => (addr.trim(), Some(prefix.trim())),
        None => (item, None),
    };
    let addr: IpAddr = addr_part.parse().ok()?;
    match addr {
        IpAddr::V4(v4) => {
            let prefix = match prefix_part {
                None => 32u8,
                Some(text) => match text.parse::<u8>() {
                    Ok(prefix) => prefix,
                    // 形如 10.0.0.0/255.255.0.0 的掩码写法。
                    Err(_) => {
                        let mask: std::net::Ipv4Addr = text.parse().ok()?;
                        let net = Ipv4Net::with_netmask(v4, mask).ok()?;
                        return Some(IpNet::V4(net));
                    }
                },
            };
            if prefix > 32 {
                return None;
            }
            // strict=False：清掉主机位。
            let bits = u32::from(v4);
            let mask = if prefix == 0 {
                0u32
            } else {
                u32::MAX << (32 - prefix)
            };
            Some(IpNet::V4(
                Ipv4Net::new(std::net::Ipv4Addr::from(bits & mask), prefix).ok()?,
            ))
        }
        IpAddr::V6(v6) => {
            let prefix = match prefix_part {
                None => 128u8,
                Some(text) => match text.parse::<u8>() {
                    Ok(prefix) => prefix,
                    Err(_) => {
                        let mask: std::net::Ipv6Addr = text.parse().ok()?;
                        let net = Ipv6Net::with_netmask(v6, mask).ok()?;
                        return Some(IpNet::V6(net));
                    }
                },
            };
            if prefix > 128 {
                return None;
            }
            let bits = u128::from(v6);
            let mask = if prefix == 0 {
                0u128
            } else {
                u128::MAX << (128 - prefix)
            };
            Some(IpNet::V6(
                Ipv6Net::new(std::net::Ipv6Addr::from(bits & mask), prefix).ok()?,
            ))
        }
    }
}

/// 对端是否有资格代表它的客户端说话：回环（本机反代）或显式配置的代理
/// （Python `wb_proxy.peer_is_trusted`，:343-359）。
///
/// 对端地址解析不出来 ⇒ 不可信（直接按对端地址分桶）。
pub fn peer_is_trusted(peer_ip: &str, trusted: &[ipnet::IpNet]) -> bool {
    let Ok(addr) = peer_ip.trim().parse::<IpAddr>() else {
        return false;
    };
    match addr {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return true;
            }
            trusted
                .iter()
                .any(|net| matches!(net, IpNet::V4(entry) if entry.contains(&v4)))
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            // 双栈下连 127.0.0.1 的对端会显示成 ::ffff:127.0.0.1，
            // 不把它当回环的话本机反代的请求会被当成不可信来源。
            if let Some(mapped) = v6.to_ipv4_mapped() {
                if mapped.is_loopback() {
                    return true;
                }
            }
            trusted
                .iter()
                .any(|net| matches!(net, IpNet::V6(entry) if entry.contains(&v6)))
        }
    }
}

/// 从代理头里取一个合法 IP 的规范文本；取不到返回 `None`（绝不抛异常）。
///
/// 对齐 Python `wb_proxy._proxy_header_ip`（:361-378）：接受裸 IPv4/IPv6，
/// 也接受 nginx 变体可能写出的 `host:port` / `[v6]:port`；多值（含逗号）、
/// 超长、其余畸形串一律判为不可用。
pub fn proxy_header_ip(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() || text.len() > PROXY_HEADER_IP_MAX || text.contains(',') {
        return None;
    }
    let mut candidate = text;
    if let Some(rest) = candidate.strip_prefix('[') {
        let end = rest.find(']')?;
        candidate = &rest[..end];
    } else if candidate.matches(':').count() == 1 {
        // v4:port 形式；裸 IPv6 至少两个冒号，不会被这里截断。
        let (host, port) = candidate.split_once(':')?;
        if !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = host;
        }
    }
    let addr: IpAddr = candidate.parse().ok()?;
    Some(addr.to_string())
}

/// 登录失败计数（按 IP），对齐 Python 的 `_login_attempts` + `_prune_login_attempts`
/// （wb_proxy.py:302-311）。
///
/// 窗口 60 秒、同一 IP 最多 5 次失败；登录成功后清零。
pub struct LoginLimiter {
    attempts: Mutex<HashMap<String, Vec<f64>>>,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// 记一次失败；返回 `Some(等待秒数)` 表示该 IP 已达 5 次/60s、应当限流。
    ///
    /// 返回值的口径是「**本次失败计入之后**」的状态：窗口内失败数 ≥ 5 ⇒
    /// `Some(wait)`。等待秒数按 Python 的算法算：`int(60 - (now - 最老一次))`，
    /// 最小 1 秒。
    ///
    /// 失败原因文案由调用方组装（Python 拼
    /// `"too many login attempts, please wait {N}s"`）。
    ///
    /// 若要**逐条复刻** Python 的请求顺序（先看是否超限、再验口令、失败才
    /// 计数），请在验证口令**之前**调用 [`LoginLimiter::is_blocked`]，失败
    /// 之后再调用本方法并忽略其返回值。
    pub fn record_failure(&self, client_key: &str, now: f64) -> Option<u64> {
        let mut attempts = self.attempts.lock().expect("login limiter poisoned");
        let bucket = attempts.entry(client_key.to_string()).or_default();
        prune(bucket, now);
        bucket.push(now);
        if bucket.len() >= LOGIN_MAX_FAILURES {
            Some(wait_secs(bucket, now))
        } else {
            None
        }
    }

    /// 当前是否已经被限流（**不**记失败）。
    ///
    /// 这是 Python `_handle_panel` 里限流判定的等价物：请求进来时先看窗口内
    /// 的失败数是否已达 5，达到就直接 429，**不**校验口令、**不**计数。
    pub fn is_blocked(&self, client_key: &str, now: f64) -> Option<u64> {
        let mut attempts = self.attempts.lock().expect("login limiter poisoned");
        let wait = match attempts.get_mut(client_key) {
            Some(bucket) => {
                prune(bucket, now);
                if bucket.len() >= LOGIN_MAX_FAILURES {
                    Some(wait_secs(bucket, now))
                } else {
                    None
                }
            }
            None => None,
        };
        if wait.is_none() {
            // 窗口内没有失败 ⇒ 顺手把空桶删掉（对齐 Python 的 _prune_login_attempts）。
            if let Some(bucket) = attempts.get(client_key) {
                if bucket.is_empty() {
                    attempts.remove(client_key);
                }
            }
        }
        wait
    }

    /// 登录成功后清掉该 IP 的计数（Python `_login_attempts.pop(client_ip)`）。
    pub fn clear(&self, client_key: &str) {
        let mut attempts = self.attempts.lock().expect("login limiter poisoned");
        attempts.remove(client_key);
    }

    /// 取限流键：Python `wb_proxy.login_rate_limit_key` 的等价物。
    ///
    /// 对端可信（回环或 `WB_TRUSTED_PROXIES` 里的代理）时取代理头里的真实
    /// 客户端 IP（`X-Real-IP` 优先，其次 `X-Forwarded-For` 的第一个）；其余
    /// 对端一律**忽略**这两个头、仍按对端地址分桶——公网客户端不能靠伪造
    /// 头换个桶绕过限流。
    pub fn client_key(peer_ip: &str, headers: &HeaderMap, trusted: &[ipnet::IpNet]) -> String {
        if !peer_is_trusted(peer_ip, trusted) {
            return peer_ip.to_string();
        }
        let real = headers
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok())
            .and_then(proxy_header_ip);
        if let Some(real) = real {
            return real;
        }
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let first = forwarded.split(',').next().unwrap_or("");
        if let Some(via_chain) = proxy_header_ip(first) {
            return via_chain;
        }
        peer_ip.to_string()
    }
}

/// 丢掉窗口外的失败记录（Python `_prune_login_attempts`：`now - t < 60`）。
fn prune(bucket: &mut Vec<f64>, now: f64) {
    bucket.retain(|stamp| now - *stamp < LOGIN_WINDOW_SECS);
}

/// 还要等多少秒才能重试（Python `int(60 - (now - attempts[0]))`，最小 1 秒）。
fn wait_secs(bucket: &[f64], now: f64) -> u64 {
    let oldest = bucket.first().copied().unwrap_or(now);
    let remaining = (LOGIN_WINDOW_SECS - (now - oldest)).trunc();
    if remaining < 1.0 {
        1
    } else {
        remaining as u64
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_settings(dir: &Path, doc: &Value) {
        std::fs::write(
            dir.join("settings.json"),
            serde_json::to_string_pretty(doc).unwrap(),
        )
        .unwrap();
    }

    fn read_settings_doc(dir: &Path) -> Value {
        let text = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    // --- PBKDF2 摘要与 Python 逐字节一致 --------------------------------

    #[test]
    fn pbkdf2_matches_python_vectors() {
        // 向量由 Python 的 hashlib.pbkdf2_hmac 直接算出。
        let first = pbkdf2_hex("admin", "00000000000000000000000000000000", 120_000).unwrap();
        assert_eq!(
            first,
            "c692cd42f8690d2a36548cfee7868556c0242397c9f472fb6028edf3ad3bfe91"
        );

        let second = pbkdf2_hex("secret", "0123456789abcdef0123456789abcdef", 120_000).unwrap();
        assert_eq!(
            second,
            "46c6ea966154fcb8bef82fe03827f8f624054814bd5e19b56eccd2463d60fb73"
        );
    }

    #[test]
    fn decode_hex_matches_python_fromhex() {
        assert_eq!(decode_hex("00ff").unwrap(), vec![0x00, 0xff]);
        // Python 的 bytes.fromhex 忽略 ASCII 空白。
        assert_eq!(decode_hex(" 00 ff\n").unwrap(), vec![0x00, 0xff]);
        assert!(decode_hex("0").is_none(), "奇数个 hex 位应报错");
        assert!(decode_hex("zz").is_none(), "非 hex 字符应报错");
    }

    #[test]
    fn constant_time_eq_handles_length_mismatch() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    // --- 默认口令分支 ---------------------------------------------------

    #[test]
    fn fresh_install_accepts_only_default_password() {
        let dir = temp_dir();
        assert!(verify_panel_password(dir.path(), "admin"));
        assert!(!verify_panel_password(dir.path(), "nope"));
        assert!(!verify_panel_password(dir.path(), ""));
        assert!(panel_password_is_default(dir.path()));
    }

    #[test]
    fn empty_hash_behaves_like_missing_hash() {
        let dir = temp_dir();
        write_settings(
            dir.path(),
            &serde_json::json!({"panel_password_hash": "", "panel_password_default": false}),
        );
        // Python：stored 为空 ⇒ 走默认口令分支。
        assert!(verify_panel_password(dir.path(), "admin"));
        assert!(!verify_panel_password(dir.path(), "other"));
        assert!(panel_password_is_default(dir.path()));
    }

    #[test]
    fn default_flag_short_circuits_to_default_password() {
        let dir = temp_dir();
        write_settings(
            dir.path(),
            &serde_json::json!({
                "panel_password_hash": "deadbeef",
                "panel_password_default": true,
                "panel_password_salt": "00112233445566778899aabbccddeeff",
            }),
        );
        assert!(verify_panel_password(dir.path(), "admin"));
        assert!(!verify_panel_password(dir.path(), "other"));
        assert!(panel_password_is_default(dir.path()));
    }

    #[test]
    fn missing_salt_fails_verification() {
        let dir = temp_dir();
        write_settings(
            dir.path(),
            &serde_json::json!({
                "panel_password_hash": "deadbeef",
                "panel_password_default": false,
            }),
        );
        assert!(!verify_panel_password(dir.path(), "admin"));
        assert!(!verify_panel_password(dir.path(), "other"));
        assert!(!panel_password_is_default(dir.path()));
    }

    #[test]
    fn broken_salt_or_rounds_fail_verification() {
        let dir = temp_dir();
        // 盐不是合法 hex ⇒ Python 的 bytes.fromhex 抛错 ⇒ 校验失败。
        write_settings(
            dir.path(),
            &serde_json::json!({
                "panel_password_hash": "deadbeef",
                "panel_password_default": false,
                "panel_password_salt": "zzzz",
            }),
        );
        assert!(!verify_panel_password(dir.path(), "admin"));

        // 轮数不是正整数 ⇒ Python 的 int()/hashlib 抛错 ⇒ 校验失败。
        write_settings(
            dir.path(),
            &serde_json::json!({
                "panel_password_hash": "deadbeef",
                "panel_password_default": false,
                "panel_password_salt": "00112233445566778899aabbccddeeff",
                "panel_password_rounds": "abc",
            }),
        );
        assert!(!verify_panel_password(dir.path(), "admin"));
    }

    // --- 改密后校验 -----------------------------------------------------

    #[test]
    fn set_password_then_verify() {
        let dir = temp_dir();
        set_panel_password(dir.path(), "hunter2").unwrap();

        let doc = read_settings_doc(dir.path());
        assert_eq!(
            doc.get("panel_password_default").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            doc.get("panel_password_rounds").and_then(Value::as_u64),
            Some(u64::from(PBKDF2_ROUNDS))
        );
        let salt = doc
            .get("panel_password_salt")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(salt.len(), 32, "盐应是 16 字节的 hex 文本");
        assert_eq!(salt, salt.to_lowercase());

        assert!(verify_panel_password(dir.path(), "hunter2"));
        assert!(!verify_panel_password(dir.path(), "admin"));
        assert!(!verify_panel_password(dir.path(), "Hunter2"));
        assert!(!panel_password_is_default(dir.path()));
    }

    #[test]
    fn set_password_keeps_other_settings_keys() {
        let dir = temp_dir();
        write_settings(
            dir.path(),
            &serde_json::json!({"ui_language": "zh", "api_key_set": true, "api_key": "k"}),
        );
        set_panel_password(dir.path(), "hunter2").unwrap();
        let doc = read_settings_doc(dir.path());
        assert_eq!(doc.get("ui_language").and_then(Value::as_str), Some("zh"));
        assert_eq!(doc.get("api_key_set").and_then(Value::as_bool), Some(true));
        assert_eq!(doc.get("api_key").and_then(Value::as_str), Some("k"));
    }

    #[test]
    fn set_default_password_removes_salt_and_rounds() {
        let dir = temp_dir();
        set_panel_password(dir.path(), "hunter2").unwrap();
        assert!(!verify_panel_password(dir.path(), "admin"));

        set_panel_password(dir.path(), "admin").unwrap();
        let doc = read_settings_doc(dir.path());
        assert!(
            doc.get("panel_password_salt").is_none(),
            "改回默认口令必须删掉盐"
        );
        assert!(
            doc.get("panel_password_rounds").is_none(),
            "改回默认口令必须删掉轮数"
        );
        assert_eq!(
            doc.get("panel_password_hash").and_then(Value::as_str),
            Some("")
        );
        assert_eq!(
            doc.get("panel_password_default").and_then(Value::as_bool),
            Some(true)
        );
        assert!(verify_panel_password(dir.path(), "admin"));
        assert!(!verify_panel_password(dir.path(), "hunter2"));
        assert!(panel_password_is_default(dir.path()));
    }

    #[test]
    fn stored_digest_matches_python_for_known_salt() {
        // 固定盐下，Rust 写出的摘要必须等于 Python 算出的同一个值。
        let dir = temp_dir();
        write_settings(
            dir.path(),
            &serde_json::json!({
                "panel_password_hash": "46c6ea966154fcb8bef82fe03827f8f624054814bd5e19b56eccd2463d60fb73",
                "panel_password_default": false,
                "panel_password_salt": "0123456789abcdef0123456789abcdef",
                "panel_password_rounds": 120000,
            }),
        );
        assert!(verify_panel_password(dir.path(), "secret"));
        assert!(!verify_panel_password(dir.path(), "Secret"));
    }

    // --- 登录限流 -------------------------------------------------------

    #[test]
    fn limiter_counts_failures_and_blocks_at_five() {
        let limiter = LoginLimiter::new();
        let now = 1_000.0;
        for index in 0..4 {
            assert!(
                limiter
                    .record_failure("1.2.3.4", now + f64::from(index))
                    .is_none(),
                "第 {} 次失败还不该限流",
                index + 1
            );
        }
        let wait = limiter.record_failure("1.2.3.4", now + 4.0).unwrap();
        assert_eq!(wait, 56, "最老一次失败在 4 秒前 ⇒ 还要等 56 秒");
    }

    #[test]
    fn limiter_window_expires() {
        let limiter = LoginLimiter::new();
        for _ in 0..5 {
            limiter.record_failure("1.2.3.4", 1_000.0);
        }
        assert!(limiter.is_blocked("1.2.3.4", 1_000.0).is_some());
        // 窗口过后（60 秒）重新放行。
        assert!(limiter.is_blocked("1.2.3.4", 1_061.0).is_none());
        assert!(limiter.record_failure("1.2.3.4", 1_061.0).is_none());
    }

    #[test]
    fn limiter_clear_resets_counter() {
        let limiter = LoginLimiter::new();
        for _ in 0..5 {
            limiter.record_failure("1.2.3.4", 1_000.0);
        }
        assert!(limiter.is_blocked("1.2.3.4", 1_000.0).is_some());
        limiter.clear("1.2.3.4");
        assert!(limiter.is_blocked("1.2.3.4", 1_000.0).is_none());
        assert!(limiter.record_failure("1.2.3.4", 1_000.0).is_none());
    }

    #[test]
    fn limiter_buckets_per_client_key() {
        let limiter = LoginLimiter::new();
        for _ in 0..5 {
            limiter.record_failure("1.2.3.4", 1_000.0);
        }
        assert!(limiter.is_blocked("1.2.3.4", 1_000.0).is_some());
        assert!(limiter.is_blocked("5.6.7.8", 1_000.0).is_none());
    }

    // --- client_key -----------------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn untrusted_peer_ignores_proxy_headers() {
        let trusted = parse_trusted_proxies("10.0.0.0/8");
        let map = headers(&[("x-real-ip", "8.8.8.8"), ("x-forwarded-for", "9.9.9.9")]);
        assert_eq!(
            LoginLimiter::client_key("203.0.113.7", &map, &trusted),
            "203.0.113.7"
        );
    }

    #[test]
    fn loopback_peer_uses_x_real_ip() {
        let trusted: Vec<IpNet> = Vec::new();
        let map = headers(&[
            ("x-real-ip", "8.8.8.8"),
            ("x-forwarded-for", "9.9.9.9, 10.0.0.1"),
        ]);
        assert_eq!(
            LoginLimiter::client_key("127.0.0.1", &map, &trusted),
            "8.8.8.8"
        );
    }

    #[test]
    fn loopback_peer_falls_back_to_x_forwarded_for() {
        let trusted: Vec<IpNet> = Vec::new();
        let map = headers(&[("x-forwarded-for", "9.9.9.9, 10.0.0.1")]);
        assert_eq!(
            LoginLimiter::client_key("127.0.0.1", &map, &trusted),
            "9.9.9.9"
        );
    }

    #[test]
    fn loopback_peer_without_usable_header_falls_back_to_peer() {
        let trusted: Vec<IpNet> = Vec::new();
        let map = headers(&[("x-real-ip", "not-an-ip")]);
        assert_eq!(
            LoginLimiter::client_key("127.0.0.1", &map, &trusted),
            "127.0.0.1"
        );
    }

    #[test]
    fn ipv4_mapped_loopback_is_trusted() {
        let trusted: Vec<IpNet> = Vec::new();
        let map = headers(&[("x-real-ip", "8.8.8.8")]);
        assert_eq!(
            LoginLimiter::client_key("::ffff:127.0.0.1", &map, &trusted),
            "8.8.8.8"
        );
    }

    #[test]
    fn configured_proxy_is_trusted() {
        let trusted = parse_trusted_proxies("10.0.0.0/8");
        let map = headers(&[("x-real-ip", "8.8.8.8")]);
        assert_eq!(
            LoginLimiter::client_key("10.1.2.3", &map, &trusted),
            "8.8.8.8"
        );
        // 版本不同（IPv6 对端 vs IPv4 网段）⇒ 不算可信。
        assert!(!peer_is_trusted("fd00::1", &trusted));
    }

    #[test]
    fn trusted_proxy_parsing_handles_mixed_entries() {
        let nets = parse_trusted_proxies("10.0.0.1, fd00::/8, 192.168.1.0/24, garbage, 10.0.0.5/8");
        assert_eq!(nets.len(), 4, "解析不了的条目要被丢掉");
        assert_eq!(nets[0].to_string(), "10.0.0.1/32");
        assert_eq!(nets[1].to_string(), "fd00::/8");
        assert_eq!(nets[2].to_string(), "192.168.1.0/24");
        // strict=False：10.0.0.5/8 ⇒ 10.0.0.0/8
        assert_eq!(nets[3].to_string(), "10.0.0.0/8");
    }

    #[test]
    fn proxy_header_ip_accepts_nginx_variants() {
        assert_eq!(proxy_header_ip("1.2.3.4").as_deref(), Some("1.2.3.4"));
        assert_eq!(proxy_header_ip("1.2.3.4:5678").as_deref(), Some("1.2.3.4"));
        assert_eq!(
            proxy_header_ip("[2001:db8::1]:443").as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(proxy_header_ip(" 1.2.3.4 ").as_deref(), Some("1.2.3.4"));
        assert_eq!(
            proxy_header_ip("::ffff:127.0.0.1").as_deref(),
            Some("::ffff:127.0.0.1")
        );
        // 多值 / 超长 / 畸形 ⇒ 不可用
        assert!(proxy_header_ip("1.2.3.4, 5.6.7.8").is_none());
        assert!(proxy_header_ip(&"x".repeat(65)).is_none());
        assert!(proxy_header_ip("1.2.3.4:").is_none());
        assert!(proxy_header_ip("not-an-ip").is_none());
        assert!(proxy_header_ip("").is_none());
    }
}
