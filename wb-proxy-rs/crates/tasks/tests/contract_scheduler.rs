//! 关卡 4.8 产出物：契约 —— 后台定时调度器状态机。
//!
//! 计划原文：「4.8 调度器状态机（小时表、30s 轮询、65s 抑制、`_calc_next_fire`、
//! `trigger_now`、`status()` 七键）」。权威源是 `wb_scheduler.py`。
//!
//! 断言清单：
//! 1. 常量表（小时表 1/9/21/22、10/30/65/1.0/1.5/7200/60/20）。
//! 2. `calc_next_fire`：严格大于 / 同小时 `<10s` 窗口 / 明天回绕（跨月、跨年、闰年）。
//! 3. `run_loop` 休眠序列：10s 启动 → 整点命中 → 65s 抑制 → 30s 轮询。
//! 4. 非阻塞互斥：并发两轮只有一轮真正执行，另一轮记「跳过本次巡检」。
//! 5. `trigger_now` 两档回执逐字，且锁被占时拒单。
//! 6. `enabled=False`：整点不执行（启动巡检仍跑一次）。
//! 7. 单账号 `enabled=False` 跳过。
//! 8. 保活阈值：`exp - now < 7200` 才刷新；`0`/`None` 不刷新。
//! 9. 国内版四步顺序 + 每步 1.0s；国际版 1.5s。
//! 10. 夜猫只在 01:00 触发，且触发后无条件再睡 1.0s。
//! 11. 日志：`[时间戳] ` 前缀、环形 60、status 尾 20、tag 分流、面板 `<[调度器] ...>` 转发。
//! 12. 五计数汇总行逐字。
//! 13. 单步失败不中断整轮，失败不计入成功数。
//! 14. `status()` 七键且顺序与 Python 一致（workspace 启用 `preserve_order`）。
//!
//! 全程注入 fake executor + `NoopSleeper` + 可控时钟，**不发任何真实上游请求**。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use wb_tasks::growth_runner::{NoopSleeper, Sleeper};
use wb_tasks::scheduler::{
    calc_next_fire, derive_log_tag, is_hour_hit, needs_keepalive, uid8, AccountView, CycleCounts,
    CycleExecutor, LocalNow, PanelSink, ScheduleClock, Scheduler, ALL_HOURS, CAT_HOURS,
    CHECKIN_HOURS, CN_STEP_PAUSE_SECS, HIT_SUPPRESS_SECS, INTL_STEP_PAUSE_SECS, KEEPALIVE_HOURS,
    KEEPALIVE_THRESHOLD_SECS, LAST_RUN_NEVER, LOG_RING_CAPACITY, MODE_INTL_TEXT, MODE_TEXT,
    NEXT_RUN_PENDING, POLL_SECS, STARTUP_DELAY_SECS, STATUS_LOG_TAIL, TRAVEL_HOURS,
};

/// 单步结果 future 的别名（`wb_tasks::scheduler` 内部别名未 `pub use` 出来）。
type StepFut<'a> = Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>>;

// ---------------------------------------------------------------------------
// 测试支架：可控时钟
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

fn at(y: i32, m: u32, d: u32, h: i32, mi: i32, s: i32) -> LocalNow {
    LocalNow::new(y, m, d, h, mi, s)
}

// ---------------------------------------------------------------------------
// 测试支架：记账 executor
// ---------------------------------------------------------------------------

/// 一次调用记录：`(步骤名, uid, 是否成功)`。
type Call = (&'static str, String, bool);

/// 可编排的 fake：按步骤名决定成败，并记录调用顺序。
struct FakeExecutor {
    accounts: Vec<AccountView>,
    /// 这些步骤返回 `ok:false`。
    fail: Mutex<Vec<&'static str>>,
    calls: Arc<Mutex<Vec<Call>>>,
    refresh_ok: bool,
}

impl FakeExecutor {
    fn new(accounts: Vec<AccountView>) -> Self {
        Self {
            accounts,
            fail: Mutex::new(Vec::new()),
            calls: Arc::new(Mutex::new(Vec::new())),
            refresh_ok: true,
        }
    }

    fn with_failing(self, steps: &[&'static str]) -> Self {
        *self.fail.lock().unwrap() = steps.to_vec();
        self
    }

    fn ok_for(&self, step: &'static str) -> bool {
        !self.fail.lock().unwrap().contains(&step)
    }

    fn record(&self, step: &'static str, uid: &str, ok: bool) {
        self.calls.lock().unwrap().push((step, uid.to_string(), ok));
    }

    fn steps(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().iter().map(|c| c.0).collect()
    }
}

impl CycleExecutor for FakeExecutor {
    fn refresh<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let ok = self.refresh_ok;
        self.record("refresh", &acc.uid, ok);
        Box::pin(async move { ok })
    }

    fn checkin<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let ok = self.ok_for("checkin");
        self.record("checkin", &acc.uid, ok);
        Box::pin(async move { json!({"ok": ok, "msg": "签到成功", "error": "签到失败原因"}) })
    }

    fn report_activity<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let ok = self.ok_for("report");
        self.record("report", &acc.uid, ok);
        Box::pin(async move {
            json!({"ok": ok, "msg": "活跃上报成功", "error": "上报失败原因"})
        })
    }

    fn cat_travel<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("travel", &acc.uid, true);
        Box::pin(async move { json!({"action": "depart", "msg": "猫咪已出发"}) })
    }

    fn streak_bonus<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("streak", &acc.uid, true);
        Box::pin(async move { json!({"logs": ["连登 3 天奖励已领取"]}) })
    }

    fn night_growth<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.record("night", &acc.uid, true);
        Box::pin(async move { json!({"logs": ["夜猫任务已完成"]}) })
    }

    fn daily_chat<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let ok = self.ok_for("daily_chat");
        self.record("daily_chat", &acc.uid, ok);
        Box::pin(async move { json!({"ok": ok, "msg": "对话成功", "error": "对话失败原因"}) })
    }

    fn accounts(&self) -> Vec<AccountView> {
        self.accounts.clone()
    }
}

/// 面板日志收集器。
#[derive(Default)]
struct RecordingPanel {
    entries: Mutex<Vec<(String, String)>>,
}

impl PanelSink for RecordingPanel {
    fn add_log_entry(&self, msg: &str, tag: &str) {
        self.entries
            .lock()
            .unwrap()
            .push((msg.to_string(), tag.to_string()));
    }
}

impl RecordingPanel {
    fn entries(&self) -> Vec<(String, String)> {
        self.entries.lock().unwrap().clone()
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
    }
}

fn intl_account(uid: &str) -> AccountView {
    AccountView {
        realm: "intl".to_string(),
        ..cn_account(uid)
    }
}

// ---------------------------------------------------------------------------
// 1. 常量表
// ---------------------------------------------------------------------------

#[test]
fn hour_tables_match_python() {
    assert_eq!(CHECKIN_HOURS, [9, 21]);
    assert_eq!(TRAVEL_HOURS, [9, 21]);
    assert_eq!(KEEPALIVE_HOURS, [22]);
    assert_eq!(CAT_HOURS, [1]);
    assert_eq!(ALL_HOURS, [1, 9, 21, 22]);
}

#[test]
fn timing_constants_match_python() {
    assert_eq!(STARTUP_DELAY_SECS, 10.0);
    assert_eq!(POLL_SECS, 30.0);
    assert_eq!(HIT_SUPPRESS_SECS, 65.0);
    assert_eq!(CN_STEP_PAUSE_SECS, 1.0);
    assert_eq!(INTL_STEP_PAUSE_SECS, 1.5);
    assert_eq!(KEEPALIVE_THRESHOLD_SECS, 7200.0);
    assert_eq!(LOG_RING_CAPACITY, 60);
    assert_eq!(STATUS_LOG_TAIL, 20);
}

// ---------------------------------------------------------------------------
// 2. calc_next_fire / is_hour_hit / 保活 / tag
// ---------------------------------------------------------------------------

#[test]
fn calc_next_fire_covers_all_branches() {
    // 严格大于当前小时
    assert_eq!(
        calc_next_fire(&at(2026, 10, 17, 0, 59, 59)),
        "2026-10-17 01:00:00"
    );
    // 同小时、mm==0、ss<10 的窗口
    assert_eq!(
        calc_next_fire(&at(2026, 10, 17, 1, 0, 0)),
        "2026-10-17 01:00:00"
    );
    // ss==10 已出窗口 ⇒ 下一个整点
    assert_eq!(
        calc_next_fire(&at(2026, 10, 17, 1, 0, 10)),
        "2026-10-17 09:00:00"
    );
    // 最后一档之后 ⇒ 明天 01:00（跨月 / 跨年 / 闰年）。
    // 注意：同小时窗口对 22:00 同样成立，`22:00:00`–`22:00:09` 会命中**当天**
    // 22:00（与 Python `_calc_next_fire` 一致），故须让 ss>=10 才回绕到明天。
    assert_eq!(
        calc_next_fire(&at(2026, 10, 31, 22, 0, 0)),
        "2026-10-31 22:00:00"
    );
    assert_eq!(
        calc_next_fire(&at(2026, 10, 31, 22, 0, 10)),
        "2026-11-01 01:00:00"
    );
    assert_eq!(
        calc_next_fire(&at(2026, 12, 31, 23, 59, 59)),
        "2027-01-01 01:00:00"
    );
    assert_eq!(
        calc_next_fire(&at(2028, 2, 28, 23, 0, 0)),
        "2028-02-29 01:00:00"
    );
}

#[test]
fn hour_hit_requires_enabled_minute_zero_and_known_hour() {
    assert!(is_hour_hit(true, &at(2026, 10, 17, 1, 0, 59)));
    assert!(!is_hour_hit(false, &at(2026, 10, 17, 1, 0, 0)));
    assert!(!is_hour_hit(true, &at(2026, 10, 17, 1, 1, 0)));
    assert!(!is_hour_hit(true, &at(2026, 10, 17, 12, 0, 0)));
}

#[test]
fn keepalive_follows_truthy_expiry() {
    assert!(needs_keepalive(Some(3600.0), 0.0));
    assert!(!needs_keepalive(Some(7200.0), 0.0), "严格小于才刷新");
    assert!(!needs_keepalive(Some(0.0), 0.0), "exp=0 视为无过期信息");
    assert!(!needs_keepalive(None, 0.0));
}

#[test]
fn log_tag_and_uid_prefix() {
    assert_eq!(derive_log_tag("自动签到成功"), "tasks");
    assert_eq!(derive_log_tag("后台定时调度器已启动"), "scheduler");
    assert_eq!(uid8("0123456789"), "01234567");
    assert_eq!(uid8(""), "?");
}

// ---------------------------------------------------------------------------
// 3. 启动 10s + 命中整点 65s 抑制 + 30s 轮询
// ---------------------------------------------------------------------------

// `run_loop` 与 Python 一样**没有自然终止条件**（只在 `stop_event` 置位时退出），
// 直接把 `NoopSleeper` 喂进去必然是紧循环。因此本节按循环体逐句驱动：
// 「10s 启动 → 启动巡检 → (重算 next_fire → 判整点 → 命中则巡检 + 65s) → 30s」，
// 断言对象仍是 `wb_scheduler.py` 里的那段控制流，但执行有界、结果确定。

#[tokio::test]
async fn run_loop_sleeps_10_then_hits_hour_and_suppresses_65() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 0, 30));
    let exec = FakeExecutor::new(vec![cn_account("u1")]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    // 循环体第 0 步：启动先等 10 秒，再跑「启动初次初始化巡检」。
    sleeper.sleep(STARTUP_DELAY_SECS).await;
    assert_eq!(
        sleeper.calls(),
        vec![STARTUP_DELAY_SECS],
        "启动先睡 10 秒（此时尚未巡检）"
    );
    sched.execute_cycle("启动初次初始化巡检").await;
    // 巡检内部：国内四步各 1.0s。
    assert_eq!(
        sleeper.calls(),
        vec![STARTUP_DELAY_SECS, 1.0, 1.0, 1.0, 1.0],
        "启动巡检 = 10s + 四步各 1.0s"
    );

    // 第 1 次迭代：9:00:30 命中 9 点档 ⇒ 巡检 + 65s 抑制 + 30s 轮询。
    sched.recalc_next_fire();
    let now = clock.local_now();
    assert!(is_hour_hit(sched.is_enabled(), &now));
    let reason = format!("整点排程命中 ({}:00)", now.hour);
    let mut before = sleeper.calls().len();
    sched.execute_cycle(&reason).await;
    // 命中档巡检本身的四步休眠紧跟在巡检之后（尚未进入 65s 抑制）。
    let after_cycle = sleeper.calls();
    assert_eq!(
        after_cycle[before..],
        [1.0, 1.0, 1.0, 1.0],
        "命中档巡检同样四步各 1.0s"
    );
    before = after_cycle.len();
    sleeper.sleep(HIT_SUPPRESS_SECS).await;
    sleeper.sleep(POLL_SECS).await;
    assert_eq!(
        sleeper.calls()[before..],
        [HIT_SUPPRESS_SECS, POLL_SECS],
        "命中后紧接 65s 抑制，再 30s 轮询"
    );

    // 第 2 次迭代：时钟不动 ⇒ 同一分钟窗口内再次命中。
    assert!(is_hour_hit(sched.is_enabled(), &clock.local_now()));
    before = sleeper.calls().len();
    sched.execute_cycle(&reason).await;
    sleeper.sleep(HIT_SUPPRESS_SECS).await;
    assert_eq!(
        sleeper.calls()[before..],
        [1.0, 1.0, 1.0, 1.0, HIT_SUPPRESS_SECS],
        "再次命中：四步 + 65s 抑制"
    );

    // 全序列形状：10 起手，其余只允许出现 1.0 / 65 / 30 三种值。
    let sleeps = sleeper.calls();
    assert_eq!(sleeps[0], STARTUP_DELAY_SECS);
    assert!(
        sleeps[1..]
            .iter()
            .all(|&s| s == 1.0 || s == HIT_SUPPRESS_SECS || s == POLL_SECS),
        "意外休眠值：{sleeps:?}"
    );
    assert_eq!(
        sleeps.iter().filter(|&&s| s == HIT_SUPPRESS_SECS).count(),
        2,
        "两次命中 ⇒ 两次 65s"
    );
    assert_eq!(
        sleeps.iter().filter(|&&s| s == POLL_SECS).count(),
        1,
        "第 1 次迭代末尾 30s 轮询"
    );

    // 启动巡检 + 两次整点巡检，每轮国内四步。
    let steps = exec.steps();
    assert_eq!(steps.len(), 12, "三轮 × 四步，实际 {steps:?}");
    let logs = sched.logs().join("\n");
    assert!(logs.contains("整点排程命中 (9:00)"), "实际：{logs}");
    assert!(logs.contains("启动初次初始化巡检"), "实际：{logs}");
}

#[tokio::test]
async fn disabled_scheduler_skips_hour_hit_but_still_runs_startup_cycle() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 0, 30));
    let exec = FakeExecutor::new(vec![cn_account("u1")]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);
    sched.toggle_enabled(); // true → false
    assert!(!sched.is_enabled());

    // 启动巡检不受 enabled 影响（Python 同：`start()` 里无条件跑一次）。
    sleeper.sleep(STARTUP_DELAY_SECS).await;
    sched.execute_cycle("启动初次初始化巡检").await;
    assert!(sched.last_run_time().is_some());

    // 之后的轮询：整点判定被 enabled 挡住，只睡 30s，不出现 65s 抑制。
    for _ in 0..3 {
        sched.recalc_next_fire();
        let hit = is_hour_hit(sched.is_enabled(), &clock.local_now());
        assert!(!hit, "enabled=False 不得命中整点");
        if hit {
            sleeper.sleep(HIT_SUPPRESS_SECS).await;
        }
        sleeper.sleep(POLL_SECS).await;
    }

    let sleeps = sleeper.calls();
    let mut expected = vec![STARTUP_DELAY_SECS];
    expected.extend([1.0, 1.0, 1.0, 1.0]); // 启动巡检四步
    expected.extend([POLL_SECS, POLL_SECS, POLL_SECS]);
    assert_eq!(sleeps, expected, "禁用后只有启动巡检 + 30s 轮询");
    assert!(
        !sleeps.contains(&HIT_SUPPRESS_SECS),
        "禁用后不得出现 65s 抑制：{sleeps:?}"
    );

    // 只跑了启动那一轮（国内四步）。
    assert_eq!(exec.steps(), vec!["checkin", "report", "travel", "streak"]);
    assert!(!sched.logs().iter().any(|l| l.contains("整点排程命中")));
}

/// 只在第一次 `checkin` 里挂住，直到测试放行：用来制造真实的持锁重叠。
struct GateExecutor {
    inner: FakeExecutor,
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl CycleExecutor for GateExecutor {
    fn refresh<'a>(
        &'a self,
        acc: &'a AccountView,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        self.inner.refresh(acc)
    }

    fn checkin<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        let fut = self.inner.checkin(acc);
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        let first = !entered.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            if first {
                // 自旋等测试放行；yield 保证执行器线程仍有进展。
                while !release.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            }
            fut.await
        })
    }

    fn report_activity<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.inner.report_activity(acc)
    }

    fn cat_travel<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.inner.cat_travel(acc)
    }

    fn streak_bonus<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.inner.streak_bonus(acc)
    }

    fn night_growth<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.inner.night_growth(acc)
    }

    fn daily_chat<'a>(&'a self, acc: &'a AccountView) -> StepFut<'a> {
        self.inner.daily_chat(acc)
    }

    fn accounts(&self) -> Vec<AccountView> {
        self.inner.accounts()
    }
}

/// 非阻塞互斥：A 持锁期间 B 必须被挡下并记「跳过本次巡检」。
///
/// `Scheduler<'_>` 借用 executor / sleeper / clock，无法 `tokio::spawn` 成
/// `'static`；改用 `join!` + 第一轮在 `checkin` 内挂住，保证第二轮确实在
/// A 持锁期间尝试抢锁（单线程下 fake 全即时 ready 会让两轮串行，测不到互斥）。
#[tokio::test]
async fn concurrent_cycles_do_not_overlap() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let exec = GateExecutor {
        inner: FakeExecutor::new(vec![cn_account("u1")]),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    };
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    // A：进入 checkin 后（已持锁）自旋等放行；B：在同一时刻尝试抢锁。
    let a_fut = async {
        let counts = sched.execute_cycle("并发A").await;
        // A 醒来后顺手放行自己，避免 B 之后再次进入（B 只跑一次）。
        counts
    };
    let b_fut = async {
        // 先让 A 抢到锁并走到 checkin。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !entered.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "A 未在 5 秒内进入 checkin"
            );
            tokio::task::yield_now().await;
        }
        let b = sched.execute_cycle("并发B").await;
        release.store(true, Ordering::SeqCst); // 放行 A
        b
    };

    let (a, b) = tokio::join!(a_fut, b_fut);

    assert_eq!(b.checkin, 0, "B 抢锁失败，不得执行 checkin");
    assert_eq!(b, CycleCounts::default(), "抢锁失败返回全零计数");
    assert_eq!(a.checkin, 1, "A 正常执行");
    assert_eq!(a.checkin + b.checkin, 1, "非阻塞互斥：只有一轮真正执行");

    let logs = sched.logs().join("\n");
    assert!(
        logs.contains("跳过本次巡检") && logs.contains("上一轮仍在执行"),
        "抢锁失败必须记跳过日志，实际：{logs}"
    );
}

#[tokio::test]
async fn trigger_now_returns_python_receipts() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![cn_account("u1")]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let (body, counts) = sched.trigger_now_blocking().await;
    assert_eq!(body, json!({"ok": true, "msg": "已触发后台调度执行"}));
    assert_eq!(counts.checkin, 1);
    assert_eq!(counts.report, 1);
    assert_eq!(counts.travel, 1);
}

// ---------------------------------------------------------------------------
// 6./7. 账号级跳过
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_account_is_skipped() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let mut off = cn_account("off1");
    off.enabled = false;
    let exec = FakeExecutor::new(vec![off, cn_account("on1")]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    sched.execute_cycle("测试").await;

    let uids: Vec<String> = exec_calls(&exec);
    assert!(
        uids.iter().all(|u| u == "on1"),
        "禁用账号不得被调用：{uids:?}"
    );
}

fn exec_calls(exec: &FakeExecutor) -> Vec<String> {
    exec.calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.1.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// 8. 保活阈值（端到端：只有快过期的才刷新）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn keepalive_runs_only_when_token_expires_within_window() {
    let clock = FakeClock::new(at(2026, 10, 17, 22, 0, 5));
    clock.set_epoch(1_000_000.0);

    let mut soon = cn_account("soon");
    soon.expires_at = Some(1_000_000.0 + 3600.0);
    let mut later = cn_account("later");
    later.expires_at = Some(1_000_000.0 + 10_000.0);
    let mut unknown = cn_account("unknown");
    unknown.expires_at = Some(0.0);

    let exec = FakeExecutor::new(vec![soon, later, unknown]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;

    let refreshed: Vec<String> = exec
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 == "refresh")
        .map(|c| c.1.clone())
        .collect();
    assert_eq!(refreshed, vec!["soon"], "只有剩余寿命不足 2 小时的才刷新");
    assert_eq!(counts.refreshed, 1);
}

// ---------------------------------------------------------------------------
// 9. 国内四步顺序与休眠 / 国际版 1.5s
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cn_account_runs_four_steps_in_order_with_one_second_pauses() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![cn_account("u1")]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;

    assert_eq!(exec.steps(), vec!["checkin", "report", "travel", "streak"]);
    // 9 点不在 cat_hours ⇒ 无夜猫；四步各 1.0s，连登 1 行 ⇒ 共 4 次
    assert_eq!(sleeper.calls(), vec![1.0; 4]);
    assert_eq!(
        counts,
        CycleCounts {
            refreshed: 0,
            checkin: 1,
            report: 1,
            travel: 1,
            daily_chat: 0,
        }
    );
}

#[tokio::test]
async fn night_growth_only_runs_in_cat_hours() {
    for (hour, want_night) in [(1, true), (9, false), (21, false), (22, false)] {
        let clock = FakeClock::new(at(2026, 10, 17, hour, 5, 0));
        let exec = FakeExecutor::new(vec![cn_account("u1")]);
        let sleeper = NoopSleeper::default();
        let sched = Scheduler::new(&exec, &sleeper, &clock);

        sched.execute_cycle("测试").await;
        let steps = exec.steps();
        assert_eq!(
            steps.contains(&"night"),
            want_night,
            "{hour} 点的步骤序列：{steps:?}"
        );

        // 夜猫触发后无条件再睡 1.0s ⇒ 五点共 5 次
        let want_sleeps = if want_night { 5 } else { 4 };
        assert_eq!(sleeper.calls().len(), want_sleeps, "{hour} 点");
    }
}

#[tokio::test]
async fn already_done_today_skips_checkin_and_report() {
    let clock = FakeClock::new(at(2026, 10, 17, 21, 5, 0));
    let mut acc = cn_account("u1");
    acc.last_checkin = Some("2026-10-17 09:00:03".to_string());
    acc.last_activity_report = Some("2026-10-17 09:00:04".to_string());
    let exec = FakeExecutor::new(vec![acc]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;
    // 签到/上报被 can_* 拦下，只剩猫猫与连登
    assert_eq!(exec.steps(), vec!["travel", "streak"]);
    assert_eq!(counts.checkin, 0);
    assert_eq!(counts.report, 0);
    assert_eq!(counts.travel, 1);
}

#[tokio::test]
async fn intl_account_uses_one_and_a_half_second_pause() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![intl_account("i1")]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;

    assert_eq!(exec.steps(), vec!["daily_chat"]);
    assert_eq!(sleeper.calls(), vec![1.5]);
    assert_eq!(counts.daily_chat, 1);
    assert_eq!(counts.checkin, 0, "国际版不走国内四步");
}

#[tokio::test]
async fn intl_account_skipped_when_granted_or_already_chatted_today() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));

    let mut granted = intl_account("i1");
    granted.daily_chat_granted_today = true;
    let mut chatted = intl_account("i2");
    chatted.last_daily_chat = Some("2026-10-17 09:00:01".to_string());

    let exec = FakeExecutor::new(vec![granted, chatted]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;
    assert_eq!(exec.steps(), Vec::<&str>::new());
    assert_eq!(counts.daily_chat, 0);
    assert!(
        sleeper.calls().is_empty(),
        "一步都没跑 ⇒ 不应有任何节流等待"
    );
}

// ---------------------------------------------------------------------------
// 11. 日志：时间戳前缀、环形、尾 20、tag、面板转发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logs_carry_timestamp_prefix_and_forward_to_panel() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 7));
    let exec = FakeExecutor::new(vec![cn_account("u1")]);
    let sleeper = NoopSleeper::default();
    let panel = RecordingPanel::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock).with_panel(&panel);

    sched.execute_cycle("整点排程命中 (9:00)").await;

    let logs = sched.logs();
    assert!(
        logs[0].starts_with("[2026-10-17 09:05:07] 开始执行任务 (整点排程命中 (9:00))..."),
        "实际首条：{}",
        logs[0]
    );

    let entries = panel.entries();
    assert!(!entries.is_empty());
    for (msg, tag) in &entries {
        assert!(msg.starts_with("[调度器] "), "面板消息需带前缀：{msg}");
        assert!(
            tag == "tasks" || tag == "scheduler",
            "tag 只能是 tasks/scheduler：{tag}"
        );
    }
    assert!(
        entries.iter().any(|(_, t)| t == "tasks"),
        "签到类日志应分流到 tasks：{entries:?}"
    );
}

#[test]
fn log_ring_keeps_last_60_and_status_returns_last_20() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 7));
    let exec = FakeExecutor::new(vec![]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    for i in 0..100 {
        sched.log(&format!("m{i}"));
    }
    let logs = sched.logs();
    assert_eq!(logs.len(), LOG_RING_CAPACITY);
    assert!(logs.first().unwrap().ends_with("m40"), "旧条目被挤出");
    assert!(logs.last().unwrap().ends_with("m99"));

    let tail = sched.status()["logs"].as_array().unwrap().len();
    assert_eq!(tail, STATUS_LOG_TAIL);
}

// ---------------------------------------------------------------------------
// 12./13. 汇总行 + 单步失败不中断
// ---------------------------------------------------------------------------

#[test]
fn summary_line_is_verbatim() {
    let c = CycleCounts {
        refreshed: 1,
        checkin: 2,
        report: 3,
        travel: 4,
        daily_chat: 5,
    };
    assert_eq!(
        c.summary_line(),
        "巡检完成：Token保活 1 个，国内签到 2 个，活跃上报 3 个，猫猫日常 4 个，国际活跃 5 个"
    );
}

#[tokio::test]
async fn step_failure_does_not_abort_cycle_or_count_as_success() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![
        cn_account("bad"),
        cn_account("good"),
        intl_account("intl_bad"),
    ])
    .with_failing(&["checkin", "report", "daily_chat"]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;

    // 每一步都跑到了（失败不中断）
    assert_eq!(
        exec.steps(),
        vec![
            "checkin",
            "report",
            "travel",
            "streak",
            "checkin",
            "report",
            "travel",
            "streak",
            "daily_chat"
        ]
    );
    // 但成功计数为 0
    assert_eq!(counts.checkin, 0);
    assert_eq!(counts.report, 0);
    assert_eq!(counts.daily_chat, 0);
    assert_eq!(counts.travel, 2, "猫猫不受签到失败影响");

    let logs = sched.logs().join("\n");
    assert!(
        logs.contains("自动签到未成功: 签到失败原因"),
        "失败日志带 error"
    );
    assert!(logs.contains("对话活跃上报失败: 上报失败原因"));
    assert!(logs.contains("每日活跃对话失败: 对话失败原因"));
    assert!(logs.contains("巡检完成"), "整轮仍要收尾");
}

#[tokio::test]
async fn empty_pool_logs_skip_message() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let counts = sched.execute_cycle("测试").await;
    assert_eq!(counts, CycleCounts::default());
    assert!(sched
        .logs()
        .iter()
        .any(|l| l.contains("暂无可用的活跃账号，跳过本次巡检")));
}

// ---------------------------------------------------------------------------
// 14. status() 七键
// ---------------------------------------------------------------------------

#[test]
fn status_exposes_seven_keys_in_python_order() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let st = sched.status();
    let keys: Vec<&str> = st.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "enabled",
            "mode",
            "mode_cn",
            "mode_intl",
            "last_run_time",
            "next_run_time",
            "logs"
        ]
    );
    assert_eq!(st["enabled"], true);
    assert_eq!(st["mode"], MODE_TEXT);
    assert_eq!(st["mode_cn"], MODE_TEXT);
    assert_eq!(st["mode_intl"], MODE_INTL_TEXT);
    assert_eq!(st["last_run_time"], LAST_RUN_NEVER);
    assert_eq!(st["next_run_time"], "2026-10-17 21:00:00", "构造时已算好");
    assert!(st["logs"].as_array().unwrap().is_empty());
}

#[test]
fn toggle_enabled_flips_and_logs() {
    let clock = FakeClock::new(at(2026, 10, 17, 9, 5, 0));
    let exec = FakeExecutor::new(vec![]);
    let sleeper = NoopSleeper::default();
    let sched = Scheduler::new(&exec, &sleeper, &clock);

    let st = sched.toggle_enabled();
    assert_eq!(st["enabled"], false);
    assert!(sched
        .logs()
        .iter()
        .any(|l| l.contains("用户切换调度器状态为: 暂停")));

    let st = sched.toggle_enabled();
    assert_eq!(st["enabled"], true);
    assert!(sched
        .logs()
        .iter()
        .any(|l| l.contains("用户切换调度器状态为: 启用")));
}

#[test]
fn placeholder_texts_match_python() {
    assert_eq!(NEXT_RUN_PENDING, "待调度");
    assert_eq!(LAST_RUN_NEVER, "尚未运行");
}
