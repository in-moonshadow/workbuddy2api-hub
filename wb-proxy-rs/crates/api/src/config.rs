use std::path::PathBuf;

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
