//! 网关提示词模式，移植自 wb_prompt.py（:38-128）与 wb_proxy.apply_prompt_mode
//! （:11779-11800）。
//!
//! 三种模式：
//! - passthrough（默认）：messages 原样；403 降级窗口内 → rewrite(DEGRADED_PROMPT)
//! - custom：rewrite(指定文本)——删全部 system/developer，头插 system
//! - append：未降级 → 在头部连续 system/developer 区块后插入网关 system；降级 → rewrite(DEGRADED_PROMPT)
//!
//! load_prompt（文件读取）属 M5 设置范围；本模块 PromptConfig.text 直接携带
//! 已加载文本（file 空时的内置 DEFAULT_PROMPT 同样由调用方解析）。

use serde_json::{Map, Value};

/// 403 降级窗口内的兜底提示词（wb_prompt.py:35-36）。
pub const DEGRADED_PROMPT: &str =
    "You are a helpful assistant. Respond in the user's language, follow the user's instructions, and be direct and concise.";

/// 内置默认提示词（load_prompt 的 file 为空时，wb_prompt.py:24-32）。
pub const DEFAULT_PROMPT: &str = "你是一名工程助手，擅长阅读代码库、诊断问题并提出可执行的修复建议。请结合上下文给出简洁、准确的回答。";

/// 提示词配置：mode ∈ {passthrough, custom, append}，未知回退 passthrough；
/// text 为 custom/append 使用的已加载提示词内容。
#[derive(Debug, Clone)]
pub struct PromptConfig {
    pub mode: String,
    pub text: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            mode: "passthrough".into(),
            text: String::new(),
        }
    }
}

impl PromptConfig {
    fn mode(&self) -> &str {
        match self.mode.as_str() {
            "custom" | "append" => self.mode.as_str(),
            _ => "passthrough",
        }
    }
}

fn system_message(text: &str) -> Value {
    let mut system = Map::new();
    system.insert("role".into(), Value::String("system".into()));
    system.insert("content".into(), Value::String(text.into()));
    Value::Object(system)
}

/// rewrite(messages, text)：删全部 role∈{system,developer} 讯息，头插 system。
/// 语义与 Python 相同：其余消息保留（custom 模式）。
fn rewrite(messages: &mut Vec<Value>, text: &str) {
    messages.retain(|m| {
        !matches!(
            m.get("role").and_then(|r| r.as_str()),
            Some("system") | Some("developer")
        )
    });
    messages.insert(0, system_message(text));
}

/// degrade：403 降级窗口内**整体替换**为单条 system——丢弃全部上下文
/// （wb_proxy 语义），与 custom 的 rewrite（保留非 system 消息）不同。
fn degrade(messages: &mut Vec<Value>) {
    messages.clear();
    messages.push(system_message(DEGRADED_PROMPT));
}

/// apply_prompt_mode：按模式与降级状态改写 messages（就地）。
/// `degraded` = 403 内容拦截触发的降级窗口（Task 4 注入）。
pub fn apply_prompt_mode(messages: &mut Vec<Value>, cfg: &PromptConfig, degraded: bool) {
    match cfg.mode() {
        "custom" => {
            // degraded 对 custom 无效（永不降级）；load_prompt 失败（text 空）→ 原样返回
            if !cfg.text.is_empty() {
                rewrite(messages, &cfg.text);
            }
        }
        "append" => {
            if degraded {
                degrade(messages);
            } else if !cfg.text.is_empty() {
                // 跳过开头连续的 system/developer 区块后插入
                let mut index = 0usize;
                while index < messages.len() {
                    match messages[index].get("role").and_then(|r| r.as_str()) {
                        Some("system") | Some("developer") => index += 1,
                        _ => break,
                    }
                }
                messages.insert(index, system_message(&cfg.text));
            }
        }
        _ => {
            // passthrough：仅降级窗口内整体替换
            if degraded {
                degrade(messages);
            }
        }
    }
}

/// next_midnight_cst（wb_prompt.py:96-105）：下一个「次日 00:00 UTC+8」。
/// **硬编码 +8h**（Python 原样）——与 notes::next_local_4am 的显式偏移参数
/// 不同，这是进程级降级窗口的专用口径，不随面板时区设置变化。
pub fn next_midnight_cst(now: f64) -> f64 {
    let offset = 8.0 * 3600.0;
    let day = ((now + offset) / 86400.0).floor() as i64;
    (day + 1) as f64 * 86400.0 - offset
}

/// DegradeGate（wb_prompt.py:107-128）：进程级 403 降级窗口，触发后到
/// 次日 00:00 CST 为止（不续期；重复 trigger 在窗口内不延后）。
/// M3 单线程路径，未加锁（Python 用锁防并发 trigger 竞态）。
pub struct DegradeGate {
    until: f64,
}

impl Default for DegradeGate {
    fn default() -> Self {
        Self { until: 0.0 }
    }
}

impl DegradeGate {
    pub fn active(&self, now: f64) -> bool {
        now < self.until
    }

    pub fn trigger(&mut self, now: f64) {
        if now >= self.until {
            self.until = next_midnight_cst(now);
        }
    }

    pub fn reset(&mut self) {
        self.until = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(roles: &[&str]) -> Vec<Value> {
        roles
            .iter()
            .map(|r| serde_json::json!({"role": r, "content": r.to_string()}))
            .collect()
    }

    fn system(content: &str) -> Value {
        serde_json::json!({"role": "system", "content": content})
    }

    #[test]
    fn passthrough_untouched_unless_degraded() {
        let mut m = msgs(&["user", "assistant"]);
        apply_prompt_mode(&mut m, &PromptConfig::default(), false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "user");

        let mut m2 = msgs(&["user", "assistant"]);
        apply_prompt_mode(&mut m2, &PromptConfig::default(), true);
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0], system(DEGRADED_PROMPT));
    }

    #[test]
    fn custom_rewrites_and_drops_previous_system() {
        let mut m = msgs(&["system", "developer", "user"]);
        let cfg = PromptConfig {
            mode: "custom".into(),
            text: "my prompt".into(),
        };
        apply_prompt_mode(&mut m, &cfg, true); // degraded 对 custom 无效
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], system("my prompt"));
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn custom_with_empty_text_is_untouched() {
        let mut m = msgs(&["user"]);
        apply_prompt_mode(
            &mut m,
            &PromptConfig {
                mode: "custom".into(),
                text: String::new(),
            },
            true,
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["role"], "user");
    }

    #[test]
    fn append_skips_leading_system_block() {
        let mut m = msgs(&["system", "system", "user", "assistant"]);
        let cfg = PromptConfig {
            mode: "append".into(),
            text: "gateway".into(),
        };
        apply_prompt_mode(&mut m, &cfg, false);
        assert_eq!(m.len(), 5);
        assert_eq!(m[2]["role"], "system");
        assert_eq!(m[2]["content"], "gateway");
        assert_eq!(m[3]["role"], "user");
    }

    #[test]
    fn append_degraded_behaves_like_custom_rewrite() {
        let mut m = msgs(&["system", "user"]);
        let cfg = PromptConfig {
            mode: "append".into(),
            text: "gateway".into(),
        };
        apply_prompt_mode(&mut m, &cfg, true);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0], system(DEGRADED_PROMPT));
    }

    #[test]
    fn unknown_mode_falls_back_to_passthrough() {
        let mut m = msgs(&["user"]);
        let cfg = PromptConfig {
            mode: "bogus".into(),
            text: "x".into(),
        };
        apply_prompt_mode(&mut m, &cfg, false);
        assert_eq!(m.len(), 1);
    }

    // ---- T4d：DegradeGate / next_midnight_cst（wb_prompt.py:96-128）----

    #[test]
    fn next_midnight_cst_lands_on_0000_utc8() {
        // 2026-10-16 00:00 UTC = 1_792_108_800（由 fetch_upstream_refs 参照值
        // 1791528299 = 2026-10-09 06:44:59 UTC 反推）。代数取整，不手打午刻。
        let midnight = 1_792_108_800.0; // 2026-10-16 00:00 UTC
        let noon_utc = midnight + 43_200.0; // 12:00 UTC = 20:00 +08
                                            // +08 纬度下 now 属 10-16（+8h 仍在同日），次日 00:00 +08 = 10-16 16:00 UTC
        let expect = midnight + 57_600.0;
        assert_eq!(next_midnight_cst(noon_utc), expect);
        // 恰好 00:00 +08 的时刻触发 → 下一个 00:00 +08（不是当前时刻）
        assert_eq!(next_midnight_cst(expect), expect + 86_400.0);
    }

    #[test]
    fn degrade_gate_activates_and_does_not_extend() {
        let mut g = DegradeGate::default();
        let now = 1_766_980_800.0;
        assert!(!g.active(now));
        g.trigger(now);
        assert!(g.active(now));
        // 窗口内重复 trigger 不延后
        let until = g.until;
        g.trigger(now + 3600.0);
        assert_eq!(g.until, until);
        // 窗口过后可再触发
        g.trigger(until + 1.0);
        assert!(g.until > until);
        g.reset();
        assert!(!g.active(until + 1.0));
    }
}
