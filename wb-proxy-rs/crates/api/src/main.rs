use std::path::PathBuf;
use std::sync::Arc;

use wb_api::{app, Config};

#[tokio::main]
async fn main() {
    let accounts_dir = PathBuf::from(std::env::var("WB_ACCOUNTS_DIR").unwrap_or_else(|_| "accounts".into()));
    let dashboard = PathBuf::from(std::env::var("WB_DASHBOARD").unwrap_or_else(|_| "../dashboard.html".into()));
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8789);
    let cfg = Config {
        accounts_dir,
        dashboard,
        api_key: std::env::var("API_KEY").ok().filter(|k| !k.is_empty()),
        lan: std::env::var("WB_LAN").map(|v| v == "1" || v == "true").unwrap_or(false),
    };
    let router = app(Arc::new(cfg));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind failed");
    eprintln!("[wb-api] listening on http://{addr}");
    axum::serve(listener, router).await.expect("server failed");
}
