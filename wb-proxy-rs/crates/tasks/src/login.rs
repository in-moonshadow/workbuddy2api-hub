//! 关卡 4.24（线 B）：OAuth 登录注册表。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// 对齐 wb_accounts.py:227 `LOGIN_TTL_SECONDS = 600` 与 wb_gateway::pool::LOGIN_TTL_SECONDS。
pub const LOGIN_TTL_SECONDS: f64 = 600.0;
pub const LOGIN_PENDING: i64 = 11217;

pub const AUTH_STATE_PATH: &str = "/v2/plugin/auth/state";
pub const AUTH_TOKEN_PATH: &str = "/v2/plugin/auth/token";
pub const LOGIN_ACCOUNT_PATH: &str = "/v2/plugin/login/account";

#[derive(Debug, Clone)]
pub struct LoginState {
    pub created: f64,
    pub platform: String,
    pub realm: String,
}

static REGISTRY: Mutex<Option<HashMap<String, LoginState>>> = Mutex::new(None);

fn ensure() -> std::sync::MutexGuard<'static, Option<HashMap<String, LoginState>>> {
    let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    g
}

pub fn login_start_register(state: &str, platform: &str, realm: &str, created: f64) {
    let mut g = ensure();
    if let Some(map) = g.as_mut() {
        map.insert(
            state.to_string(),
            LoginState {
                created,
                platform: platform.to_string(),
                realm: realm.to_string(),
            },
        );
    }
}

/// 不做 TTL 判定的读取（poll 需要区分 unknown / expired 两种失败）。
pub fn login_peek(state: &str) -> Option<LoginState> {
    let g = ensure();
    g.as_ref()?.get(state).cloned()
}

/// 读取登录 state；TTL 过期时自动移除并返回 None。
pub fn login_next(state: &str, now: f64) -> Option<LoginState> {
    let mut g = ensure();
    let map = g.as_mut()?;
    let info = map.get(state).cloned()?;
    if now - info.created > LOGIN_TTL_SECONDS {
        map.remove(state);
        return None;
    }
    Some(info)
}

pub fn login_cancel(state: &str) -> bool {
    let mut g = ensure();
    if let Some(map) = g.as_mut() {
        return map.remove(state).is_some();
    }
    false
}

pub fn login_remove(state: &str) {
    let mut g = ensure();
    if let Some(map) = g.as_mut() {
        map.remove(state);
    }
}

pub fn login_count() -> usize {
    let g = ensure();
    g.as_ref().map(|m| m.len()).unwrap_or(0)
}

#[allow(dead_code)]
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
