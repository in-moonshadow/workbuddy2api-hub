//! 只读访问 Python 版本留下的 legacy 数据目录。

/// 一个账号快照的聚合信息（M1 只需要计数与 realm）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub realm: String,
    pub accounts: usize,
    pub accounts_ready: usize,
    pub api_key_required: bool,
}
