//! 关卡 4.7 产出物：契约 20 —— growth 快照 TTL 缓存。
//!
//! 计划原文：「`TtlCache<String, (Vec<GrowthTask>, GrowthSummary)>`，TTL 注入、
//! 并发双路、异常不缓存、`invalidate`。」断言清单：
//! 1. **TTL=0 直通**：每次都重新取，缓存不写。
//! 2. **命中不重复请求**：TTL 内第二次调用不触发取数。
//! 3. **异常不写入**：取数失败后缓存仍为空；下一次成功才写入。
//! 4. **`invalidate` 后重取**。
//! 5. **TTL 过期后重取**（注入时钟推进）。
//! 6. **键是 uid**：不同账号互不串台。
//!
//! 全程用注入的计数闭包取数，**不发任何真实上游请求**（M4 计划硬约束）。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use wb_tasks::growth_api::{TaskAccount, TaskEndpoints};
use wb_tasks::snapshot_cache::{growth_snapshot, GrowthSnapshotValue, SnapshotCache};
use wb_tasks::{GrowthSummary, GrowthTask};

// ---------------------------------------------------------------------------
// 测试支架
// ---------------------------------------------------------------------------

static EMPTY_HEADERS: std::sync::LazyLock<HashMap<String, String>> =
    std::sync::LazyLock::new(HashMap::new);

fn account(uid: &'static str) -> TaskAccount<'static> {
    TaskAccount {
        uid,
        token: "test-token",
        enterprise_id: "test-ent",
        chat_headers: &EMPTY_HEADERS,
    }
}

fn task(code: &str) -> GrowthTask {
    GrowthTask {
        task_code: code.to_string(),
        name: code.to_string(),
        description: String::new(),
        jump_url: String::new(),
        status: "not_accepted".to_string(),
        task_type: String::new(),
        current: 0,
        target: 1,
        reward_credit: 0,
        reward_energy: 0,
        unforgeable: false,
        reason: String::new(),
    }
}

/// 可推进的假时钟（秒）。
#[derive(Clone)]
struct Clock(Arc<Mutex<f64>>);

impl Clock {
    fn new(at: f64) -> Self {
        Self(Arc::new(Mutex::new(at)))
    }
    fn advance_to(&self, t: f64) {
        *self.0.lock().unwrap() = t;
    }
    fn reader(&self) -> impl Fn() -> f64 + Send + Sync + 'static {
        let inner = Arc::clone(&self.0);
        move || *inner.lock().unwrap()
    }
}

/// 单次取数脚本项：`Some((task_code, energy))` 成功，`None` 模拟失败。
type ScriptEntry = Option<(&'static str, i64)>;

/// 计数取数器：第 `n` 次调用返回脚本第 `n` 项（超出则重复最后一项）。
#[derive(Clone)]
struct ScriptedFetcher {
    calls: Arc<AtomicUsize>,
    /// 每次调用返回的脚本项；`None` 表示模拟失败。
    script: Arc<Mutex<Vec<ScriptEntry>>>,
}

impl ScriptedFetcher {
    fn new(script: Vec<ScriptEntry>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            script: Arc::new(Mutex::new(script)),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 转成 `SnapshotCache::with_fetch` 所需的闭包。
    #[allow(clippy::type_complexity)]
    fn into_fetch(
        self,
    ) -> impl for<'a> Fn(
        &'a TaskAccount<'a>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<GrowthSnapshotValue, String>> + Send + 'a>,
    > + Send
           + Sync
           + 'static {
        move |_acc| {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let script = Arc::clone(&self.script);
            Box::pin(async move {
                let entry = {
                    let s = script.lock().unwrap();
                    if s.is_empty() {
                        None
                    } else {
                        s[n.min(s.len() - 1)]
                    }
                };
                match entry {
                    Some((code, energy)) => Ok((
                        vec![task(code)],
                        GrowthSummary {
                            energy,
                            ..Default::default()
                        },
                    )),
                    None => Err("upstream boom".to_string()),
                }
            })
        }
    }
}

// ---------------------------------------------------------------------------
// 1. TTL=0 直通
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ttl_zero_is_passthrough_and_always_refetches() {
    let fetcher = ScriptedFetcher::new(vec![Some(("a", 1)), Some(("b", 2))]);
    let counter = fetcher.clone();
    let cache = SnapshotCache::with_ttl(0.0).with_fetch(fetcher.into_fetch());
    assert!(cache.is_passthrough());

    let acc = account("u1");
    let (t1, s1) = growth_snapshot(&cache, &acc).await.unwrap();
    let (t2, s2) = growth_snapshot(&cache, &acc).await.unwrap();

    assert_eq!(counter.call_count(), 2, "TTL=0：每次都重新取");
    assert_eq!(t1[0].task_code, "a");
    assert_eq!(t2[0].task_code, "b", "读到的是第二次取数结果");
    assert_eq!(s1.energy, 1);
    assert_eq!(s2.energy, 2);
    assert!(cache.is_empty(), "直通模式不写缓存");
}

// ---------------------------------------------------------------------------
// 2. 命中不重复请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hit_within_ttl_does_not_refetch() {
    let clock = Clock::new(1000.0);
    let fetcher = ScriptedFetcher::new(vec![Some(("a", 5)), Some(("b", 6))]);
    let counter = fetcher.clone();
    let cache = SnapshotCache::with_ttl(20.0)
        .with_now(clock.reader())
        .with_fetch(fetcher.into_fetch());

    let acc = account("u1");
    let first = growth_snapshot(&cache, &acc).await.unwrap();
    clock.advance_to(1019.0); // TTL 内
    let second = growth_snapshot(&cache, &acc).await.unwrap();

    assert_eq!(counter.call_count(), 1, "TTL 内第二次调用不得再取数");
    assert_eq!(first.0[0].task_code, second.0[0].task_code);
    assert_eq!(cache.len(), 1);
}

// ---------------------------------------------------------------------------
// 3. 异常不写入
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_is_not_cached_and_retry_after_success_caches() {
    let fetcher = ScriptedFetcher::new(vec![None, Some(("ok", 9))]);
    let counter = fetcher.clone();
    let cache = SnapshotCache::with_ttl(60.0).with_fetch(fetcher.into_fetch());

    let acc = account("u1");
    assert!(
        growth_snapshot(&cache, &acc).await.is_err(),
        "取数失败必须向外抛"
    );
    assert!(cache.is_empty(), "失败结果不得写入缓存");

    let ok = growth_snapshot(&cache, &acc).await.unwrap();
    assert_eq!(counter.call_count(), 2, "失败后下一次必须重试");
    assert_eq!(ok.0[0].task_code, "ok");
    assert_eq!(cache.len(), 1);

    // 再取一次应命中，不再请求
    growth_snapshot(&cache, &acc).await.unwrap();
    assert_eq!(counter.call_count(), 2);
}

// ---------------------------------------------------------------------------
// 4. invalidate 后重取
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalidate_forces_refetch() {
    let fetcher = ScriptedFetcher::new(vec![Some(("a", 1)), Some(("b", 2))]);
    let counter = fetcher.clone();
    let cache = SnapshotCache::with_ttl(60.0).with_fetch(fetcher.into_fetch());

    let acc = account("u1");
    growth_snapshot(&cache, &acc).await.unwrap();
    assert_eq!(counter.call_count(), 1);

    cache.invalidate();
    assert!(cache.is_empty(), "invalidate 清空全表");

    let after = growth_snapshot(&cache, &acc).await.unwrap();
    assert_eq!(counter.call_count(), 2, "invalidate 后必须重取");
    assert_eq!(after.0[0].task_code, "b");
}

// ---------------------------------------------------------------------------
// 5. TTL 过期后重取（边界：expiry == now 不命中）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expiry_boundary_requires_strictly_greater() {
    let clock = Clock::new(0.0);
    let fetcher = ScriptedFetcher::new(vec![Some(("a", 1)), Some(("b", 2))]);
    let counter = fetcher.clone();
    let cache = SnapshotCache::with_ttl(20.0)
        .with_now(clock.reader())
        .with_fetch(fetcher.into_fetch());
    let acc = account("u1");

    growth_snapshot(&cache, &acc).await.unwrap();
    clock.advance_to(19.9);
    growth_snapshot(&cache, &acc).await.unwrap();
    assert_eq!(counter.call_count(), 1, "未到过期仍命中");

    clock.advance_to(20.0); // expiry == now，Python 的 `hit[0] > now` 不成立
    let third = growth_snapshot(&cache, &acc).await.unwrap();
    assert_eq!(counter.call_count(), 2, "边界时刻必须重取");
    assert_eq!(third.0[0].task_code, "b");
}

// ---------------------------------------------------------------------------
// 6. 键是 uid，不同账号不串台
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_is_keyed_by_uid() {
    let fetcher = ScriptedFetcher::new(vec![Some(("a", 1)), Some(("b", 2))]);
    let counter = fetcher.clone();
    let cache = SnapshotCache::with_ttl(60.0).with_fetch(fetcher.into_fetch());

    let (u1, u2) = (account("u1"), account("u2"));
    let a = growth_snapshot(&cache, &u1).await.unwrap();
    let b = growth_snapshot(&cache, &u2).await.unwrap();
    assert_eq!(counter.call_count(), 2, "不同 uid 各取一次");
    assert_eq!(a.0[0].task_code, "a");
    assert_eq!(b.0[0].task_code, "b");
    assert_eq!(cache.len(), 2);

    // 回到 u1 应命中自己的条目，而不是 u2 的
    let a2 = growth_snapshot(&cache, &u1).await.unwrap();
    assert_eq!(counter.call_count(), 2);
    assert_eq!(a2.0[0].task_code, "a", "不得串到 u2 的结果");
}

// ---------------------------------------------------------------------------
// 7. 生产构造：常量域名与空表（只读属性，不发请求）
// ---------------------------------------------------------------------------

#[test]
fn production_shape_without_network() {
    let cache = SnapshotCache::production();
    assert!(cache.ttl() >= 0.0);
    assert_eq!(
        cache.endpoints().chat_base,
        TaskEndpoints::production().chat_base
    );
    assert!(cache.is_empty(), "构造时不得有任何条目");
}
