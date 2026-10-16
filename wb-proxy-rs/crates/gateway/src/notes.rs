//! 账号错误回调（note_* 系列），移植自 wb_accounts.py（:2017-2123）与
//! wb_proxy.py 触发点（:12516-12747）。每个回调更新调度状态位，均为
//! `max(旧值, 新值)` 单调推进（唯一例外：note_success 全清零）。
//!
//! 调用方（重试循环）职责：
//! - 一次请求对同一账号**最多记一笔失败**（charged 集合，wb_proxy 12466-12481）；
//! - 每个失败分支同时 `affinity.demote(session_key, uid)`（demote 在 pool.rs）。

use crate::pool::{AccountState, ModelCap};

// ---- 退避曲线（wb_accounts.py:414-456）----

/// 账号级 429 软限：600s × 2^(streak-1)，顶 7200s。
pub fn soft_backoff(streak: i64) -> f64 {
    let raw = 600.0 * 2.0_f64.powi((streak - 1).max(0) as i32);
    raw.min(7200.0)
}

/// 无作用域 429 软限（未证实账号级）：60s × 2^(n-1)，顶 600s。
pub fn unverified_soft_backoff(n: i64) -> f64 {
    let raw = 60.0 * 2.0_f64.powi((n - 1).max(0) as i32);
    raw.min(600.0)
}

/// 5xx 熔断：1800s × 2^(fails-3)，顶 21600s（从第 3 次起）。
pub fn breaker_backoff(fails: i64) -> f64 {
    let raw = 1800.0 * 2.0_f64.powi((fails - 3).max(0) as i32);
    raw.min(21_600.0)
}

/// 瞬时异常降权：600s × 2^(count-5)，顶 7200s（从第 5 次起）。
pub fn degrade_backoff(count: i64) -> f64 {
    let raw = 600.0 * 2.0_f64.powi((count - 5).max(0) as i32);
    raw.min(7200.0)
}

/// 下一个本地 04:00（wb_accounts.py:393-406）：Python 用 `time.localtime` +
/// `time.mktime`（**本地时区墙壁 04:00，非 UTC**）；`stamp <= now` 则 +86400。
///
/// M3 用显式 UTC 偏移近似（DST 边界差 1h，M6 shadow 对照修正）；生产调用处
/// 传当前本地时区偏移（chrono::Local 属 M5 接线，本函数保持纯输入）。
pub fn next_local_4am(now_epoch: f64, utc_offset_seconds: i64) -> f64 {
    let offset = utc_offset_seconds as f64;
    let local_now = now_epoch + offset; // 本地钟面时刻（epoch 表示）
    let local_day = (local_now / 86_400.0).floor();
    let local_4am = local_day * 86_400.0 + 4.0 * 3600.0;
    let mut stamp = local_4am - offset; // 转回 UTC epoch
    if stamp <= now_epoch {
        stamp += 86_400.0;
    }
    stamp
}

/// 402 余额停用（wb_accounts.py:2035-2039）：直到下一个本地 04:00，
/// 不动 cooldown_until。
pub fn note_balance_cooled(account: &mut AccountState, now_epoch: f64, utc_offset_seconds: i64) {
    let until = next_local_4am(now_epoch, utc_offset_seconds);
    account.balance_until = account.balance_until.max(until);
}

/// note_success（wb_accounts.py:2110-2123）：清 model_cooldowns[model] 与全部
/// 连击/熔断/降权/软限流状态、错误标签。**唯一全清零回调**。
pub fn note_success(account: &mut AccountState, model: Option<&str>) {
    if let Some(m) = model {
        account.model_cooldowns.remove(m);
    }
    account.soft_streak = 0;
    account.unscoped_streak = 0;
    account.fails = 0;
    account.degrade_count = 0;
    account.breaker_until = 0.0;
    account.degrade_until = 0.0;
    account.cooldown_until = 0.0;
    account.last_error = String::new();
    account.last_error_detail = String::new();
}

/// note_soft_rate（wb_accounts.py:438-440, 414-415）：账号级 429。
/// soft_streak+=1；cooldown_until = max(旧, now + soft_backoff(streak))。
pub fn note_soft_rate(account: &mut AccountState, now: f64) {
    account.soft_streak += 1;
    let until = now + soft_backoff(account.soft_streak);
    account.cooldown_until = account.cooldown_until.max(until);
}

/// note_unscoped_rate（wb_accounts.py:443-450）：无作用域 429。
/// unscoped_streak+=1；返回退避秒数（由调用方经 note_error 落到
/// model_cooldowns）。**不直接改任何字段**。
pub fn note_unscoped_rate(account: &mut AccountState) -> f64 {
    account.unscoped_streak += 1;
    unverified_soft_backoff(account.unscoped_streak)
}

/// note_error（wb_accounts.py:2017-2021）：通用冷却入口（429 模型级 / 403 风控 /
/// 401 / 未知异常）。model 非空 → 记 model_cooldowns[model]：
/// wait = max(1.0, until-now) 否则 (3.0 if single_account else cooldown)；
/// until 由上游 reset_at 给出（无则 None）。同步写 last_error*。
///
/// 参数即 Python `note_error(self, msg, model, until, cooldown, single_account, now)`
/// 原样签名（8 个是契约的一部分，拆结构体有害于逐参对照）。
#[allow(clippy::too_many_arguments)]
pub fn note_error(
    account: &mut AccountState,
    msg: &str,
    model: Option<&str>,
    until: Option<f64>,
    cooldown: f64,
    single_account: bool,
    detail: &str,
    now: f64,
) {
    account.last_error = msg.into();
    account.last_error_detail = detail.into();
    if let Some(m) = model {
        let wait = match until {
            Some(u) => (u - now).max(1.0),
            None => {
                if single_account {
                    3.0
                } else {
                    cooldown
                }
            }
        };
        let next = account.model_cooldowns.get(m).copied().unwrap_or(0.0);
        account
            .model_cooldowns
            .insert(m.into(), next.max(now + wait));
    }
}

/// note_model_cap（wb_accounts.py:927-959）：code 6004「额度用满」日额度。
/// 仅 free_models 内的模型记 model_caps（重复撞同 reset 不算新记）；
/// model_cooldowns[model] = max(旧, reset_at)。free_models 为空（M5 填充）
/// 时只挂冷却不记 caps。
pub fn note_model_cap(account: &mut AccountState, model: &str, reset_at: Option<f64>, now: f64) {
    if let Some(reset) = reset_at {
        if account.free_models.contains(model) {
            let fresh = account
                .model_caps
                .get(model)
                .map(|cap| cap.reset != reset)
                .unwrap_or(true);
            if fresh {
                account
                    .model_caps
                    .insert(model.into(), ModelCap { at: now, reset });
            }
        }
        let next = account.model_cooldowns.get(model).copied().unwrap_or(0.0);
        account
            .model_cooldowns
            .insert(model.into(), next.max(reset));
    }
}

/// note_failure（wb_accounts.py:2085-2093, 453-456）：硬失败（5xx 计账号）。
/// fails+=1；fails>=3 → breaker_until = max(旧, now + breaker_backoff(fails))。
pub fn note_failure(account: &mut AccountState, msg: &str, now: f64) {
    account.last_error = msg.into();
    account.fails += 1;
    if account.fails >= 3 {
        let until = now + breaker_backoff(account.fails);
        account.breaker_until = account.breaker_until.max(until);
    }
}

/// note_unknown_failure（wb_accounts.py:2095-2108）：传输瞬时异常。
/// degrade_count+=1、fails+=1；degrade_count>=5 → degrade_until =
/// max(旧, now + degrade_backoff(count))；fails>=3 同时推熔断。
pub fn note_unknown_failure(account: &mut AccountState, msg: &str, now: f64) {
    account.last_error = msg.into();
    account.degrade_count += 1;
    account.fails += 1;
    if account.degrade_count >= 5 {
        let until = now + degrade_backoff(account.degrade_count);
        account.degrade_until = account.degrade_until.max(until);
    }
    if account.fails >= 3 {
        let until = now + breaker_backoff(account.fails);
        account.breaker_until = account.breaker_until.max(until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::AccountState;

    const NOW: f64 = 1_000.0;

    fn acc(uid: &str) -> AccountState {
        AccountState::new(uid, "intl")
    }

    #[test]
    fn soft_backoff_staircase_capped() {
        assert_eq!(soft_backoff(1), 600.0);
        assert_eq!(soft_backoff(2), 1200.0);
        assert_eq!(soft_backoff(3), 2400.0);
        assert_eq!(soft_backoff(5), 7200.0); // 600*16=9600 → 顶
        assert_eq!(soft_backoff(9), 7200.0);
    }

    #[test]
    fn unverified_backoff_staircase_capped() {
        assert_eq!(unverified_soft_backoff(1), 60.0);
        assert_eq!(unverified_soft_backoff(4), 480.0);
        assert_eq!(unverified_soft_backoff(5), 600.0); // 960 → 顶
    }

    #[test]
    fn breaker_backoff_from_third_fail() {
        assert_eq!(breaker_backoff(1), 1800.0); // fails<3 仍给 1800（Python 语义）
        assert_eq!(breaker_backoff(3), 1800.0);
        assert_eq!(breaker_backoff(4), 3600.0);
        assert_eq!(breaker_backoff(8), 21_600.0); // 1800*32=57600 → 顶
    }

    #[test]
    fn degrade_backoff_from_fifth() {
        assert_eq!(degrade_backoff(1), 600.0);
        assert_eq!(degrade_backoff(5), 600.0);
        assert_eq!(degrade_backoff(6), 1200.0);
        assert_eq!(degrade_backoff(9), 7200.0); // 顶
    }

    #[test]
    fn next_local_4am_rolls_forward() {
        // 基线：2026-01-01T00:00:00Z（已用今天逐日累加验证）
        let day0 = 1_767_225_600.0;
        let hour = 3600.0;
        // UTC+0：now=01:00Z → 当天 04:00Z
        let now = day0 + hour;
        let four_am = day0 + 4.0 * hour;
        assert_eq!(next_local_4am(now, 0), four_am);
        // 已过 04:00（07:30Z）→ 次日 04:00Z
        let after = day0 + 7.5 * hour;
        let next = day0 + 24.0 * hour + 4.0 * hour;
        assert_eq!(next_local_4am(after, 0), next);
        // UTC+8：now=2026-01-01T01:00Z = 09:00 北京，今天 04:00 已过
        // → 2026-01-02T04:00+08 = 2026-01-01T20:00Z = day0 + 20h
        let now_bj = day0 + hour;
        let bj_4am = day0 + 20.0 * hour;
        assert_eq!(next_local_4am(now_bj, 8 * 3600), bj_4am);
    }

    #[test]
    fn note_balance_cooled_uses_local_4am_and_is_monotonic() {
        let mut a = acc("u1");
        let day0 = 1_767_225_600.0;
        let now = day0 + 3600.0; // 01:00Z
        note_balance_cooled(&mut a, now, 0);
        assert_eq!(a.balance_until, day0 + 4.0 * 3600.0);
        // 后续再次调用：max 保持（同 reset 窗口）
        note_balance_cooled(&mut a, now, 0);
        assert_eq!(a.balance_until, day0 + 4.0 * 3600.0);
        // 不影响软限流
        assert_eq!(a.cooldown_until, 0.0);
    }

    #[test]
    fn note_success_clears_everything() {
        let mut a = acc("u1");
        a.model_cooldowns.insert("m".into(), NOW + 100.0);
        a.cooldown_until = NOW + 100.0;
        a.breaker_until = NOW + 100.0;
        a.degrade_until = NOW + 100.0;
        a.soft_streak = 3;
        a.unscoped_streak = 2;
        a.fails = 4;
        a.degrade_count = 6;
        a.last_error = "x".into();
        note_success(&mut a, Some("m"));
        assert!(a.model_cooldowns.is_empty());
        assert_eq!(a.cooldown_until, 0.0);
        assert_eq!(a.breaker_until, 0.0);
        assert_eq!(a.degrade_until, 0.0);
        assert_eq!(a.soft_streak, 0);
        assert_eq!(a.unscoped_streak, 0);
        assert_eq!(a.fails, 0);
        assert_eq!(a.degrade_count, 0);
        assert_eq!(a.last_error, "");
        // model=None 不清别的模型冷却
        let mut b = acc("u1");
        b.model_cooldowns.insert("other".into(), NOW + 100.0);
        note_success(&mut b, Some("m"));
        assert!(b.model_cooldowns.contains_key("other"));
    }

    #[test]
    fn note_soft_rate_streak_and_cooldown() {
        let mut a = acc("u1");
        note_soft_rate(&mut a, NOW);
        assert_eq!(a.soft_streak, 1);
        assert_eq!(a.cooldown_until, NOW + 600.0);
        note_soft_rate(&mut a, NOW);
        assert_eq!(a.soft_streak, 2);
        assert_eq!(a.cooldown_until, NOW + 1200.0);
        // 连续第 5 次顶到 7200
        for _ in 0..3 {
            note_soft_rate(&mut a, NOW);
        }
        assert_eq!(a.cooldown_until, NOW + 7200.0);
    }

    #[test]
    fn note_unscoped_rate_returns_backoff_only() {
        let mut a = acc("u1");
        assert_eq!(note_unscoped_rate(&mut a), 60.0);
        assert_eq!(a.unscoped_streak, 1);
        assert_eq!(note_unscoped_rate(&mut a), 120.0);
        assert_eq!(a.unscoped_streak, 2);
        // 不改冷却字段——由调用方经 note_error 落
        assert!(a.model_cooldowns.is_empty());
    }

    #[test]
    fn note_error_model_cooldown_with_until() {
        let mut a = acc("u1");
        // 有 reset_at：wait = max(1, until-now)
        note_error(
            &mut a,
            "429 model",
            Some("m"),
            Some(NOW + 50.0),
            600.0,
            false,
            "d",
            NOW,
        );
        assert_eq!(a.model_cooldowns.get("m"), Some(&(NOW + 50.0)));
        // 二次调用 max 保持
        note_error(
            &mut a,
            "429 model",
            Some("m"),
            Some(NOW + 30.0),
            600.0,
            false,
            "d",
            NOW,
        );
        assert_eq!(a.model_cooldowns.get("m"), Some(&(NOW + 50.0)));
        // until 早于 now → wait 地板 1s
        note_error(
            &mut a,
            "stale",
            Some("x"),
            Some(NOW - 10.0),
            600.0,
            false,
            "d",
            NOW,
        );
        assert_eq!(a.model_cooldowns.get("x"), Some(&(NOW + 1.0)));
    }

    #[test]
    fn note_error_without_until_uses_cooldown_or_single() {
        let mut a = acc("u1");
        note_error(
            &mut a,
            "403 restricted",
            Some("m"),
            None,
            7200.0,
            false,
            "d",
            NOW,
        );
        assert_eq!(a.model_cooldowns.get("m"), Some(&(NOW + 7200.0)));
        // single_account → 3s
        note_error(
            &mut a,
            "403 restricted",
            Some("m2"),
            None,
            7200.0,
            true,
            "d",
            NOW,
        );
        assert_eq!(a.model_cooldowns.get("m2"), Some(&(NOW + 3.0)));
        // last_error 更新
        assert_eq!(a.last_error, "403 restricted");
        assert_eq!(a.last_error_detail, "d");
    }

    #[test]
    fn note_model_cap_marks_cap_and_cooldown() {
        let mut a = acc("u1");
        a.free_models.insert("hy3".into());
        note_model_cap(&mut a, "hy3", Some(NOW + 3600.0), NOW);
        assert!(a.model_caps.contains_key("hy3"));
        assert_eq!(a.model_cooldowns.get("hy3"), Some(&(NOW + 3600.0)));
        // 重复同 reset：不重记 cap（at 不变）
        note_model_cap(&mut a, "hy3", Some(NOW + 3600.0), NOW + 10.0);
        assert_eq!(a.model_caps["hy3"].at, NOW);
        // 非免费模型：不记 cap 但仍挂冷却
        note_model_cap(&mut a, "other", Some(NOW + 500.0), NOW);
        assert!(!a.model_caps.contains_key("other"));
        assert_eq!(a.model_cooldowns.get("other"), Some(&(NOW + 500.0)));
    }

    #[test]
    fn note_failure_breaks_after_three() {
        let mut a = acc("u1");
        note_failure(&mut a, "502", NOW);
        note_failure(&mut a, "502", NOW);
        assert_eq!(a.breaker_until, 0.0);
        assert_eq!(a.fails, 2);
        note_failure(&mut a, "502", NOW);
        assert_eq!(a.fails, 3);
        assert_eq!(a.breaker_until, NOW + 1800.0);
        note_failure(&mut a, "502", NOW);
        assert_eq!(a.breaker_until, NOW + 3600.0); // 单调 max
    }

    #[test]
    fn note_unknown_failure_degrades_and_breaks() {
        let mut a = acc("u1");
        for _ in 0..4 {
            note_unknown_failure(&mut a, "timeout", NOW);
        }
        assert_eq!(a.degrade_count, 4);
        assert_eq!(a.degrade_until, 0.0);
        assert_eq!(a.fails, 4);
        assert_eq!(a.breaker_until, NOW + 3600.0); // fails>=3 已熔断
        note_unknown_failure(&mut a, "timeout", NOW);
        assert_eq!(a.degrade_count, 5);
        assert_eq!(a.degrade_until, NOW + 600.0);
    }

    #[test]
    fn model_caps_and_free_models_defaults() {
        let a = acc("u1");
        assert!(a.model_caps.is_empty());
        assert!(a.free_models.is_empty());
        assert_eq!(a.last_error, "");
    }
}
