//! HTTP 层：M1 只有 /health 与静态面板 /；M3 关卡 5.4 接入 10 条 chat 路由。
pub mod auth;
pub mod chat;
pub mod config;
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
