use std::collections::HashMap;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::auth::{key_ok, PanelSessions};
use crate::Config;

/// 面板会话 TTL：7 天（对齐 wb_settings.SESSION_TTL）。
const SESSION_TTL: u64 = 7 * 24 * 3600;

/// 聚合路由状态：配置 + 进程内面板会话表（Python 侧 PANEL 是模块级单例，
/// 这里用 Arc 让 /health 共享同一张表）。
#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    panels: Arc<PanelSessions>,
}

pub fn router(cfg: Arc<Config>) -> Router {
    let state = AppState {
        cfg,
        panels: Arc::new(PanelSessions::new(SESSION_TTL)),
    };
    Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard))
        .with_state(state)
}

/// 复刻 Python `_get_health`（:15794-15815）：始终 200；账号身份字段只在
/// 授权时出现（panel 会话 / API key / 无需鉴权三条路径，见 auth::key_ok）。
async fn health(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let cfg = &state.cfg;
    match wb_storage::load_snapshot(&cfg.accounts_dir, cfg.api_key.as_deref(), cfg.lan) {
        Ok(snapshot) => {
            let mut body = json!({
                "ok": true,
                "realm": snapshot.realm,
                "accounts": snapshot.accounts,
                "accounts_ready": snapshot.accounts_ready,
                "api_key_required": snapshot.api_key_required,
            });
            if key_ok(&headers, &params, cfg, &state.panels) {
                apply_identity(&mut body, &snapshot.realm, cfg);
            }
            Json(body).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {
                "message": err.to_string(), "type": "server_error", "code": 500
            }})),
        )
            .into_response(),
    }
}

/// 授权后补身份字段（对齐 _get_health 的 info.update）。代表账号从 realm
/// 过滤后的池中取；池空时五个字段全部为 null（Python：rep 为 None）。
fn apply_identity(body: &mut Value, realm: &str, cfg: &Config) {
    const IDENTITY_FIELDS: [&str; 5] = ["uid", "domain", "issuer", "credential_file", "expires_at"];
    let Some(map) = body.as_object_mut() else {
        return;
    };
    match wb_storage::representative(&cfg.accounts_dir, realm) {
        Some(rep) => {
            map.insert("uid".into(), json!(rep.uid));
            map.insert("domain".into(), json!(rep.domain));
            map.insert(
                "issuer".into(),
                json!(wb_identity::jwt::jwt_issuer(&rep.access_token)),
            );
            map.insert("credential_file".into(), json!(rep.credential_file));
            map.insert("expires_at".into(), json!(rep.expires_at));
        }
        None => {
            for key in IDENTITY_FIELDS {
                map.insert(key.into(), Value::Null);
            }
        }
    }
}

/// 与 Python `_dashboard` 一致的静态面板：注入语言占位、发 ETag，
/// 命中 If-None-Match 时回 304 空体。
async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let cfg = &state.cfg;
    let body = match tokio::fs::read(&cfg.dashboard).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {
                    "message": format!("dashboard.html unavailable: {err}"),
                    "type": "server_error", "code": 500
                }})),
            )
                .into_response()
        }
    };
    let language = wb_storage::ui_language(&cfg.accounts_dir);
    let html = String::from_utf8_lossy(&body).replace(
        "data-ui-language=\"__WB_UI_LANGUAGE__\"",
        &format!("data-ui-language=\"{language}\""),
    );

    let etag = match tokio::fs::metadata(&cfg.dashboard).await {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| format!("\"{:x}-{:x}-{}\"", since.as_nanos(), meta.len(), language)),
        Err(_) => None,
    };

    if let (Some(tag), Some(supplied)) = (etag.as_deref(), headers.get(header::IF_NONE_MATCH)) {
        if supplied.to_str().ok() == Some(tag) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }

    let mut response = (StatusCode::OK, html).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Some(tag) = etag {
        if let Ok(value) = HeaderValue::from_str(&tag) {
            response.headers_mut().insert(header::ETAG, value);
        }
    }
    response
}
