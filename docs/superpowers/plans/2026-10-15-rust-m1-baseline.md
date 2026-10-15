# M1：Rust 基线（workspace + /health + 静态面板 + 契约测试框架）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 不触碰运行中的 Python 容器，建立可编译、可测试的 Rust workspace；影子实例在 8789 提供与 Python 字段一致的 `/health` 与静态面板 `/`，并落下契约测试框架与 CI。

**Architecture:** `wb-proxy-rs/` 独立 Cargo workspace，与 Python 并存（不同进程/端口，只读同一份 legacy JSON）。M1 只落 `storage`（只读解析账号目录）与 `api`（axum）两个 crate。

**Tech Stack:** Rust stable（MSRV 1.85）、axum 0.7、tokio 1、serde_json 1；dev：tower 0.5(util)、http-body-util 0.1、tempfile 3。前端继续用现有 `dashboard.html`。

## Global Constraints

- 100% 向后兼容：endpoint 路径、JSON 字段名、错误信封 `{"error":{"message","type","code"}}`、`workbuddy-accounts` v1 格式不得改名。
- 不扰动运行中的 Python 服务：不得重建/重启/改配置 8788 容器；验证只在 8789 与测试进程内。
- 影子默认 `127.0.0.1:8789`，`HOST`/`PORT` 可由环境变量覆盖。
- 持久化只用现有 legacy JSON；M1 只读，不写任何账号/用量文件。
- 不引入运行时代理环境变量；不新增本计划未列出的生产依赖。
- 新文件只放 `wb-proxy-rs/`；不得改 `wb_*.py`、`dashboard.html`、Dockerfile、compose。
- 每个任务提交前必须 `cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo test` 全绿。
  - **例外（Task 1，用户 2026-10-15 裁定）**：Task 1 只交付骨架，验收为 `cargo metadata` 通过 + `cargo build -p wb-storage` 成功；fmt/clippy/test 从 Task 2 起强制全绿（`wb-api` 的 `config`/`routes` 在 Task 3 才存在）。
- MSRV 为 1.85（2026-10-15 用户裁定，因 hyper-util 0.1.21 要求 ≥1.85）；ACCOUNTS_DIR 优先于 WB_ACCOUNTS_DIR。

---

### Task 1: Rust 工具链 + workspace 骨架

**Files:**
- Create: `wb-proxy-rs/Cargo.toml`、`rust-toolchain.toml`、`.gitignore`、`README.md`
- Create: `wb-proxy-rs/crates/storage/{Cargo.toml,src/lib.rs}`
- Create: `wb-proxy-rs/crates/api/{Cargo.toml,src/lib.rs,src/main.rs}`

**Interfaces:**
- Consumes: 无
- Produces: workspace 根；crate `wb-storage`、`wb-api`；二进制 `wb-api`

- [ ] **Step 1: 安装 Rust 工具链（用户本机执行一次）**

本机无 `cargo`；该命令非交互式：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --component rustfmt clippy
source "$HOME/.cargo/env"
rustc --version && cargo --version
```

Expected: 两条命令打印版本号。

- [ ] **Step 2: workspace 根文件**

`wb-proxy-rs/Cargo.toml`：

```toml
[workspace]
resolver = "2"
members = ["crates/storage", "crates/api"]

[workspace.package]
edition = "2021"
rust-version = "1.85"

[workspace.dependencies]
serde_json = "1"
```

`wb-proxy-rs/rust-toolchain.toml`：

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

`wb-proxy-rs/.gitignore`：

```
/target
```

`wb-proxy-rs/README.md`：

```markdown
# wb-proxy-rs

WorkBuddy 网关的 Rust 重写（影子实例，默认 127.0.0.1:8789）。

设计与里程碑见 `../docs/superpowers/specs/2026-10-15-rust-refactor-design.md`。
运行中的 Python 服务（8788）在切换前保持不动；本目录只读同一份
`accounts/`、`usage/` legacy 数据。

    cargo fmt --check
    cargo clippy -- -D warnings
    cargo test
    PORT=8789 cargo run -p wb-api
```

- [ ] **Step 3: storage crate 骨架**

`wb-proxy-rs/crates/storage/Cargo.toml`：

```toml
[package]
name = "wb-storage"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde_json.workspace = true

[dev-dependencies]
tempfile = "3"
```

`wb-proxy-rs/crates/storage/src/lib.rs`：

```rust
//! 只读访问 Python 版本留下的 legacy 数据目录。

/// 一个账号快照的聚合信息（M1 只需要计数与 realm）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub realm: String,
    pub accounts: usize,
    pub accounts_ready: usize,
    pub api_key_required: bool,
}
```

- [ ] **Step 4: api crate 骨架**

`wb-proxy-rs/crates/api/Cargo.toml`：

```toml
[package]
name = "wb-api"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
axum = "0.7"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "fs"] }
serde_json.workspace = true
wb-storage = { path = "../storage" }

[dev-dependencies]
http-body-util = "0.1"
tower = { version = "0.5", features = ["util"] }
tempfile = "3"
```

`wb-proxy-rs/crates/api/src/lib.rs`：

```rust
//! HTTP 层：M1 只有 /health 与静态面板 /。
pub mod config;
pub mod routes;

pub use config::Config;
```

`wb-proxy-rs/crates/api/src/main.rs`：

```rust
use std::path::PathBuf;
use std::sync::Arc;

use wb_api::{app, Config};

#[tokio::main]
async fn main() {
    // 与 Python 侧一致：优先 ACCOUNTS_DIR，兼容旧名 WB_ACCOUNTS_DIR。
    let accounts_dir = PathBuf::from(
        std::env::var("ACCOUNTS_DIR")
            .or_else(|_| std::env::var("WB_ACCOUNTS_DIR"))
            .unwrap_or_else(|_| "accounts".into()),
    );
    let dashboard = PathBuf::from(std::env::var("WB_DASHBOARD").unwrap_or_else(|_| "../dashboard.html".into()));
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
        lan: std::env::var("WB_LAN").map(|v| v == "1" || v == "true").unwrap_or(false),
    };
    let router = app(Arc::new(cfg));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind failed");
    eprintln!("[wb-api] listening on http://{addr}");
    axum::serve(listener, router).await.expect("server failed");
}
```

注：`config`/`routes` 模块在 Task 3 落地；Step 5 的 `cargo metadata` 不编译依赖，可直接通过。

- [ ] **Step 5: 确认 workspace 结构**

```bash
cd wb-proxy-rs && cargo metadata --no-deps --format-version 1 > /dev/null && echo "workspace ok"
```

Expected: 打印 `workspace ok`。

- [ ] **Step 6: 提交骨架**

```bash
git add wb-proxy-rs
git commit -m "feat(rust): bootstrap wb-proxy-rs workspace (storage/api skeletons)"
```

---

### Task 2: storage 只读账号快照

**Files:**
- Modify: `wb-proxy-rs/crates/storage/src/lib.rs`（含 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `Snapshot`
- Produces:
  - `wb_storage::load_snapshot(accounts_dir: &Path, api_key_env: Option<&str>, lan: bool) -> Result<Snapshot, StorageError>`
  - `wb_storage::ui_language(accounts_dir: &Path) -> String`
  - `wb_storage::StorageError`（`Display` + `std::error::Error`）

- [ ] **Step 1: 写失败测试**

在 `crates/storage/src/lib.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_account(dir: &Path, name: &str, enabled: bool) {
        let body = serde_json::json!({
            "uid": name, "realm": "cn", "enabled": enabled,
            "accessToken": "h.p.s", "cooldownUntil": 0.0, "lastError": ""
        });
        fs::write(dir.join(format!("{name}.json")), body.to_string()).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("active_realm.json"), r#"{"realm":"cn"}"#).unwrap();
        fs::write(dir.path().join("settings.json"), r#"{"ui_language":"en"}"#).unwrap();
        write_account(dir.path(), "a", true);
        write_account(dir.path(), "b", false);
        dir
    }

    #[test]
    fn snapshot_counts_only_account_files() {
        let dir = fixture();
        let snap = load_snapshot(dir.path(), None, false).unwrap();
        assert_eq!(snap.realm, "cn");
        assert_eq!(snap.accounts, 2);
        assert_eq!(snap.accounts_ready, 1);
        assert!(!snap.api_key_required);
    }

    #[test]
    fn missing_realm_falls_back_to_intl() {
        let dir = tempfile::tempdir().unwrap();
        let snap = load_snapshot(dir.path(), None, false).unwrap();
        assert_eq!(snap.realm, "intl");
        assert_eq!(snap.accounts, 0);
    }

    #[test]
    fn lan_or_env_key_requires_a_key() {
        let dir = fixture();
        assert!(load_snapshot(dir.path(), None, true).unwrap().api_key_required);
        assert!(load_snapshot(dir.path(), Some("k"), false).unwrap().api_key_required);
    }

    #[test]
    fn language_defaults_to_zh_when_absent_or_invalid() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ui_language(dir.path()), "zh");
        fs::write(dir.path().join("settings.json"), r#"{"ui_language":"fr"}"#).unwrap();
        assert_eq!(ui_language(dir.path()), "zh");
    }

    #[test]
    fn language_reads_valid_value() {
        let dir = fixture();
        assert_eq!(ui_language(dir.path()), "en");
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd wb-proxy-rs && cargo test -p wb-storage
```

Expected: 编译失败，报 `cannot find function load_snapshot` / `ui_language`。

- [ ] **Step 3: 写最小实现**

在 `Snapshot` 之后追加：

```rust
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 与 Python 侧相同：这几个文件不是账号。
const NON_ACCOUNT_FILES: [&str; 3] = ["settings.json", "model.json", "active_realm.json"];

#[derive(Debug)]
pub enum StorageError {
    Io(PathBuf, std::io::Error),
    Json(PathBuf, serde_json::Error),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Io(path, err) => write!(f, "{}: {err}", path.display()),
            StorageError::Json(path, err) => write!(f, "{}: {err}", path.display()),
        }
    }
}

impl std::error::Error for StorageError {}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>, StorageError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(StorageError::Io(path.to_path_buf(), err)),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| StorageError::Json(path.to_path_buf(), err))
}

/// 实例级默认界面语言，取值与 Python 的 wb_settings.ui_language 一致。
pub fn ui_language(accounts_dir: &Path) -> String {
    const VALID: [&str; 3] = ["zh", "zh-Hant", "en"];
    let value = read_json(&accounts_dir.join("settings.json"))
        .ok()
        .flatten()
        .and_then(|doc| doc.get("ui_language").and_then(|v| v.as_str()).map(str::to_string));
    match value {
        Some(lang) if VALID.contains(&lang.as_str()) => lang,
        _ => "zh".to_string(),
    }
}

fn read_realm(accounts_dir: &Path) -> Result<String, StorageError> {
    let doc = read_json(&accounts_dir.join("active_realm.json"))?;
    let realm = doc.and_then(|d| d.get("realm").and_then(|v| v.as_str()).map(str::to_string));
    Ok(match realm.as_deref() {
        Some("cn") => "cn".to_string(),
        _ => "intl".to_string(),
    })
}

fn unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// M1 的 ready 判定是 Python `Account.ready()` 的保守近似：已启用 + 有 token
/// + 未在 cooldown + 无残留错误。M2 引入 identity crate 后补齐 park/breaker/
/// degrade 等状态位。
fn is_ready(account: &serde_json::Value, now: f64) -> bool {
    let enabled = account.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let token = account.get("accessToken").and_then(|v| v.as_str()).unwrap_or("");
    let cooldown = account.get("cooldownUntil").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let last_error = account.get("lastError").and_then(|v| v.as_str()).unwrap_or("");
    enabled && !token.is_empty() && cooldown <= now && last_error.is_empty()
}

fn api_key_configured(accounts_dir: &Path, api_key_env: Option<&str>, lan: bool) -> bool {
    if lan || api_key_env.map(|k| !k.is_empty()).unwrap_or(false) {
        return true;
    }
    let doc = match read_json(&accounts_dir.join("settings.json")) {
        Ok(Some(doc)) => doc,
        _ => return false,
    };
    if doc.get("auth_disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    if let Some(keys) = doc.get("api_keys").and_then(|v| v.as_array()) {
        if keys.iter().any(|k| k.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)) {
            return true;
        }
    }
    doc.get("api_key").and_then(|v| v.as_str()).map(|k| !k.is_empty()).unwrap_or(false)
}

/// 汇总 legacy 账号目录：账号数、就绪数、当前 realm、是否需要 API key。
pub fn load_snapshot(
    accounts_dir: &Path,
    api_key_env: Option<&str>,
    lan: bool,
) -> Result<Snapshot, StorageError> {
    let realm = read_realm(accounts_dir)?;
    let now = unix_secs();
    let mut accounts = 0usize;
    let mut accounts_ready = 0usize;
    let entries = fs::read_dir(accounts_dir).map_err(|err| StorageError::Io(accounts_dir.to_path_buf(), err))?;
    for entry in entries {
        let entry = entry.map_err(|err| StorageError::Io(accounts_dir.to_path_buf(), err))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if NON_ACCOUNT_FILES.contains(&name.as_str()) {
            continue;
        }
        let doc = match read_json(&path)? {
            Some(doc) if doc.is_object() => doc,
            _ => continue,
        };
        accounts += 1;
        if is_ready(&doc, now) {
            accounts_ready += 1;
        }
    }
    let api_key_required = api_key_configured(accounts_dir, api_key_env, lan);
    Ok(Snapshot { realm, accounts, accounts_ready, api_key_required })
}
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd wb-proxy-rs && cargo test -p wb-storage
```

Expected: 5 passed。

- [ ] **Step 5: fmt + clippy + 提交**

```bash
cd wb-proxy-rs && cargo fmt --check && cargo clippy -p wb-storage -- -D warnings
cd .. && git add wb-proxy-rs && git commit -m "feat(rust): read-only legacy account snapshot in storage crate"
```

---

### Task 3: api 的 /health（字段与 Python 一致）

**Files:**
- Create: `wb-proxy-rs/crates/api/src/config.rs`、`src/routes.rs`
- Modify: `wb-proxy-rs/crates/api/src/lib.rs`
- Test: `wb-proxy-rs/crates/api/tests/health.rs`

**Interfaces:**
- Consumes: `wb_storage::load_snapshot`
- Produces: `wb_api::Config { accounts_dir, dashboard, api_key, lan }`、`wb_api::app(Arc<Config>) -> axum::Router`

- [ ] **Step 1: 写失败测试**

`wb-proxy-rs/crates/api/tests/health.rs`：

```rust
use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("active_realm.json"), r#"{"realm":"cn"}"#).unwrap();
    fs::write(dir.path().join("settings.json"), r#"{"ui_language":"zh"}"#).unwrap();
    let account = |uid: &str, enabled: bool| {
        serde_json::json!({
            "uid": uid, "realm": "cn", "enabled": enabled,
            "accessToken": "h.p.s", "cooldownUntil": 0.0, "lastError": ""
        })
    };
    fs::write(dir.path().join("a.json"), account("a", true).to_string()).unwrap();
    fs::write(dir.path().join("b.json"), account("b", false).to_string()).unwrap();
    dir
}

fn config(dir: &Path) -> Config {
    Config {
        accounts_dir: dir.to_path_buf(),
        dashboard: dir.join("dashboard.html"),
        api_key: None,
        lan: false,
    }
}

#[tokio::test]
async fn health_reports_pool_shape() {
    let dir = fixture();
    let response = app(Arc::new(config(dir.path())))
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "ok": true, "realm": "cn", "accounts": 2,
            "accounts_ready": 1, "api_key_required": false
        })
    );
}
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd wb-proxy-rs && cargo test -p wb-api
```

Expected: 编译失败（`Config`/`app` 不存在）。

- [ ] **Step 3: 写最小实现**

`config.rs`：

```rust
use std::path::PathBuf;

/// 运行期配置，全部来自环境变量（见 main.rs）。
#[derive(Debug, Clone)]
pub struct Config {
    pub accounts_dir: PathBuf,
    pub dashboard: PathBuf,
    pub api_key: Option<String>,
    pub lan: bool,
}
```

`routes.rs`：

```rust
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
```

`lib.rs`：

```rust
//! HTTP 层：M1 只有 /health 与静态面板 /。
pub mod config;
pub mod routes;

use std::sync::Arc;

pub use config::Config;

/// 构建完整路由。M1 的 /health 已可用，`/` 在 Task 4 接入。
pub fn app(cfg: Arc<Config>) -> axum::Router {
    routes::router(cfg)
}
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd wb-proxy-rs && cargo test -p wb-api
```

Expected: `health_reports_pool_shape` PASS。

- [ ] **Step 5: 手动对照 Python**

```bash
cd wb-proxy-rs && WB_ACCOUNTS_DIR=../accounts PORT=8789 cargo run -p wb-api &
sleep 3
curl -s http://127.0.0.1:8789/health; echo
curl -s http://127.0.0.1:8788/health; echo
kill %1
```

Expected: 字段名相同；`accounts_ready` 允许因 M1 近似判定与 Python 不同（差异记入 Task 5 fixture 说明）。

- [ ] **Step 6: fmt + clippy + 提交**

```bash
cd wb-proxy-rs && cargo fmt --check && cargo clippy -p wb-api -- -D warnings
cd .. && git add wb-proxy-rs && git commit -m "feat(rust): expose /health shadow endpoint matching python field contract"
```

---

### Task 4: 静态面板 `/`（语言占位替换 + ETag/304）

**Files:**
- Modify: `wb-proxy-rs/crates/api/src/routes.rs`
- Test: `wb-proxy-rs/crates/api/tests/dashboard.rs`

**Interfaces:**
- Consumes: `wb_storage::ui_language`、`Config.dashboard`
- Produces: `GET /` → 200 `text/html; charset=utf-8` + `ETag` + `Cache-Control: no-cache`；`If-None-Match` 命中 → 304 空体

- [ ] **Step 1: 写失败测试**

`wb-proxy-rs/crates/api/tests/dashboard.rs`：

```rust
use std::fs;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};

fn fixture(language: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("settings.json"), format!(r#"{{"ui_language":"{language}"}}"#)).unwrap();
    fs::write(
        dir.path().join("dashboard.html"),
        r#"<html data-ui-language="__WB_UI_LANGUAGE__"><body>ok</body></html>"#,
    )
    .unwrap();
    dir
}

fn config(dir: &std::path::Path) -> Config {
    Config {
        accounts_dir: dir.to_path_buf(),
        dashboard: dir.join("dashboard.html"),
        api_key: None,
        lan: false,
    }
}

#[tokio::test]
async fn dashboard_injects_language_and_serves_etag() {
    let dir = fixture("en");
    let router = app(Arc::new(config(dir.path())));
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("content-type").unwrap(), "text/html; charset=utf-8");
    let etag = response.headers().get("etag").unwrap().to_str().unwrap().to_string();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains(r#"data-ui-language="en""#));

    let cached = router
        .oneshot(
            Request::builder()
                .uri("/")
                .header("If-None-Match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    let body = cached.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd wb-proxy-rs && cargo test -p wb-api --test dashboard
```

Expected: FAIL —— `/` 返回 404。

- [ ] **Step 3: 写最小实现**

`routes.rs` 顶部 import 增补：

```rust
use axum::http::{header, HeaderMap, HeaderValue};
use std::time::UNIX_EPOCH;
```

```rust
/// 与 Python `_dashboard` 一致的静态面板：注入语言占位、发 ETag，
/// 命中 If-None-Match 时回 304 空体。
async fn dashboard(State(cfg): State<Arc<Config>>, headers: HeaderMap) -> Response {
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
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
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
```

`router()` 增加 `.route("/", get(dashboard))`。

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd wb-proxy-rs && cargo test -p wb-api
```

Expected: health + dashboard 两个测试文件全 PASS。

- [ ] **Step 5: 手动验证真实面板**

```bash
cd wb-proxy-rs && WB_ACCOUNTS_DIR=../accounts WB_DASHBOARD=../dashboard.html PORT=8789 cargo run -p wb-api &
sleep 3
curl -s -o /dev/null -w 'status=%{http_code} type=%{content_type}\n' http://127.0.0.1:8789/
kill %1
```

Expected: `status=200 type=text/html; charset=utf-8`（8788 不受影响）。

- [ ] **Step 6: fmt + clippy + 提交**

```bash
cd wb-proxy-rs && cargo fmt --check && cargo clippy -p wb-api -- -D warnings
cd .. && git add wb-proxy-rs && git commit -m "feat(rust): serve dashboard.html with language injection and ETag/304"
```

---

### Task 5: 契约测试框架（Python 输出固化为 fixture）

**Files:**
- Create: `wb-proxy-rs/tests/fixtures/health_python.json`
- Create: `wb-proxy-rs/tests/contract_health.rs`
- Modify: `wb-proxy-rs/README.md`（追加 fixture 重新生成说明）

**Interfaces:**
- Consumes: Task 3 的 `app`/`Config`
- Produces: `assert_same_shape(actual, expected)`（定义在 `tests/contract_health.rs`）

- [ ] **Step 1: 固化 Python 真实输出**

`wb-proxy-rs/tests/fixtures/health_python.json`（2026-10-15 从 8788 实测抓取，未授权请求）：

```json
{"ok": true, "realm": "intl", "accounts": 5, "accounts_ready": 5, "api_key_required": true}
```

- [ ] **Step 2: 写契约测试**

`wb-proxy-rs/tests/contract_health.rs`：

```rust
//! 契约检查：影子实例的 /health 必须与 Python 输出同字段、同类型。
//! fixture 由真实 Python 实例抓取，重新生成方式见 wb-proxy-rs/README.md。

use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;
use wb_api::{app, Config};

fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("active_realm.json"), r#"{"realm":"intl"}"#).unwrap();
    fs::write(dir.path().join("settings.json"), r#"{"ui_language":"zh"}"#).unwrap();
    let account = serde_json::json!({
        "uid": "a", "realm": "intl", "enabled": true,
        "accessToken": "h.p.s", "cooldownUntil": 0.0, "lastError": ""
    });
    fs::write(dir.path().join("a.json"), account.to_string()).unwrap();
    dir
}

fn config(dir: &Path) -> Config {
    Config {
        accounts_dir: dir.to_path_buf(),
        dashboard: dir.join("dashboard.html"),
        api_key: None,
        lan: false,
    }
}

/// 同字段、同 JSON 类型；值本身允许不同（账号池与开关因机器而异）。
fn assert_same_shape(actual: &serde_json::Value, expected: &serde_json::Value) {
    let a = actual.as_object().expect("actual must be an object");
    let e = expected.as_object().expect("expected must be an object");
    let mut a_keys: Vec<&String> = a.keys().collect();
    let mut e_keys: Vec<&String> = e.keys().collect();
    a_keys.sort();
    e_keys.sort();
    assert_eq!(a_keys, e_keys, "field sets differ");
    for key in e_keys {
        assert_eq!(
            std::mem::discriminant(&a[key]),
            std::mem::discriminant(&e[key]),
            "type of {key} differs"
        );
    }
}

#[tokio::test]
async fn health_shape_matches_python_fixture() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/health_python.json")).unwrap();
    let dir = fixture_dir();
    let response = app(Arc::new(config(dir.path())))
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let actual: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_same_shape(&actual, &expected);
}
```

- [ ] **Step 3: 运行测试，确认通过**

```bash
cd wb-proxy-rs && cargo test --test contract_health
```

Expected: `health_shape_matches_python_fixture` PASS（Task 3 已完成）。

- [ ] **Step 4: README 记录 fixture 重新生成方式**

在 `wb-proxy-rs/README.md` 追加：

```markdown
## 契约 fixture

`tests/fixtures/health_python.json` 是从运行中的 Python 实例抓取的真实响应：

    curl -s http://127.0.0.1:8788/health > wb-proxy-rs/tests/fixtures/health_python.json

改动 Python 侧 /health 字段时，必须重新抓取并同步本 fixture，否则契约测试会在 CI 里失败。
```

- [ ] **Step 5: fmt + clippy + 提交**

```bash
cd wb-proxy-rs && cargo fmt --check && cargo clippy --tests -- -D warnings
cd .. && git add wb-proxy-rs && git commit -m "test(rust): contract harness comparing shadow /health against python fixture"
```

---

### Task 6: CI 接入

**Files:**
- Create: `.github/workflows/rust.yml`

**Interfaces:**
- Consumes: 全部 Rust 代码
- Produces: GitHub Actions job `rust`（fmt/clippy/test）

- [ ] **Step 1: 写 workflow**

```yaml
name: rust

on:
  push:
    paths:
      - "wb-proxy-rs/**"
      - ".github/workflows/rust.yml"
  pull_request:
    paths:
      - "wb-proxy-rs/**"
      - ".github/workflows/rust.yml"

jobs:
  test:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: wb-proxy-rs
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: wb-proxy-rs
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

- [ ] **Step 2: 本地复现 CI 命令**

```bash
cd wb-proxy-rs && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: 三条命令全部成功。

- [ ] **Step 3: 提交**

```bash
git add .github/workflows/rust.yml && git commit -m "ci(rust): build, lint and test wb-proxy-rs on push"
```

---

## Self-Review

**Spec coverage（对 `2026-10-15-rust-refactor-design.md`）：**
- §10 M1「workspace + 最小 /health 与静态面板 + contract 测试框架 + CI」→ Task 1–6 全覆盖。
- §4 目标结构：M1 只落 `storage`/`api`；`identity`/`gateway`/`tasks`/`shadow` 分属 M2–M4，本计划不预建空 crate（YAGNI），新增时改 `members` 一行。
- §2 约束（不扰动 Python、8789 影子、只读 legacy JSON）→ Global Constraints + Task 3/4 手动验证。
- §8.1 契约测试 → Task 5；§8.2 shadow 回放属 M3，本计划不含。

**Placeholder scan:** 无 TBD/TODO；每个代码步骤含完整代码与期望输出。

**Type consistency:** `Config` 四字段在 config.rs/main.rs/测试中一致；`load_snapshot`/`ui_language`/`Snapshot` 签名一次定义、后续复用；crate 名 `wb-storage`/`wb-api` 与依赖声明一致。
