//! D48：调度器「网络类失败延后补跑」的契约测试（Rust 侧）。
//!
//! 背景（2026-10-21 定位）：宿主 Clash Verge 每天 08:2x 自动更新订阅并重载
//! Mihomo core，容器出站被宿主 TUN 承载 ⇒ 在途 TCP 被打断 ⇒ 签到 / 每日活跃
//! 批量报 `SSL: UNEXPECTED_EOF_WHILE_READING`（偶发 `Errno -3` DNS 临时失败）。
//! 中断持续 1-3 分钟，而整点排程离失败时刻最近的一次在 21:00（12 小时后），
//! 于是早上失败只能靠用户手动补跑。
//!
//! 断言清单（与 Python 侧 `tests/_test_scheduler_retry.py` 同口径）：
//! 1. 网络类错误串被识别为「值得延后补跑」；业务类错误不识别；
//! 2. 巡检里因网络故障失败的签到 / 每日活跃会进补跑队列；
//! 3. 到期后补跑真的再调一次业务方法；成功即出队；
//! 4. 补跑仍遇网络故障 ⇒ 按上限重新排队；业务类失败不再排；
//! 5. 补跑前复查门禁（已做完 / 账号不在池）⇒ 不重复打；
//! 6. 没到期的补跑不提前执行。
//!
//! 全程注入 fake executor + `NoopSleeper` + 可控时钟，**不发任何真实上游请求**。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use wb_tasks::growth_runner::NoopSleeper;
use wb_tasks::scheduler::{
    is_network_failure, AccountView, CycleExecutor, LocalNow, ScheduleClock, Scheduler, StepResult,
    RETRY_DELAY_SECS, RETRY_MAX_TRIES,
};

/// 单步结果 future 的别名（与 `contract_scheduler.rs` 同款）。
type StepFut<'a> = Pin<Box<dyn std::future::Future<Output = StepResult> + Send + 'a>>;

const SSL_EOF: &str = "<urlopen error [SSL: UNEXPECTED_EOF_WHILE_READING] \
                       EOF occurred in violation of protocol (_ssl.c:1016)>";
const DNS_AGAIN: &str = "<urlopen error [Errno -3] Try again>";
const TODAY: &str = "2026-10-21";

// ---------------------------------------------------------------------------
// 测试支架：可控时钟（epoch 可调，用来让补跑到期）
// ---------------------------------------------------------------------------

struct FakeClock {
    now: Mutex<LocalNow>,
    epoch: Mutex<f64>,
}

impl FakeClock {
    fn new(now: LocalNow) -> Self {
        Self {
            now: Mutex::new(now),
            epoch: Mutex::new(0.0),
        }
    }

    fn set_epoch(&self, epoch: f64) {
        *self.epoch.lock().unwrap() = epoch;
    }
}

impl ScheduleClock for FakeClock {
    fn local_now(&self) -> LocalNow {
        *self.now.lock().unwrap()
    }

    fn epoch_secs(&self) -> f64 {
        *self.epoch.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// 测试支架：可编排的 fake executor
// ---------------------------------------------------------------------------

/// 一次调用记录：`(步骤名, uid)`。
type Call = (&'static str, String);

struct FakeExecutor {
    accounts: Mutex<Vec<AccountView>>,
    calls: Arc<Mutex<Vec<Call>>>,
    checkin_results: Mutex<Vec<Value>>,
    daily_chat_results: Mutex<Vec<Value>>,
}

impl FakeExecutor {
    fn new(accounts: Vec<AccountView>) -> Self {
        Self {
            accounts: Mutex::new(accounts),
            calls: Arc::new(Mutex::new(Vec::new())),
            checkin_results: Mutex::new(Vec::new()),
            daily_chat_results: Mutex::new(Vec::new()),
        }
    }

    fn with_checkin_results(self, results: Vec<Value>) -> Self {
        *self.checkin_results.lock().unwrap() = results;
        self
    }

    fn with_daily_chat_results(self, results: Vec<Value>) -> Self {
        *self.daily_chat_results.lock().unwrap() = results;
        self
    }

    fn next(results: &Mutex<Vec<Value>>) -> Value {
        let mut guard = results.lock().unwrap();
        if guard.is_empty() {
            return json!({"ok": true, "msg": "成功"});
        }
        guard.remove(0)
    }

    fn record(&self, step: &'static str, uid: &str) {
        self.calls.lock().unwrap().push((step, uid.to_string()));
    }

    fn count(&self, step: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| *s == step)
            .count()
    }
}

impl CycleExecutor for FakeExecutor {
    fn refresh<'a>(
        &'a self,
        acc: &'a AccountView,
        _trigger: &str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        self.record("refresh", &acc.uid);
        Box::pin(async { true })
    }

    fn checkin<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("checkin", &acc.uid);
        let res = FakeExecutor::next(&self.checkin_results);
        Box::pin(async move { res })
    }

    fn report_activity<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("report", &acc.uid);
        Box::pin(async { json!({"ok": true, "msg": "上报成功"}) })
    }

    fn cat_travel<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("travel", &acc.uid);
        Box::pin(async { json!({"action": "none", "msg": ""}) })
    }

    fn streak_bonus<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("streak", &acc.uid);
        Box::pin(async { json!({"logs": []}) })
    }

    fn night_growth<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("night", &acc.uid);
        Box::pin(async { json!({"logs": []}) })
    }

    fn daily_chat<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("daily_chat", &acc.uid);
        let res = FakeExecutor::next(&self.daily_chat_results);
        Box::pin(async move { res })
    }

    fn accounts(&self) -> Vec<AccountView> {
        self.accounts.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// 账号构造
// ---------------------------------------------------------------------------

fn cn_account(uid: &str) -> AccountView {
    AccountView {
        uid: uid.to_string(),
        realm: "cn".to_string(),
        enabled: true,
        expires_at: None,
        is_enterprise: false,
        last_checkin: None,
        last_activity_report: None,
        last_daily_chat: None,
        daily_chat_granted_today: false,
        trigger: "unknown".to_string(),
    }
}

fn intl_account(uid: &str) -> AccountView {
    AccountView {
        realm: "intl".to_string(),
        ..cn_account(uid)
    }
}

fn now() -> LocalNow {
    LocalNow::new(2026, 10, 21, 8, 30, 0)
}

fn sched<'a>(
    exec: &'a dyn CycleExecutor,
    clock: &'a FakeClock,
    sleeper: &'a NoopSleeper,
) -> Scheduler<'a> {
    Scheduler::new(exec, sleeper, clock)
}

/// 让队列里的项到期：把时钟的 epoch 往前推一大截。
///
/// 必须**每次都往前推**：补跑失败后会按当前 epoch 重新排一次
/// （due = epoch + RETRY_DELAY_SECS），固定 epoch 会让重排的项永远不到期。
fn make_due(clock: &FakeClock) {
    clock.set_epoch(clock.epoch_secs() + 1_000_000.0);
}

// ---------------------------------------------------------------------------
// 1：分类
// ---------------------------------------------------------------------------

#[test]
fn network_error_strings_are_recognised() {
    for text in [
        SSL_EOF,
        DNS_AGAIN,
        "请求超时",
        "<urlopen error [Errno 111] Connection refused>",
        "error sending request for url (https://x/)",
    ] {
        assert!(is_network_failure(text), "网络类错误没被认出来: {text:?}");
    }
}

#[test]
fn business_errors_are_not_network_failures() {
    for text in [
        r#"HTTP 400: {"code":11128,"msg":"request illegal"}"#,
        r#"HTTP 429: {"code":6004,"msg":"usage exceeds frequency limit"}"#,
        "今天已签到",
        "今日已做或不可做",
        "",
    ] {
        assert!(
            !is_network_failure(text),
            "业务类错误被当成网络故障了: {text:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2：入队
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cycle_queues_network_failure_for_retry() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(vec![json!({
        "ok": false,
        "error": SSL_EOF
    })]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;

    let pending = s.pending_retries();
    assert_eq!(pending.len(), 1, "网络故障没有进补跑队列: {pending:?}");
    assert_eq!(pending[0].uid, "cn-net");
    assert_eq!(pending[0].tries, 1);
    assert_eq!(pending[0].due, RETRY_DELAY_SECS);
}

#[tokio::test]
async fn business_failure_is_not_queued() {
    let exec = FakeExecutor::new(vec![cn_account("cn-biz")]).with_checkin_results(vec![json!({
        "ok": false,
        "error": "HTTP 400: {\"code\":11128}"
    })]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    assert!(
        s.pending_retries().is_empty(),
        "业务类失败不该进补跑队列: {:?}",
        s.pending_retries()
    );
}

#[tokio::test]
async fn intl_daily_chat_network_failure_is_queued() {
    let exec = FakeExecutor::new(vec![intl_account("intl-net")])
        .with_daily_chat_results(vec![json!({"ok": false, "error": SSL_EOF})]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    let pending = s.pending_retries();
    assert_eq!(pending.len(), 1, "国际版每日活跃的网络故障没进队列");
    assert_eq!(pending[0].uid, "intl-net");
}

// ---------------------------------------------------------------------------
// 3：补跑
// ---------------------------------------------------------------------------

#[tokio::test]
async fn due_retry_runs_again_and_succeeds() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(vec![
        json!({"ok": false, "error": SSL_EOF}),
        json!({"ok": true, "msg": "签到成功"}),
    ]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    make_due(&clock);
    s.run_due_retries().await;

    assert_eq!(exec.count("checkin"), 2, "补跑没有再调一次签到");
    assert!(s.pending_retries().is_empty(), "成功后没出队");
    assert!(
        s.logs().iter().any(|l| l.contains("补跑成功")),
        "补跑成功没进日志: {:?}",
        s.logs()
    );
}

#[tokio::test]
async fn retry_failure_requeues_until_cap() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(
        (0..8)
            .map(|_| json!({"ok": false, "error": SSL_EOF}))
            .collect(),
    );
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    for _ in 0..6 {
        make_due(&clock);
        s.run_due_retries().await;
    }
    // 初次失败 + 最多 RETRY_MAX_TRIES 次补跑，之后不再排
    assert_eq!(
        exec.count("checkin"),
        1 + RETRY_MAX_TRIES as usize,
        "补跑次数没封顶"
    );
    assert!(
        s.pending_retries().is_empty(),
        "超过上限仍在排队: {:?}",
        s.pending_retries()
    );
}

#[tokio::test]
async fn business_failure_during_retry_is_not_requeued() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(vec![
        json!({"ok": false, "error": SSL_EOF}),
        json!({"ok": false, "error": "HTTP 429: {\"code\":6004}"}),
    ]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    make_due(&clock);
    s.run_due_retries().await;
    assert!(
        s.pending_retries().is_empty(),
        "业务类失败不该再排补跑: {:?}",
        s.pending_retries()
    );
}

// ---------------------------------------------------------------------------
// 4：补跑前的门禁
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_skipped_when_task_already_done() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(vec![json!({
        "ok": false,
        "error": SSL_EOF
    })]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    // 巡检之后、补跑之前，账号变成「今天已签到」（期间别人做完了）
    {
        let mut accs = exec.accounts.lock().unwrap();
        accs[0].last_checkin = Some(format!("{TODAY} 08:00:00"));
    }
    make_due(&clock);
    s.run_due_retries().await;
    assert_eq!(exec.count("checkin"), 1, "已完成的任务被重复打了");
    assert!(s.pending_retries().is_empty(), "取消后没出队");
}

#[tokio::test]
async fn retry_skipped_when_account_left_pool() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(vec![json!({
        "ok": false,
        "error": SSL_EOF
    })]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    *exec.accounts.lock().unwrap() = Vec::new(); // 账号被移除
    make_due(&clock);
    s.run_due_retries().await;
    assert!(s.pending_retries().is_empty(), "账号不在池里却还留着待办");
}

#[tokio::test]
async fn retry_not_due_yet_is_left_alone() {
    let exec = FakeExecutor::new(vec![cn_account("cn-net")]).with_checkin_results(vec![json!({
        "ok": false,
        "error": SSL_EOF
    })]);
    let clock = FakeClock::new(now());
    let sleeper = NoopSleeper::default();
    let s = sched(&exec, &clock, &sleeper);
    s.run_cycle("单测").await;
    s.run_due_retries().await; // 还没到期
    assert_eq!(exec.count("checkin"), 1, "没到期的补跑被提前执行了");
    assert_eq!(s.pending_retries().len(), 1, "没到期的待办丢了");
}
