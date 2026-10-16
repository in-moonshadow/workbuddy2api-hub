use std::path::PathBuf;

/// 运行期配置，全部来自环境变量（见 main.rs）。
#[derive(Debug, Clone)]
pub struct Config {
    pub accounts_dir: PathBuf,
    pub dashboard: PathBuf,
    pub api_key: Option<String>,
    pub lan: bool,
}
