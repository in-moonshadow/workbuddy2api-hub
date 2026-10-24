use std::path::PathBuf;
use std::time::Duration;

use ipnet::IpNet;

use crate::panel_auth::parse_trusted_proxies;

/// 运行期配置，全部来自环境变量（见 main.rs）。
#[derive(Debug, Clone)]
pub struct Config {
    pub accounts_dir: PathBuf,
    pub dashboard: PathBuf,
    pub api_key: Option<String>,
    pub lan: bool,
    /// 上游 base URL 覆盖（env `WB_UPSTREAM_BASE_URL`）。
    ///
    /// None → 按 realm+product 走 `wb_gateway::endpoints::chat_base_url`
    /// 的端点表（生产路径）；Some → 所有 chat 路由直连该地址。契约测试用
    /// wiremock 地址注入，**绝不触真实上游**。语义对应
    /// `wb_gateway::retry::RetryOptions::base_url`。
    pub upstream_base_url: Option<String>,
    /// 当前 realm（Python `CURRENT_REALM`：进程启动时读一次，之后不变）。
    ///
    /// 复刻 Python 的行为：POST /realm 只把选择落盘到 active_realm.json，
    /// 进程内这个值**不会**改变，要重启才生效。
    pub realm: String,
}

/// 面板相关的可调项。
///
/// ⚠️ 刻意**不**放进 [`Config`]：`Config` 被 `main.rs` 与十来个契约测试用
/// 结构体字面量构造，加字段会让那些文件全部编译不过（而它们不在本关卡的
/// 可改文件清单里）。这里单独成一个结构体，由 `routes::app_with_config` 注入。
#[derive(Debug, Clone)]
pub struct PanelConfig {
    /// 登录失败后的退避时长（Python `wb_proxy.py:17057` 的 `time.sleep(0.5)`）。
    ///
    /// Python 是**同步阻塞** 0.5 秒再回 401（对抗暴力破解）；Rust 侧改成
    /// `tokio::time::sleep`（不占 worker 线程），时长本身保持一致。
    ///
    /// 契约测试把它设成 0（否则每个失败用例都要白等半秒）。
    pub login_backoff: Duration,
    /// 可信代理网段（env `WB_TRUSTED_PROXIES`，Python `parse_trusted_proxies`
    /// `:324-339`）。对端可信时才采信 `X-Real-IP` / `X-Forwarded-For`。
    pub trusted_proxies: Vec<IpNet>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            login_backoff: Duration::from_millis(500),
            trusted_proxies: Vec::new(),
        }
    }
}

impl PanelConfig {
    /// 生产口径：退避 0.5 秒 + 解析 env `WB_TRUSTED_PROXIES`。
    pub fn from_env() -> Self {
        let raw = std::env::var("WB_TRUSTED_PROXIES").unwrap_or_default();
        Self {
            trusted_proxies: parse_trusted_proxies(&raw),
            ..Self::default()
        }
    }
}
