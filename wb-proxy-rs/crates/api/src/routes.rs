use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::Config;

pub fn router(cfg: Arc<Config>) -> Router {
    Router::new().route("/health", get(health)).with_state(cfg)
}

/// 复刻 Python `_get_health` 的公开字段：始终 200；账号身份字段只在授权时
/// 出现（M2 随 identity crate 补上）。
async fn health(State(cfg): State<Arc<Config>>) -> Response {
    match wb_storage::load_snapshot(&cfg.accounts_dir, cfg.api_key.as_deref(), cfg.lan) {
        Ok(snapshot) => Json(json!({
            "ok": true,
            "realm": snapshot.realm,
            "accounts": snapshot.accounts,
            "accounts_ready": snapshot.accounts_ready,
            "api_key_required": snapshot.api_key_required,
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {
                "message": err.to_string(), "type": "server_error", "code": 500
            }})),
        )
            .into_response(),
    }
}
