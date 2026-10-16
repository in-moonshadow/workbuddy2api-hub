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
    // 生效 key 对齐 Python 三来源（wb_proxy.py:18722-18737）：env API_KEY >
    // 面板 api_key_set > LAN launcher_key。影子实例以 LAN 模式启动时 launcher_key
    // 是唯一 key 来源，只读 env 会漏掉它，导致 /health 误判为未授权。
    let api_key =
        wb_storage::effective_api_key(&accounts_dir, std::env::var("API_KEY").ok().as_deref());
    let cfg = Config {
        accounts_dir,
        dashboard,
        api_key,
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
