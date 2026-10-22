//! 关卡 4.19（线 A）产出物：契约 —— `PriceRefresher` 后台线程。
//!
//! 权威源：Python `wb_pricing.py` 的 `PriceRefresher`（`:2306-2554`）。
//!
//! 断言清单：
//! 1. 总开关关闭时 `run_once` 返回 `None`，**一个请求都不发**，**不写文件**。
//! 2. 间隔为 0 时不刷（返回 `None`，不写文件）。
//! 3. 首次 `run_once` **立刻**抓一次（fetch 次数 == 1），并写下两个文件。
//! 4. 抓取失败时**不写新文件**、**记 `last_error`**、**保留旧快照**（文件内容
//!    一字不变）。
//! 5. 抓取失败且**盘上没有上一版可保留**时 `run_once` 返回 `None`。
//! 6. `status()` 的键名齐全（`interval_minutes` / `enabled` / `master_enabled`
//!    / `last_run` / `last_error` / `logs` 等）。
//! 7. 落盘落在 `pricing-policies.jsonl` 与 `pricing-timeline.jsonl` 两个文件。
//! 8. 后台循环：**没有任何价格历史时启动后立刻抓一次**（不等一个间隔）；
//!    已有价格历史时**不重复抓**。
//! 9. 总开关关着时后台循环 **park 不计时**（不抓、不写）；开关切回开后
//!    **立刻抓一次**。
//!
//! 全程注入 fake `PriceSource`，**不发任何真实上游请求**（M5 计划硬约束）。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use wb_tasks::live_catalog::LiveCollect;
use wb_tasks::price_refresher::{
    PriceCatalog, PriceRefresher, PriceSource, POLICIES_FILE, TIMELINE_FILE,
};

// ---------------------------------------------------------------------------
// 测试支架：fake 数据源（不触网）
// ---------------------------------------------------------------------------

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 记录抓取次数与落盘结果的 fake 数据源。
struct FakeSource {
    /// 数据目录（策略表与时间线落在这里）。
    usage_dir: PathBuf,
    /// 可控时钟（epoch 秒）。
    now: Mutex<f64>,
    /// 抓取次数（断言「不刷 / 只刷一次」用）。
    fetch_count: AtomicUsize,
    /// 让下一次抓取失败（`Some(错误文案)`）。
    fail: Mutex<Option<String>>,
    /// 总开关。**缺键 = 开** ⇒ 默认 `true`。
    master_enabled: AtomicBool,
    /// 取价输入清单。
    catalog: Mutex<PriceCatalog>,
    /// live 目录新增的 id（Python `live_ids()` 的返回）。
    live: Mutex<Vec<String>>,
    /// live 目录各 realm 的失败原因。
    live_errors: Mutex<Vec<(String, String)>>,
    /// 上游原文（OpenRouter 的 `/api/v1/models` 响应）。
    raw: Mutex<Value>,
}

impl FakeSource {
    fn new(usage_dir: PathBuf) -> Arc<FakeSource> {
        Arc::new(FakeSource {
            usage_dir,
            now: Mutex::new(1_800_000_000.0),
            fetch_count: AtomicUsize::new(0),
            fail: Mutex::new(None),
            master_enabled: AtomicBool::new(true),
            catalog: Mutex::new(PriceCatalog {
                hub_ids: vec!["gpt-4o".to_string()],
                runtime_overrides: Default::default(),
                variants: true,
            }),
            live: Mutex::new(Vec::new()),
            live_errors: Mutex::new(Vec::new()),
            raw: Mutex::new(json!({
                "data": [{
                    "id": "openai/gpt-4o",
                    "pricing": {
                        "prompt": "0.0000025",
                        "completion": "0.00001",
                        "input_cache_read": "0.00000125",
                    },
                }],
            })),
        })
    }

    fn fetch_count(&self) -> usize {
        self.fetch_count.load(Ordering::SeqCst)
    }

    fn set_fail(&self, message: Option<&str>) {
        *self.fail.lock().unwrap() = message.map(|m| m.to_string());
    }

    fn set_master_enabled(&self, enabled: bool) {
        self.master_enabled.store(enabled, Ordering::SeqCst);
    }
}

impl PriceSource for FakeSource {
    fn fetch_models(&self) -> BoxFut<'_, Result<Value, String>> {
        Box::pin(async move {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            match self.fail.lock().unwrap().clone() {
                Some(message) => Err(message),
                None => Ok(self.raw.lock().unwrap().clone()),
            }
        })
    }

    fn now(&self) -> f64 {
        *self.now.lock().unwrap()
    }

    fn usage_dir(&self) -> &Path {
        &self.usage_dir
    }

    fn master_enabled(&self) -> bool {
        self.master_enabled.load(Ordering::SeqCst)
    }

    fn catalog(&self) -> PriceCatalog {
        self.catalog.lock().unwrap().clone()
    }

    /// live 目录新增的 id（Python `live_ids()`）。
    fn live_ids(&self) -> BoxFut<'_, LiveCollect> {
        let ids = self.live.lock().unwrap().clone();
        let errors = self.live_errors.lock().unwrap().clone();
        Box::pin(async move { LiveCollect { ids, errors } })
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("JSONL 的每一行都要是合法 JSON"))
        .collect()
}

fn policies_path(dir: &Path) -> PathBuf {
    dir.join(POLICIES_FILE)
}

fn timeline_path(dir: &Path) -> PathBuf {
    dir.join(TIMELINE_FILE)
}

// ---------------------------------------------------------------------------
// 1. 总开关关着 ⇒ 不跑、不写
// ---------------------------------------------------------------------------

#[tokio::test]
async fn master_switch_off_means_no_fetch_and_no_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());
    source.set_master_enabled(false);

    let refresher = PriceRefresher::new(source.clone(), 300.0);
    let result = refresher.run_once().await;

    assert_eq!(result, None, "总开关关着 ⇒ 本轮没有产出");
    assert_eq!(source.fetch_count(), 0, "总开关关着 ⇒ 一个请求都不发");
    assert!(
        !policies_path(tmp.path()).exists(),
        "总开关关着 ⇒ 不写策略表"
    );
    assert!(
        !timeline_path(tmp.path()).exists(),
        "总开关关着 ⇒ 不写时间线"
    );
    assert_eq!(
        refresher.status()["master_enabled"],
        json!(false),
        "status 要单独回报总开关"
    );
    assert_eq!(refresher.status()["enabled"], json!(false));
}

// ---------------------------------------------------------------------------
// 2. 间隔为 0 ⇒ 不刷
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zero_interval_means_no_refresh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    let refresher = PriceRefresher::new(source.clone(), 0.0);
    let result = refresher.run_once().await;

    assert_eq!(result, None, "间隔为 0 ⇒ 本轮没有产出");
    assert_eq!(source.fetch_count(), 0, "间隔为 0 ⇒ 一个请求都不发");
    assert!(!policies_path(tmp.path()).exists());
    assert!(!timeline_path(tmp.path()).exists());
    assert_eq!(refresher.status()["enabled"], json!(false));
}

// ---------------------------------------------------------------------------
// 3. 首次 run_once 立刻抓一次 + 写两个文件
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_run_fetches_immediately_and_writes_both_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    let refresher = PriceRefresher::new(source.clone(), 300.0);
    let result = refresher.run_once().await;

    assert_eq!(result, Some(String::new()), "成功 ⇒ 空文案");
    assert_eq!(source.fetch_count(), 1, "首次 run_once 立刻抓一次");
    assert_eq!(refresher.status()["last_error"], Value::Null);

    // 策略表：一行 = 整份快照
    let policies = read_lines(&policies_path(tmp.path()));
    assert_eq!(policies.len(), 1, "策略表追加一行快照");
    assert_eq!(
        policies[0]["models"]["gpt-4o"]["or_id"],
        json!("openai/gpt-4o"),
        "策略表里是塑形后的价格条目"
    );

    // 时间线：一行 = 一条时间戳条目
    let timeline = read_lines(&timeline_path(tmp.path()));
    assert_eq!(timeline.len(), 1, "时间线追加一行时间戳条目");
    assert_eq!(timeline[0]["at"], json!(1_800_000_000.0));
    assert_eq!(timeline[0]["models"], json!(1));

    assert_eq!(refresher.status()["last_run"], json!(1_800_000_000.0));
}

// ---------------------------------------------------------------------------
// 4. 抓取失败 ⇒ 不写盘、记 last_error、保留旧快照
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_fetch_keeps_the_old_snapshot_and_records_the_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    let refresher = PriceRefresher::new(source.clone(), 300.0);
    refresher.run_once().await.expect("第一次应当成功");

    let policies_before = std::fs::read_to_string(policies_path(tmp.path())).unwrap();
    let timeline_before = std::fs::read_to_string(timeline_path(tmp.path())).unwrap();

    source.set_fail(Some("OpenRouter 返回 503"));
    let result = refresher.run_once().await;

    assert_eq!(
        result,
        Some("OpenRouter 返回 503".to_string()),
        "抓取失败 ⇒ 报告错误文案"
    );
    assert_eq!(source.fetch_count(), 2);
    assert_eq!(
        std::fs::read_to_string(policies_path(tmp.path())).unwrap(),
        policies_before,
        "抓取失败 ⇒ 策略表一字不改"
    );
    assert_eq!(
        std::fs::read_to_string(timeline_path(tmp.path())).unwrap(),
        timeline_before,
        "抓取失败 ⇒ 时间线一字不改"
    );
    assert_eq!(
        refresher.status()["last_error"],
        json!("OpenRouter 返回 503"),
        "抓取失败 ⇒ 记 last_error"
    );
    assert_eq!(
        refresher.status()["last_run"],
        json!(1_800_000_000.0),
        "抓取失败 ⇒ 不更新 last_run（还是上一次成功的那次）"
    );
}

// ---------------------------------------------------------------------------
// 5. 抓取失败且没有上一版可保留 ⇒ None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_fetch_without_a_previous_snapshot_returns_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());
    source.set_fail(Some("network error"));

    let refresher = PriceRefresher::new(source.clone(), 300.0);
    let result = refresher.run_once().await;

    assert_eq!(result, None, "没有上一版可保留 ⇒ 本轮没有产出");
    assert_eq!(source.fetch_count(), 1);
    assert_eq!(refresher.status()["last_error"], json!("network error"));
    assert!(!policies_path(tmp.path()).exists(), "失败 ⇒ 不写盘");
}

// ---------------------------------------------------------------------------
// 6. status() 键名齐全
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_exposes_the_expected_keys() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());
    let refresher = PriceRefresher::new(source.clone(), 300.0);

    let status = refresher.status();
    for key in [
        "interval_minutes",
        "enabled",
        "master_enabled",
        "running",
        "last_run",
        "last_error",
        "logs",
        "policies_file",
        "timeline",
    ] {
        assert!(status.get(key).is_some(), "status 缺键: {key}");
    }

    assert_eq!(status["interval_minutes"], json!(5.0), "300 秒 = 5 分钟");
    assert_eq!(status["enabled"], json!(true));
    assert_eq!(status["master_enabled"], json!(true));
    assert_eq!(status["last_run"], Value::Null);
    assert_eq!(status["last_error"], Value::Null);
    assert!(status["logs"].is_array());
}

// ---------------------------------------------------------------------------
// 6.5 live 目录：新增 id 进取价清单；取不到不缩小覆盖（关卡 4.21）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_ids_join_the_price_input_list() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    // 上游认识 gpt-4o，也认识那个只有 live 目录里才有的模型。
    *source.raw.lock().unwrap() = json!({
        "data": [
            {"id": "openai/gpt-4o", "pricing": {"prompt": "0.0000025"}},
            {"id": "vendor/brand-new", "pricing": {"prompt": "0.000001"}},
        ]
    });
    *source.live.lock().unwrap() = vec!["brand-new".to_string()];

    let refresher = PriceRefresher::new(source.clone(), 300.0);
    let result = refresher.run_once().await;
    assert_eq!(result, Some(String::new()), "成功 ⇒ 空文案");

    let policies = read_lines(&policies_path(tmp.path()));
    assert_eq!(policies.len(), 1, "策略表追加一行快照");
    let models = policies[0]["models"].as_object().expect("models 是对象");
    assert!(
        models.contains_key("brand-new"),
        "live 目录的新模型必须进取价清单：{models:?}"
    );
    assert_eq!(
        models["brand-new"]["or_id"],
        json!("vendor/brand-new"),
        "live 模型也要拿到价格条目"
    );

    assert_eq!(
        refresher.status()["last_extra"],
        json!(1),
        "last_extra 报告 live 新增了几个"
    );
}

#[tokio::test]
async fn a_failing_live_catalog_does_not_shrink_the_coverage() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    // 两个 realm 都取不到 ⇒ live 为空，但静态目录的 gpt-4o 照样取到价。
    *source.live_errors.lock().unwrap() = vec![
        ("intl".to_string(), "没有可用的 intl 账号".to_string()),
        ("cn".to_string(), "没有可用的 cn 账号".to_string()),
    ];

    let refresher = PriceRefresher::new(source.clone(), 300.0);
    let result = refresher.run_once().await;
    assert_eq!(result, Some(String::new()), "live 失败不该让整轮失败");

    let policies = read_lines(&policies_path(tmp.path()));
    let models = policies[0]["models"].as_object().expect("models 是对象");
    assert!(
        models.contains_key("gpt-4o"),
        "live 挂掉时静态目录必须照样取价：{models:?}"
    );
    assert_eq!(refresher.status()["last_extra"], json!(0));
}

// ---------------------------------------------------------------------------
// 7. 后台循环：没有价格历史 ⇒ 启动后立刻抓一次
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_loop_fetches_immediately_when_there_is_no_price_history() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    // 间隔给一小时：立刻抓到就说明**没有先睡一个 tick**。
    let refresher = Arc::new(PriceRefresher::new(source.clone(), 3600.0));
    refresher.start();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    refresher.shutdown_and_wait().await;

    assert_eq!(source.fetch_count(), 1, "启动后立刻抓一次");
    assert!(policies_path(tmp.path()).exists(), "立刻抓 ⇒ 立刻落盘");
    assert!(timeline_path(tmp.path()).exists());
}

// ---------------------------------------------------------------------------
// 8. 后台循环：已有价格历史 ⇒ 不重复抓
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_loop_does_not_refetch_when_a_price_history_exists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());

    // 先造一条时间线（= 已经取过价）。
    std::fs::write(
        timeline_path(tmp.path()),
        "{\"at\": 1800000000.0, \"generated_at\": \"2026-01-01T00:00:00\", \"models\": 1}\n",
    )
    .expect("写时间线");

    let refresher = Arc::new(PriceRefresher::new(source.clone(), 3600.0));
    refresher.start();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    refresher.shutdown_and_wait().await;

    assert_eq!(
        source.fetch_count(),
        0,
        "已经取过价的正常重启不重复抓（Python:2465-2508）"
    );
}

// ---------------------------------------------------------------------------
// 9. 总开关关着 ⇒ park 不计时；切回开 ⇒ 立刻抓一次
// ---------------------------------------------------------------------------

#[tokio::test]
async fn master_switch_off_parks_the_loop_and_switching_on_fetches_at_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = FakeSource::new(tmp.path().to_path_buf());
    source.set_master_enabled(false);

    let refresher = Arc::new(PriceRefresher::new(source.clone(), 3600.0));
    refresher.start();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    assert_eq!(source.fetch_count(), 0, "总开关关着 ⇒ 不取价");
    assert!(!policies_path(tmp.path()).exists(), "总开关关着 ⇒ 不写盘");
    assert_eq!(
        refresher.status()["next_run"],
        Value::Null,
        "park 中不计时 ⇒ 没有下次刷新时刻"
    );

    // 开关切回开 ⇒ 循环被唤醒后**立刻**抓一次（Python:2465-2508 的
    // `came_from_off` 分支）。
    source.set_master_enabled(true);
    refresher.wake();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    refresher.shutdown_and_wait().await;

    assert_eq!(source.fetch_count(), 1, "开关切回开 ⇒ 立刻抓一次");
    assert!(policies_path(tmp.path()).exists());
}
