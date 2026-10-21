//! 模型额度 cap（code 6004）的**范围判定**表。
//!
//! 权威源：Python `wb_accounts.py` 的 `FREE_CAP_MODELS` / `is_free_cap_model`
//! （:275-287）。
//!
//! ## 为什么住在 wb-gateway 而不是 wb-tasks
//!
//! 范围判定的**唯一真实消费者**是 `wb_gateway::notes::note_model_cap`
//! （转发路径撞 6004 时记 `model_caps`）。而依赖方向是 `wb-tasks → wb-gateway`
//! —— 表若留在 wb-tasks，gateway 就反向依赖 tasks，环依赖。
//!
//! ## 与 `AccountState::free_models` 的关系（**两套并存，不是二选一**）
//!
//! Python 侧有**两个**互不相干的门控，Rust 早期把它们混成了一个：
//!
//! | | Python | 作用 | Rust 落点 |
//! |---|---|---|---|
//! | 静态表 | `is_free_cap_model(realm, model)` | 决定 6004 **要不要写进 `model_caps`**（跨重启持久禁用） | 本模块 |
//! | 运行时目录 | `account.free_models` | 决定「这个模型算不算免费」（每日额度守卫 `model_is_free`） | `AccountState::free_models` |
//!
//! Python 的 `Account.free_models` 由 `apply_daily_credit_limit()` 从**模型目录**
//! （`wb_catalog`）按 realm 填充，与本静态表是两套数据源。
//! Rust 侧 M5 的「每日额度守卫」尚未落地，`free_models` 目前**没有消费者**，
//! 但字段保留（对齐 Python 的 `Account` 形状，M5 接线时填）。
//!
//! ⚠️ 因此：`note_model_cap` 的门控**必须**用本静态表（按 `account.realm`
//! 判定），**不能**用 `free_models` —— 后者在 Rust 侧恒为空，会让 6004
//! 永远不落 `model_caps`（面板显示失败、模型却不被禁用）。

/// 各区域参与「限额联动」的免费模型（Python `FREE_CAP_MODELS`）。
///
/// 范围外的模型即使收到 6004 也不写 cap、不跨重启禁用。
pub const FREE_CAP_MODELS: [(&str, &[&str]); 2] = [
    ("intl", &["hy4-preview-f", "hy3", "deepseek-v4.1-flash"]),
    ("cn", &["hy4-preview-f", "hy3"]),
];

/// 该「区域 × 模型」是否在限额联动的范围内。
///
/// 未知区域或空模型一律不在：范围判断必须**失败关闭**，宁可漏记一次也不把
/// 付费模型拉进持久化禁用。
pub fn is_free_cap_model(realm: &str, model: &str) -> bool {
    if model.is_empty() {
        return false;
    }
    FREE_CAP_MODELS
        .iter()
        .find(|(r, _)| *r == realm)
        .map(|(_, models)| models.contains(&model))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_cap_models_match_the_python_table() {
        assert!(is_free_cap_model("intl", "deepseek-v4.1-flash"));
        assert!(is_free_cap_model("intl", "hy4-preview-f"));
        assert!(is_free_cap_model("intl", "hy3"));
        // cn 侧不含 deepseek-v4.1-flash
        assert!(!is_free_cap_model("cn", "deepseek-v4.1-flash"));
        assert!(is_free_cap_model("cn", "hy3"));
        // 未知区域一律不在（失败关闭）
        assert!(!is_free_cap_model("jp", "hy3"));
        assert!(!is_free_cap_model("intl", ""));
        assert!(!is_free_cap_model("", "hy3"));
    }
}
