//! 关卡 4.18（线 B）产出物：契约 —— 后台积分刷新线程（`CreditsRefresher`）。
//!
//! 权威源：Python `wb_accounts.py` 的 `CreditsRefresher`（`:2451-2586`）。
//!
//! 断言清单：
//! 1. 两个候选时**只刷最旧的一个**（`fetch` 次数 == 1）。
//! 2. TTL 未到的账号不刷。
//! 3. 从未抓过的账号优先于已抓过的（Python `:2527-2531`：从未抓过 = 无限旧）。
//! 4. 失败账号在 6 小时冷却内不再被选；冷却过后可再被选。
//! 5. 成功后 `save_credits` 被调用、`last_uid` 更新、失败记录清除。
//! 6. `ttl_hours == 0`（关闭）时不刷任何账号。
//! 7. `enabled == false` 或无 token 的账号不刷。
//! 8. `status()` 的键名与 Python 一致（六个键都在）。
//! 9. 一次失败后 `last_error` 非空且仍保留在 status 里。
//!
//! 全程注入 fake `CreditsSource`，**不发任何真实上游请求**（M5 计划硬约束）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use wb_tasks::credits_refresher::{
    CreditsRefresher, CreditsSource, RefresherAccount, FAILED_COOLDOWN_SECONDS, INTERVAL_SECONDS,
};

// ---------------------------------------------------------------------------
// 测试支架：fake 数据源（不触网）
// ---------------------------------------------------------------------------

/// 记录抓取/落盘次数与内容的 fake 数据源。
struct FakeSource {
    accounts: Mutex<Vec<RefresherAccount>>,
    /// 当前 epoch 秒（可控时钟）。
    now: Mutex<f64>,
    /// 抓取次数（断言「只刷一个」用）。
    fetch_count: AtomicUsize,
    /// 每次抓取的 uid（按顺序）。
    fetched: Mutex<Vec<String>>,
    /// 落盘次数。
    save_count: AtomicUsize,
    /// 落盘的快照（最后一次）。
    saved: Mutex<Option<Value>>,
    /// 让指定 uid 的抓取失败（`uid -> 错误文案`）。
    fail_for: Mutex<HashMap<String, String>>,
}

impl FakeSource {
    fn new(accounts: Vec<RefresherAccount>, now: f64) -> Arc<FakeSource> {
        Arc::new(FakeSource {
            accounts: Mutex::new(accounts),
            now: Mutex::new(now),
            fetch_count: AtomicUsize::new(0),
            fetched: Mutex::new(Vec::new()),
            save_count: AtomicUsize::new(0),
            saved: Mutex::new(None),
            fail_for: Mutex::new(HashMap::new()),
        })
    }

    fn set_now(&self, now: f64) {
        *self.now.lock().unwrap() = now;
    }

    fn fail(&self, uid: &str, msg: &str) {
        self.fail_for
            .lock()
            .unwrap()
            .insert(uid.to_string(), msg.to_string());
    }

    fn fetch_count(&self) -> usize {
        self.fetch_count.load(Ordering::SeqCst)
    }

    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().unwrap().clone()
    }

    fn save_count(&self) -> usize {
        self.save_count.load(Ordering::SeqCst)
    }

    fn saved(&self) -> Option<Value> {
        self.saved.lock().unwrap().clone()
    }
}

impl CreditsSource for FakeSource {
    fn accounts(&self) -> Vec<RefresherAccount> {
        self.accounts.lock().unwrap().clone()
    }

    fn now(&self) -> f64 {
        *self.now.lock().unwrap()
    }

    fn fetch_credits<'a>(
        &'a self,
        acc: &'a RefresherAccount,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            self.fetched.lock().unwrap().push(acc.uid.clone());
            if let Some(msg) = self.fail_for.lock().unwrap().get(&acc.uid).cloned() {
                return Err(msg);
            }
            Ok(json!({
                "remain": 10.0,
                "used": 0.0,
                "size": 10.0,
                "updated_at": self.now(),
                "updated_iso": "2026-10-21 12:00:00",
            }))
        })
    }

    fn save_credits(&self, _acc: &RefresherAccount, credits: &Value) -> Result<(), String> {
        self.save_count.fetch_add(1, Ordering::SeqCst);
        *self.saved.lock().unwrap() = Some(credits.clone());
        Ok(())
    }
}

/// 构造一个账号视图。
fn account(uid: &str, updated_at: Option<f64>) -> RefresherAccount {
    RefresherAccount {
        uid: uid.to_string(),
        file_name: format!("{uid}.json"),
        enabled: true,
        has_token: true,
        credits_updated_at: updated_at,
    }
}

/// 一个固定的“现在”（2026-10-21 12:00:00 附近的 epoch 秒，纯测试用）。
const NOW: f64 = 1_800_000_000.0;

// ---------------------------------------------------------------------------
// 1. 两个候选时只刷最旧的一个
// ---------------------------------------------------------------------------

#[tokio::test]
async fn picks_only_the_stalest_account_per_tick() {
    // 两个都已过 12h TTL：old 的快照更旧。
    let src = FakeSource::new(
        vec![
            account("fresh", Some(NOW - 13.0 * 3600.0)),
            account("old", Some(NOW - 30.0 * 3600.0)),
        ],
        NOW,
    );
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    let uid = refresher.run_once().await;
    assert_eq!(uid.as_deref(), Some("old"), "应刷积分快照最旧的那个");
    assert_eq!(src.fetch_count(), 1, "每 tick 只刷一个账号");
    assert_eq!(src.fetched(), vec!["old".to_string()]);
}

// ---------------------------------------------------------------------------
// 2. TTL 未到的账号不刷
// ---------------------------------------------------------------------------

#[tokio::test]
async fn skips_accounts_inside_the_ttl() {
    let src = FakeSource::new(vec![account("young", Some(NOW - 60.0))], NOW);
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    assert_eq!(refresher.run_once().await, None);
    assert_eq!(src.fetch_count(), 0, "TTL 未到不该发请求");
    assert!(refresher.stalest_account(NOW).is_none());
}

// ---------------------------------------------------------------------------
// 3. 从未抓过的账号优先于已抓过的
// ---------------------------------------------------------------------------

#[tokio::test]
async fn never_fetched_account_wins_over_fetched_one() {
    // fetched 的快照很旧（30h），never 从未抓过 ⇒ Python 视为无限旧（:2527-2531）。
    let src = FakeSource::new(
        vec![
            account("fetched", Some(NOW - 30.0 * 3600.0)),
            account("never", None),
        ],
        NOW,
    );
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    let uid = refresher.run_once().await;
    assert_eq!(uid.as_deref(), Some("never"));
    assert_eq!(src.fetch_count(), 1);
}

// ---------------------------------------------------------------------------
// 4. 失败冷却 6 小时
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_account_is_parked_for_six_hours() {
    let src = FakeSource::new(vec![account("bad", None)], NOW);
    src.fail("bad", "boom");
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    // 第一次：失败。
    assert_eq!(refresher.run_once().await, None);
    assert_eq!(src.fetch_count(), 1);
    let parked_until = refresher.parked_until("bad").expect("失败后应进入冷却");
    assert!(
        (parked_until - (NOW + FAILED_COOLDOWN_SECONDS)).abs() < 1e-6,
        "冷却应到 6 小时后，实际 {parked_until}"
    );

    // 冷却期内：不再被选。
    src.set_now(NOW + 3600.0);
    assert_eq!(refresher.run_once().await, None);
    assert_eq!(src.fetch_count(), 1, "冷却期内不该再打上游");

    // 冷却过后：可以再被选。
    src.set_now(NOW + FAILED_COOLDOWN_SECONDS + 1.0);
    assert_eq!(refresher.run_once().await, None, "仍失败");
    assert_eq!(src.fetch_count(), 2, "冷却过后应重新尝试");
}

// ---------------------------------------------------------------------------
// 5. 成功路径
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_refresh_saves_snapshot_and_records_uid() {
    let src = FakeSource::new(vec![account("ok", None)], NOW);
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    let uid = refresher.run_once().await;
    assert_eq!(uid.as_deref(), Some("ok"));
    assert_eq!(src.save_count(), 1, "成功后应落盘快照");
    let saved = src.saved().expect("应有落盘内容");
    assert_eq!(saved["remain"], 10.0);

    let status = refresher.status();
    assert_eq!(status["last_uid"], "ok");
    assert!(status["last_error"].is_null(), "成功后应清掉 last_error");
    assert!(!status["last_run"].is_null(), "成功后应记 last_run");
    assert!(refresher.parked_until("ok").is_none(), "不应有冷却记录");
}

#[tokio::test]
async fn success_clears_the_failure_record() {
    let src = FakeSource::new(vec![account("flip", None)], NOW);
    src.fail("flip", "boom");
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    assert_eq!(refresher.run_once().await, None);
    assert!(refresher.parked_until("flip").is_some(), "失败后有冷却记录");

    // 冷却过后改成成功：失败记录应被清掉。
    src.set_now(NOW + FAILED_COOLDOWN_SECONDS + 1.0);
    {
        let mut map = src.fail_for.lock().unwrap();
        map.remove("flip");
    }
    assert_eq!(refresher.run_once().await, Some("flip".to_string()));
    assert!(
        refresher.parked_until("flip").is_none(),
        "成功后应清除该 uid 的失败记录"
    );
}

// ---------------------------------------------------------------------------
// 6. TTL = 0（关闭）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zero_ttl_disables_the_refresher() {
    let src = FakeSource::new(vec![account("off", None)], NOW);
    let refresher = CreditsRefresher::new(src.clone(), 0.0);

    assert_eq!(refresher.run_once().await, None);
    assert_eq!(src.fetch_count(), 0, "关闭时一个都不刷");
    assert_eq!(src.save_count(), 0);
    assert!(refresher.stalest_account(NOW).is_none());
}

// ---------------------------------------------------------------------------
// 7. 停用 / 无 token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_or_tokenless_accounts_are_skipped() {
    let mut disabled = account("disabled", None);
    disabled.enabled = false;
    let mut tokenless = account("tokenless", None);
    tokenless.has_token = false;

    let src = FakeSource::new(vec![disabled, tokenless], NOW);
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    assert_eq!(refresher.run_once().await, None);
    assert_eq!(src.fetch_count(), 0, "停用 / 无 token 的账号不刷");
}

// ---------------------------------------------------------------------------
// 8. status() 的键名
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_keys_match_python() {
    let src = FakeSource::new(vec![account("a", None)], NOW);
    let refresher = CreditsRefresher::new(src, 12.0);

    let status = refresher.status();
    let obj = status.as_object().expect("status 必须是对象");
    for key in [
        "ttl_hours",
        "interval_seconds",
        "last_run",
        "last_uid",
        "last_error",
        "logs",
    ] {
        assert!(obj.contains_key(key), "status 缺少键 `{key}`");
    }
    assert_eq!(obj.len(), 6, "status 应恰好六个键");
    assert_eq!(status["ttl_hours"], 12.0);
    assert_eq!(status["interval_seconds"], INTERVAL_SECONDS);
    assert!(status["logs"].is_array());
}

// ---------------------------------------------------------------------------
// 9. 失败后 last_error 非空且保留
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failure_keeps_last_error_in_status() {
    let src = FakeSource::new(vec![account("bad", None)], NOW);
    src.fail("bad", "boom");
    let refresher = CreditsRefresher::new(src.clone(), 12.0);

    assert_eq!(refresher.run_once().await, None);
    let status = refresher.status();
    assert_eq!(status["last_error"], "boom", "失败后 last_error 应非空");
    assert!(status["last_uid"].is_null(), "失败不算成功，不记 last_uid");
    assert!(status["last_run"].is_null(), "失败不算成功，不记 last_run");

    // 再跑一轮（仍在冷却里）：last_error 依然保留。
    assert_eq!(refresher.run_once().await, None);
    assert_eq!(refresher.status()["last_error"], "boom");
    assert_eq!(src.fetch_count(), 1);
}
