use std::path::PathBuf;
use std::sync::Arc;

use wb_api::{app, Config};

#[tokio::main]
async fn main() {
    // 与 Python 侧一致：优先 ACCOUNTS_DIR，兼容旧名 WB_ACCOUNTS_DIR；
    // 空串等同未设置，因此先过滤空值再取优先级。
    let accounts_dir = PathBuf::from(
        std::env::var("ACCOUNTS_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                std::env::var("WB_ACCOUNTS_DIR")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "accounts".into()),
    );
    let dashboard =
        PathBuf::from(std::env::var("WB_DASHBOARD").unwrap_or_else(|_| "../dashboard.html".into()));
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    // PORT 未设置时用默认端口；显式设置却无法解析（非整数或超出 u16）时必须快速失败，
    // 避免静默回落到 8789 而掩盖配置错误。
    let port: u16 = match std::env::var("PORT") {
        Ok(raw) => match raw.parse::<u16>() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[wb-api] invalid PORT={raw:?}: {e} (expected integer 0-65535)");
                std::process::exit(2);
            }
        },
        Err(std::env::VarError::NotPresent) => 8789,
        Err(e) => {
            eprintln!("[wb-api] invalid PORT: {e} (must be valid UTF-8)");
            std::process::exit(2);
        }
    };
    let cfg = Config {
        accounts_dir,
        dashboard,
        api_key: std::env::var("API_KEY").ok().filter(|k| !k.is_empty()),
        lan: std::env::var("WB_LAN")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false),
    };
    let router = app(Arc::new(cfg));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind failed");
    eprintln!("[wb-api] listening on http://{addr}");
    axum::serve(listener, router).await.expect("server failed");
}
