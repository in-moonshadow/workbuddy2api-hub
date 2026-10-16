//! 出口级瞬时失败退避（issue #271），移植自 wb_egress.py。
//!
//! 账号级熔断把「连续 3 次硬失败」记在账号头上（30 分钟起、翻倍到 6 小时）；
//! 当失败来自出口/链路的瞬时抖动而非账号本身时，一次抖动会被记到每个账号头上，
//! 现场实测 4 小时 196 次 URLError 陆续关掉 13 个账号。本模块把这类失败改记在
//! **出口**上：同一出口的账号共用一个计数器，一次抖动只换来一段**秒级**退避。
//!
//! 尺度对比：账号级阶梯分钟级（30m 起），出口级秒级（1s 起、翻倍、封顶 15s、
//! 带抖动）。M3 无代理接线，出口键 = `scheme://authority`（M5 代理展开时并入代理串）。

use std::collections::HashMap;
use std::sync::Mutex;

/// 第一次抖动的退避秒数；之后每次翻倍，封顶 EGRESS_BACKOFF_MAX（wb_egress.py:22-23）。
pub const EGRESS_BACKOFF_BASE: f64 = 1.0;
pub const EGRESS_BACKOFF_MAX: f64 = 15.0;
/// 距上次抖动超过这么久，就把连续计数当作新的一轮（wb_egress.py:25）。
pub const EGRESS_STREAK_RESET: f64 = 60.0;
/// 抖动比例区间：实际退避在计算值的 0.75~1.25 倍之间，避免所有请求同步重试。
pub const EGRESS_JITTER_LOW: f64 = 0.75;
pub const EGRESS_JITTER_HIGH: f64 = 1.25;

/// transient_backoff（wb_egress.py:35-39）：连续 `streak` 次抖动后的退避（未加抖动）。
/// step = max(0, streak-1)（先减 1 再取非负），指数封顶 step 20，再截 EGRESS_BACKOFF_MAX。
pub fn transient_backoff(streak: i64) -> f64 {
    let step = (streak - 1).clamp(0, 20);
    (EGRESS_BACKOFF_BASE * 2f64.powi(step as i32)).min(EGRESS_BACKOFF_MAX)
}

/// jitter（wb_egress.py:41-44）：wait 乘以 [0.75, 1.25] 区间内的一个比例。
/// `r ∈ [0,1)` 由调用方注入——测试用固定值，生产用 uniform()。
pub fn jitter(wait: f64, r: f64) -> f64 {
    wait * (EGRESS_JITTER_LOW + r * (EGRESS_JITTER_HIGH - EGRESS_JITTER_LOW))
}

/// 均匀 [0,1)。复用 workspace 既有 getrandom（leaked.rs new_call_id 同源），
/// 不新增依赖。
fn uniform() -> f64 {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("os rng must be available");
    let n = u64::from_le_bytes(bytes);
    (n as f64) / (u64::MAX as f64)
}

/// egress_key（wb_upstream_pool.py:579-591）：出站路径的稳定标识——同一物理
/// 出口的账号落在同一个键上。Python 返回 (scheme, host, port, 代理串)；M3 无
/// 代理，`scheme://authority`（路径/查询不参与键）即等价口径。
pub fn egress_key(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, tail)) => {
            let authority = tail.split(['/', '?', '#']).next().unwrap_or("");
            format!("{scheme}://{authority}")
        }
        None => url.to_string(),
    }
}

/// 出口退避状态：`key -> (streak, last_hit_epoch)`。Mutex 保护（M3 并发闸
/// 之后仍是多请求并发；Python 用 threading.Lock）。
#[derive(Default)]
pub struct EgressState {
    inner: Mutex<HashMap<String, (i64, f64)>>,
}

impl EgressState {
    /// note_transient（wb_egress.py:46-58）：记一次出口级瞬时失败，返回这次
    /// 应当退避的秒数（带抖动）。距上次抖动超过 60s → 视为新一轮（streak 重置 0）。
    pub fn note_transient(&self, key: &str, now: f64) -> f64 {
        self.note_transient_jittered(key, now, &|w| jitter(w, uniform()))
    }

    /// 测试入口：jitter 注入固定函数（Python 测试 monkeypatch _jitter 同思路）。
    pub fn note_transient_jittered(
        &self,
        key: &str,
        now: f64,
        jitter_fn: &dyn Fn(f64) -> f64,
    ) -> f64 {
        let mut map = self.inner.lock().unwrap();
        let state = map
            .get(key)
            .filter(|(_, last)| now - *last <= EGRESS_STREAK_RESET)
            .copied()
            .unwrap_or((0, now));
        let streak = state.0 + 1;
        map.insert(key.into(), (streak, now));
        jitter_fn(transient_backoff(streak))
    }

    /// note_success（wb_egress.py:60-66）：这一段出口成功了一次，清零连续计数。
    pub fn note_success(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }

    /// streak（wb_egress.py:68-75）：当前连续抖动次数（测试与排障用）。
    pub fn streak(&self, key: &str, now: f64) -> i64 {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .filter(|(_, last)| now - *last <= EGRESS_STREAK_RESET)
            .map(|(s, _)| *s)
            .unwrap_or(0)
    }

    /// reset：清空全部状态（测试用，wb_egress.py:78-81）。
    pub fn reset(&self) {
        self.inner.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_backoff_staircase_capped() {
        assert_eq!(transient_backoff(1), 1.0);
        assert_eq!(transient_backoff(2), 2.0);
        assert_eq!(transient_backoff(3), 4.0);
        assert_eq!(transient_backoff(4), 8.0);
        assert_eq!(transient_backoff(5), 15.0); // 16 顶到 15
        assert_eq!(transient_backoff(6), 15.0); // 32 顶到 15
        assert_eq!(transient_backoff(25), 15.0); // step 顶 20 后再顶 15
        assert_eq!(transient_backoff(0), 1.0); // max(0, -1)=0
    }

    #[test]
    fn jitter_scales_into_range() {
        assert_eq!(jitter(10.0, 0.0), 7.5);
        assert_eq!(jitter(10.0, 0.5), 10.0);
        assert_eq!(jitter(10.0, 1.0), 12.5);
    }

    #[test]
    fn egress_key_strips_path_and_query() {
        assert_eq!(
            egress_key("https://open.workbuddy.ai/v2/chat/completions?x=1"),
            "https://open.workbuddy.ai"
        );
        assert_eq!(
            egress_key("https://www.workbuddy.cn/"),
            "https://www.workbuddy.cn"
        );
        // 端口保留在 authority 里 → 不同端口不同出口
        assert_eq!(
            egress_key("http://127.0.0.1:8789/v1/chat/completions"),
            "http://127.0.0.1:8789"
        );
        // 无 scheme → 原样（退化路径，正常 URL 不会出现）
        assert_eq!(egress_key("open.workbuddy.ai"), "open.workbuddy.ai");
    }

    #[test]
    fn note_transient_streak_and_reset() {
        let s = EgressState::default();
        let key = "https://open.workbuddy.ai";
        // 注入固定 r=0.5 → jitter = 1.0×（无抖动放大）
        assert_eq!(
            s.note_transient_jittered(key, 1_000.0, &|w| jitter(w, 0.5)),
            1.0
        );
        assert_eq!(s.streak(key, 1_000.0), 1);
        // 连续 → streak 2 → 2s
        assert_eq!(
            s.note_transient_jittered(key, 1_001.0, &|w| jitter(w, 0.5)),
            2.0
        );
        assert_eq!(s.streak(key, 1_001.0), 2);
        // 成功清零
        s.note_success(key);
        assert_eq!(s.streak(key, 1_002.0), 0);
        // 再次抖动 → 回到 1s
        assert_eq!(
            s.note_transient_jittered(key, 1_002.0, &|w| jitter(w, 0.5)),
            1.0
        );
    }

    #[test]
    fn note_transient_restarts_after_streak_reset_window() {
        let s = EgressState::default();
        let key = "https://open.workbuddy.ai";
        assert_eq!(
            s.note_transient_jittered(key, 1_000.0, &|w| jitter(w, 0.5)),
            1.0
        );
        // 隔 61s → 新一轮，streak 回到 1
        assert_eq!(
            s.note_transient_jittered(key, 1_061.0, &|w| jitter(w, 0.5)),
            1.0
        );
        assert_eq!(s.streak(key, 1_061.0), 1);
    }

    #[test]
    fn distinct_keys_have_independent_counters() {
        let s = EgressState::default();
        let a = "https://open.workbuddy.ai";
        let b = "https://www.workbuddy.cn";
        assert_eq!(
            s.note_transient_jittered(a, 1_000.0, &|w| jitter(w, 0.5)),
            1.0
        );
        assert_eq!(
            s.note_transient_jittered(a, 1_001.0, &|w| jitter(w, 0.5)),
            2.0
        );
        assert_eq!(
            s.note_transient_jittered(b, 1_002.0, &|w| jitter(w, 0.5)),
            1.0
        );
        assert_eq!(s.streak(a, 1_002.0), 2);
        assert_eq!(s.streak(b, 1_002.0), 1);
    }
}
