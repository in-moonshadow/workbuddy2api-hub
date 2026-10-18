//! 关卡 4.7：growth 快照 TTL 缓存（契约 20，`wb_proxy.py:5866-5892`）。
//!
//! ## Python 语义（逐条对齐）
//! - `TASKS_CACHE_TTL = float(os.environ.get("WB_TASKS_CACHE_TTL") or "20")`：
//!   环境变量缺省 **20 秒**；空串也回落 20（`or` 的真值判断）。
//! - `TTL <= 0` ⇒ **直通**：不读、不写缓存，每次都向上游取。
//! - 键是 `account.uid`；值是 `(expiry, (tasks, summary))`，命中条件
//!   `expiry > now`（严格大于；过期项不再命中）。
//! - `growth_snapshot` 并发跑 `fetch_growth_tasks` 与 `fetch_growth_summary`；
//!   **任一条抛错就向外抛且不写缓存**（`_both` 的 docstring）。
//! - `invalidate_tasks_cache()` 是 **`clear()`**：清空全表，不是按 uid 删。
//!
//! ## 为什么取数是可注入的
//! Python 的 `growth_snapshot` 直接 import `wb_tasks` 的函数。测试若照抄，
//! 就只能打真实上游（违反 M4 计划「不得对真实上游发任何请求」）。这里把
//! 「取数」收成一个闭包字段：生产用 [`SnapshotCache::production`]（真实 HTTP +
//! 常量域名），测试注入 wiremock 的结果。缓存行为（TTL / 命中 / 失效 /
//! 异常不缓存）与取数实现无关，因此可被完全覆盖。

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::growth_api::{TaskAccount, TaskEndpoints};
use crate::{GrowthSummary, GrowthTask};

/// env 名（`wb_proxy.py:5865`）。
pub const CACHE_TTL_ENV: &str = "WB_TASKS_CACHE_TTL";
/// 缺省 TTL：20 秒。
pub const DEFAULT_CACHE_TTL_SECS: f64 = 20.0;

/// 从环境变量读 TTL。
pub fn cache_ttl_from_env() -> f64 {
    parse_ttl(std::env::var(CACHE_TTL_ENV).ok().as_deref())
}

/// 纯函数形式，便于不碰进程环境地测试解析规则。
///
/// 对齐 Python `float(os.environ.get(env) or "20")`：缺省与空串都取 20。
/// Python 对非法字符串会在 `float()` 抛异常；这里回落缺省（有意容错，账本登记）。
pub fn parse_ttl(raw: Option<&str>) -> f64 {
    match raw {
        Some(s) if !s.is_empty() => s.parse::<f64>().unwrap_or(DEFAULT_CACHE_TTL_SECS),
        _ => DEFAULT_CACHE_TTL_SECS,
    }
}

type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// 进程单调时钟（距首次调用经过的秒数），避免系统时间回拨影响 TTL。
pub fn monotonic_now() -> f64 {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let t = Instant::now();
    let origin = *ORIGIN.get_or_init(|| t);
    t.duration_since(origin).as_secs_f64()
}

/// 通用 TTL 缓存（注入 TTL 与 now）。
///
/// - `ttl <= 0` ⇒ 直通：`get` 恒 `None`、`put` 直接丢弃。
/// - 过期项在 `get` 时即被移除。对外可观测行为与 Python 一致：过期后一定重新取。
#[derive(Clone)]
pub struct TtlCache<K, V> {
    ttl: f64,
    now: NowFn,
    inner: Arc<Mutex<HashMap<K, (f64, V)>>>,
}

impl<K, V> std::fmt::Debug for TtlCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtlCache").field("ttl", &self.ttl).finish()
    }
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    /// 以进程单调时钟计时。
    pub fn new(ttl_secs: f64) -> Self {
        Self::with_now(ttl_secs, monotonic_now)
    }

    /// 注入 now（单元测试推进时间用；生产用 [`TtlCache::new`]）。
    pub fn with_now<F>(ttl_secs: f64, now: F) -> Self
    where
        F: Fn() -> f64 + Send + Sync + 'static,
    {
        Self::with_now_fn(ttl_secs, Arc::new(now))
    }

    /// 内部构造：直接接收已装箱的时钟（[`SnapshotCache::with_now`] 需要共享
    /// 同一份 `Arc` 给两层缓存，避免闭包捕获自引用）。
    fn with_now_fn(ttl_secs: f64, now: NowFn) -> Self {
        Self {
            ttl: ttl_secs,
            now,
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn ttl(&self) -> f64 {
        self.ttl
    }

    /// `ttl <= 0`：Python 的「关闭缓存、每次直连」。
    pub fn is_passthrough(&self) -> bool {
        self.ttl <= 0.0
    }

    /// 命中返回 `Some(clone)`；过期即删并返回 `None`。
    pub fn get(&self, key: &K) -> Option<V> {
        if self.is_passthrough() {
            return None;
        }
        let now = (self.now)();
        let mut guard = self.lock();
        match guard.get(key) {
            Some((expiry, v)) if *expiry > now => Some(v.clone()),
            Some(_) => {
                guard.remove(key);
                None
            }
            None => None,
        }
    }

    /// 写入（直通模式下丢弃）。过期时刻为 `now + ttl`。
    pub fn put(&self, key: K, value: V) {
        if self.is_passthrough() {
            return;
        }
        let expiry = (self.now)() + self.ttl;
        self.lock().insert(key, (expiry, value));
    }

    /// `invalidate_tasks_cache()`：清空全表。
    pub fn invalidate(&self) {
        self.lock().clear();
    }

    /// 当前条目数（测试与可观测性用）。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, (f64, V)>> {
        // 锁毒化只可能来自其它线程 panic；缓存是纯派生数据，恢复即可
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 快照缓存的值：`(tasks, summary)`。
pub type GrowthSnapshotValue = (Vec<GrowthTask>, GrowthSummary);

/// 取数闭包：给定账号，并发取两份数据。`Err` 表示**任一路失败**。
pub type SnapshotFetcher = Arc<
    dyn for<'a> Fn(
            &'a TaskAccount<'a>,
        )
            -> Pin<Box<dyn Future<Output = Result<GrowthSnapshotValue, String>> + Send + 'a>>
        + Send
        + Sync,
>;

/// 快照缓存：`uid → (tasks, summary)`，TTL 注入。
pub struct SnapshotCache {
    ttl: f64,
    now: NowFn,
    endpoints: TaskEndpoints<'static>,
    headers: HashMap<String, String>,
    fetch: SnapshotFetcher,
    cache: TtlCache<String, GrowthSnapshotValue>,
}

impl std::fmt::Debug for SnapshotCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotCache")
            .field("ttl", &self.ttl)
            .field("endpoints", &self.endpoints)
            .field("entries", &self.cache.len())
            .finish()
    }
}

impl SnapshotCache {
    /// 最小构造：TTL 给定；取数暂未配置（必须先 [`SnapshotCache::with_fetch`]
    /// 或改用 [`SnapshotCache::production`]，否则调用返回 `Err` 且不缓存）。
    pub fn with_ttl(ttl_secs: f64) -> Self {
        Self {
            ttl: ttl_secs,
            now: Arc::new(monotonic_now),
            endpoints: TaskEndpoints::production(),
            headers: HashMap::new(),
            fetch: Arc::new(|_acc| {
                Box::pin(async { Err("snapshot fetcher not configured".into()) })
            }),
            cache: TtlCache::new(ttl_secs),
        }
    }

    /// 生产口径：env TTL + 常量域名 + 真实 HTTP 并发取数。
    pub fn production() -> Self {
        Self::with_ttl(cache_ttl_from_env()).with_fetch(production_fetch)
    }

    pub fn ttl(&self) -> f64 {
        self.ttl
    }

    pub fn endpoints(&self) -> TaskEndpoints<'static> {
        self.endpoints
    }

    pub fn with_endpoints(mut self, ep: TaskEndpoints<'static>) -> Self {
        self.endpoints = ep;
        self
    }

    /// 出站头（生产 = `chat_request_headers` 产物；缓存本身不直接使用，
    /// 保留给调度器/诊断展示）。
    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    /// 注入取数实现（测试用 wiremock 结果；生产用 [`Self::production`]）。
    pub fn with_fetch<F>(mut self, f: F) -> Self
    where
        F: for<'a> Fn(
                &'a TaskAccount<'a>,
            ) -> Pin<
                Box<dyn Future<Output = Result<GrowthSnapshotValue, String>> + Send + 'a>,
            > + Send
            + Sync
            + 'static,
    {
        self.fetch = Arc::new(f);
        self
    }

    /// 注入已装箱的取数实现（测试/生产装配用）。
    #[allow(clippy::type_complexity)]
    pub fn with_fetch_arc(
        mut self,
        f: Arc<
            dyn for<'a> Fn(
                    &'a TaskAccount<'a>,
                ) -> Pin<
                    Box<
                        dyn std::future::Future<Output = Result<GrowthSnapshotValue, String>>
                            + Send
                            + 'a,
                    >,
                > + Send
                + Sync,
        >,
    ) -> Self {
        self.fetch = f;
        self
    }

    /// 注入时钟（秒）。
    pub fn with_now<F>(mut self, now: F) -> Self
    where
        F: Fn() -> f64 + Send + Sync + 'static,
    {
        self.now = Arc::new(now);
        self.cache = TtlCache::with_now_fn(self.ttl, Arc::clone(&self.now));
        self
    }

    /// `invalidate_tasks_cache()`：任何会改变任务状态的动作都要调用。
    pub fn invalidate(&self) {
        self.cache.invalidate();
    }

    /// 当前缓存条目数。
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// 直通模式（`TTL <= 0`）。
    pub fn is_passthrough(&self) -> bool {
        self.cache.is_passthrough()
    }

    pub fn ttl_duration(&self) -> Duration {
        if self.ttl <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(self.ttl)
        }
    }
}

/// 生产取数：两路 `tokio::join!` 并发（Python 是 2 线程池）。
///
/// 任务侧 `fetch_growth_tasks` 与汇总侧 `fetch_growth_summary` 都按 Python
/// 语义吞异常、降级成缺省值，因此本闭包**恒 `Ok`**。「异常不写入缓存」的
/// 分支仍由 [`growth_snapshot`] 承担，并由注入式取数的测试覆盖。
pub fn production_fetch<'a>(
    acc: &'a TaskAccount<'a>,
) -> Pin<Box<dyn Future<Output = Result<GrowthSnapshotValue, String>> + Send + 'a>> {
    Box::pin(async move {
        let ep = TaskEndpoints::production();
        let client = wb_gateway::outbound::UpstreamClient::new();
        let (tasks, summary) = tokio::join!(
            crate::growth_api::fetch_growth_tasks(&client, acc, &ep),
            crate::summary::fetch_growth_summary(&client, acc, &ep),
        );
        Ok((tasks, summary))
    })
}

/// 契约 20：成长任务 + 汇总；TTL 内复用，过期或未命中才重新取。
///
/// - `TTL <= 0` ⇒ 直通（不读不写缓存）；
/// - 取数失败 ⇒ 原样返回 `Err`，**不写缓存**（下一位调用者会重试）；
/// - 取数成功 ⇒ 写入，过期时刻 `now + ttl`。
pub async fn growth_snapshot(
    cache: &SnapshotCache,
    acc: &TaskAccount<'_>,
) -> Result<GrowthSnapshotValue, String> {
    if !cache.is_passthrough() {
        if let Some(hit) = cache.cache.get(&acc.uid.to_string()) {
            return Ok(hit);
        }
    }
    let data = (cache.fetch)(acc).await?;
    if !cache.is_passthrough() {
        cache.cache.put(acc.uid.to_string(), data.clone());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ttl_matches_python_or_default() {
        assert_eq!(parse_ttl(None), 20.0);
        assert_eq!(parse_ttl(Some("")), 20.0, "空串在 Python 里走 or 缺省");
        assert_eq!(parse_ttl(Some("5.5")), 5.5);
        assert_eq!(parse_ttl(Some("0")), 0.0);
        assert_eq!(parse_ttl(Some("-1")), -1.0);
        assert_eq!(parse_ttl(Some("abc")), 20.0, "非法值回落缺省");
    }

    #[test]
    fn ttl_zero_is_passthrough() {
        let c: TtlCache<String, i32> = TtlCache::with_now(0.0, || 0.0);
        c.put("a".into(), 1);
        assert!(c.is_passthrough());
        assert!(c.get(&"a".into()).is_none());
        assert!(c.is_empty(), "直通模式不写缓存");
    }

    #[test]
    fn hit_requires_expiry_strictly_greater_than_now() {
        let clock = Arc::new(Mutex::new(0.0f64));
        let c: TtlCache<String, i32> = {
            let clock = Arc::clone(&clock);
            TtlCache::with_now(10.0, move || *clock.lock().unwrap())
        };
        c.put("k".into(), 7);
        assert_eq!(c.get(&"k".into()), Some(7));
        *clock.lock().unwrap() = 10.0; // expiry == now ⇒ 不再命中
        assert_eq!(c.get(&"k".into()), None);
        assert!(c.is_empty(), "过期项在读取时移除");
    }

    #[test]
    fn invalidate_clears_every_key() {
        let c: TtlCache<String, i32> = TtlCache::with_now(60.0, || 0.0);
        c.put("a".into(), 1);
        c.put("b".into(), 2);
        assert_eq!(c.len(), 2);
        c.invalidate();
        assert!(c.is_empty());
    }
}
