//! M5 数据侧：模型额度 cap（code 6004）的**判定**层。
//!
//! 权威源：Python `wb_accounts.py`
//! - `FREE_CAP_MODELS` / `is_free_cap_model`（:275-287）
//! - `_note_daily_chat_cap`（:975-1010）
//! - `wb_proxy.rate_limit_is_cap` / `parse_rate_limit_reset`（:12283 / :12345）
//!
//! 「记录」那一半已经在 `wb_gateway::notes::note_model_cap`（端口 2 的既有实现），
//! 本模块只负责**判定**：这份上游原文是不是一次「模型额度用满」、恢复时刻是
//! 什么时候、该不该持久化。
//!
//! 为什么需要这一层：每日活跃走自己的通道（调度器 / 一键领取 / 网页腿），
//! 不经过网关的转发路径，于是 6004 在那里只被当成一条失败日志，从不进
//! `model_caps` —— 当天后续每一次巡检都会重试同一个必然失败的窗口。

use serde_json::{json, Value};

/// 每日活跃用的模型（与 `crate::daily_chat::DAILY_CHAT_MODEL` 同一个常量值）。
///
/// 这里**重新声明**而不是引用：`wb-tasks::daily_chat` 与本模块的依赖方向
/// 应当是单向的（daily_chat 不依赖 cap），所以不复用那边的符号。
pub const DAILY_CHAT_MODEL: &str = "deepseek-v4.1-flash";

/// 上游「这个模型的窗口额度用满」的应答码。
///
/// 6004 是唯一带重置墙钟的额度判定，也是用户口径里「达到限额」的唯一标志：
/// 别的 429（14003、裸频率限制文本、账号级软限）即使碰巧带了个时钟，
/// 也不是额度用满的证据。
pub const CAP_LIMIT_CODE: i64 = 6004;

/// 各区域参与「限额联动」的免费模型（Python `FREE_CAP_MODELS`）。
///
/// **权威定义在 `wb_gateway::cap`** —— 那里才是 `note_model_cap` 的消费者
/// （依赖方向 `wb-tasks → wb-gateway`，表不能留在 wb-tasks）。此处 re-export
/// 只是让 wb-tasks 侧调用方不必多写一层路径，**不构成第二份定义**。
pub use wb_gateway::cap::FREE_CAP_MODELS;

/// 该「区域 × 模型」是否在限额联动的范围内。
///
/// 权威实现见 [`wb_gateway::cap::is_free_cap_model`]（Python
/// `wb_accounts.py:281-287`）。未知区域或空模型一律不在：范围判断必须
/// 失败关闭，宁可漏记一次也不把付费模型拉进持久化禁用。
pub use wb_gateway::cap::is_free_cap_model;

/// 一次 cap 判定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapNotice {
    /// 被判定的模型。
    pub model: &'static str,
    /// 上游给的恢复时刻（epoch 秒）。
    pub reset_at: f64,
}

impl CapNotice {
    /// 面板/日志用的可读说明。
    pub fn describe(&self, now: f64) -> String {
        let wait = (self.reset_at - now).max(0.0);
        format!(
            "模型 {} 的额度用满，{} 后（epoch {}）恢复",
            self.model, wait as i64, self.reset_at as i64
        )
    }
}

/// 判断一份上游原文是不是一次「模型额度用满」，并取出恢复时刻。
///
/// `detail` 必须是**未截断**的上游原文：`reset at YYYY-MM-DD HH:MM:SS` 落在
/// 前 120 字符之后时，用被截断过的那份解析会取不到重置时刻，于是静默不记 ——
/// 这正是「面板有失败、modelCaps 却一直空」的成因之一。
///
/// 判定链与 Python `_note_daily_chat_cap` 一致：
/// 1. `rate_limit_is_cap(detail)` —— 必须是 code 6004；
/// 2. `parse_rate_limit_reset(detail)` —— 必须能取出恢复时刻；
/// 3. 模型必须在限额联动范围内（`is_free_cap_model`）。
///
/// 任一步不成立就返回 `None`（不记 cap）。
pub fn daily_chat_cap_notice(realm: &str, detail: &str) -> Option<CapNotice> {
    if !wb_gateway::upstream::rate_limit_is_cap(detail) {
        return None;
    }
    let reset_at = wb_gateway::upstream::parse_rate_limit_reset(detail)?;
    if !is_free_cap_model(realm, DAILY_CHAT_MODEL) {
        return None;
    }
    Some(CapNotice {
        model: DAILY_CHAT_MODEL,
        reset_at,
    })
}

/// 是否需要**重写**凭证档（Python `note_model_cap` 的返回值语义）。
///
/// 重复撞同一个恢复窗口**不算新记**：不重写凭证档，但冷却要重新挂上——
/// 中间可能被换身分之类的清理顺手清掉过；挂不回来，这个窗口就一直不会被禁用。
///
/// 判定需要一个「当前已记的 reset」作为输入（`None` = 还没记过）。
pub fn is_fresh_window(current_reset: Option<f64>, reset_at: f64) -> bool {
    match current_reset {
        Some(existing) => existing != reset_at,
        None => true,
    }
}

/// 面板展示用的 cap 摘要。
pub fn cap_summary(notice: Option<&CapNotice>, now: f64) -> Value {
    match notice {
        Some(n) => json!({
            "capped": true,
            "model": n.model,
            "reset_at": n.reset_at,
            "reset_iso": crate::credits::local_text(n.reset_at),
            "remaining": (n.reset_at - now).max(0.0),
        }),
        None => json!({"capped": false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_800_000_000.0;

    #[test]
    fn free_cap_models_match_the_python_table() {
        assert!(is_free_cap_model("intl", "deepseek-v4.1-flash"));
        assert!(is_free_cap_model("intl", "hy4-preview-f"));
        assert!(is_free_cap_model("intl", "hy3"));
        // cn 侧不含 deepseek-v4.1-flash
        assert!(!is_free_cap_model("cn", "deepseek-v4.1-flash"));
        assert!(is_free_cap_model("cn", "hy3"));
        // 未知区域一律不在
        assert!(!is_free_cap_model("jp", "hy3"));
        assert!(!is_free_cap_model("intl", ""));
    }

    #[test]
    fn cap_notice_reads_the_reset_wall_clock() {
        let detail = r#"{"code":6004,"msg":"usage limit reached, your usage will reset at 2027-01-01 00:00:00 UTC+8"}"#;
        let notice = daily_chat_cap_notice("intl", detail).expect("6004 must be a cap");
        assert_eq!(notice.model, DAILY_CHAT_MODEL);
        // 2027-01-01 00:00:00 +08:00 == 2026-12-31T16:00:00Z == 1798732800
        assert_eq!(notice.reset_at, 1_798_732_800.0);
    }

    #[test]
    fn cap_notice_reads_the_chinese_reset_wording() {
        let detail = r#"{"code":6004,"msg":"将在 2027-01-01 00:00:00 UTC+8 重置"}"#;
        let notice = daily_chat_cap_notice("intl", detail).expect("cn wording must parse");
        assert_eq!(notice.reset_at, 1_798_732_800.0);
    }

    #[test]
    fn cap_notice_ignores_other_rate_limits() {
        // 14003「too many requests」不是额度用满
        let detail = r#"{"code":14003,"msg":"too many requests"}"#;
        assert!(daily_chat_cap_notice("intl", detail).is_none());
        // 裸频率限制文本
        assert!(daily_chat_cap_notice("intl", "usage exceeds frequency limit").is_none());
    }

    #[test]
    fn cap_notice_ignores_models_outside_the_linkage_scope() {
        let detail = r#"{"code":6004,"msg":"your usage will reset at 2027-01-01 00:00:00 UTC+8"}"#;
        assert!(daily_chat_cap_notice("cn", detail).is_none());
    }

    #[test]
    fn fresh_window_distinguishes_repeats() {
        assert!(is_fresh_window(None, 100.0));
        assert!(is_fresh_window(Some(99.0), 100.0));
        assert!(!is_fresh_window(Some(100.0), 100.0));
    }

    #[test]
    fn cap_summary_reports_capped_and_uncapped() {
        let notice = CapNotice {
            model: DAILY_CHAT_MODEL,
            reset_at: NOW + 600.0,
        };
        let s = cap_summary(Some(&notice), NOW);
        assert_eq!(s["capped"], true);
        assert_eq!(s["remaining"], 600.0);
        assert!(s["reset_iso"].as_str().unwrap().len() >= 10);

        let s = cap_summary(None, NOW);
        assert_eq!(s["capped"], false);
    }
}
