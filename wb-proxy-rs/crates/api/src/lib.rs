//! HTTP 层：M1 只有 /health 与静态面板 /；M3 关卡 5.4 接入 10 条 chat 路由。
pub mod auth;
pub mod chat;
pub mod config;
// 关卡 4.22：面板口令认证（PBKDF2）+ 登录限流。权威源是 Python
// `wb_settings.verify_panel_password` / `set_panel_password` 与
// `wb_proxy.login_rate_limit_key`。
pub mod panel_auth;
pub mod routes;

use std::sync::Arc;

pub use config::Config;

/// 构建完整路由。M1 的 /health 已可用，`/` 在 Task 4 接入。
pub fn app(cfg: Arc<Config>) -> axum::Router {
    routes::router(cfg)
}

pub fn app_with(
    cfg: Arc<Config>,
    panels: Arc<crate::auth::PanelSessions>,
    task: Arc<wb_tasks::api_routes::TaskRuntime>,
) -> axum::Router {
    routes::app_with(cfg, panels, task)
}
