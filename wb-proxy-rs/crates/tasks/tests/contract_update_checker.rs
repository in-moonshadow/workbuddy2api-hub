//! 关卡 4.19（线 B）：`UpdateChecker` 契约测试 —— **不发任何真实上游请求**。
//!
//! 权威源：Python `tests/_test_update_check.py`（同名的用例逐条对应）。
//!
//! 覆盖：
//! - 开关缺省（settings 里没有 `update_check_enabled`）⇒ **一个请求都不发**；
//! - 开关打开后，后台线程自己跑一次（首次延迟到点即触发）；
//! - 24 小时窗口：同一天内重复触发不再发请求；
//! - `check_manual` 绕过开关与窗口，真的去问一次；
//! - 抓取失败记 `last_error`、**不**写 `last_success`；
//! - 成功后写 settings 的 `update_check` 键（只含三个字段）；
//! - `status()` 的十个键与 Python 逐字一致；
//! - 网关那一半（API → 回退 → 判版本）走 wiremock，不碰真实上游。
//!
//! 所有请求都打到 wiremock 的本地 mock server 或一个纯内存的假数据源。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use wb_gateway::outbound::UpstreamClient;
use wb_tasks::update_checker::{UpdateChecker, UpdateSource};

// ---------------------------------------------------------------------------
// 假数据源（纯内存，绝不触网）
// ---------------------------------------------------------------------------

/// 一条 releases 条目（形状照 Python 测试里的 `release()`）。
fn release(tag: &str) -> Value {
    json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": false,
        "html_url": format!("https://github.com/ardeyouxipianyi/workbuddy2api-hub/releases/tag/{tag}"),
        "published_at": "2026-10-08T00:00:00Z",
    })
}

/// 一份 releases 文档（Python 测试里的 `CURRENT = "1.6.17"` 那组）。
fn releases_document() -> Value {
    json!([release("v1.6.18"), release("v1.6.17")])
}

/// 假数据源：数请求、给固定时钟、记住写下的记账。
struct FakeSource {
    enabled: AtomicBool,
    calls: AtomicUsize,
    now: Mutex<f64>,
    outcome: Mutex<Result<Value, String>>,
    saved: Mutex<Vec<Value>>,
}

impl FakeSource {
    fn new(enabled: bool, outcome: Result<Value, String>) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            calls: AtomicUsize::new(0),
            now: Mutex::new(1_800_000_000.0),
            outcome: Mutex::new(outcome),
            saved: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn saved(&self) -> Vec<Value> {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl UpdateSource for FakeSource {
    fn fetch_releases(&self) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
    }

    fn now(&self) -> f64 {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn load_state(&self) -> Value {
        json!({})
    }

    fn save_state(&self, state: &Value) -> Result<(), String> {
        self.saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(state.clone());
        Ok(())
    }
}

/// 数据源里的 fetch 走真实的网关那一半（端点指向 wiremock）。
struct WireSource {
    client: UpstreamClient,
    api_url: String,
    latest_url: String,
    enabled: AtomicBool,
    calls: AtomicUsize,
}

impl WireSource {
    fn new(api_url: String, latest_url: String) -> Arc<Self> {
        Arc::new(Self {
            client: UpstreamClient::new(),
            api_url,
            latest_url,
            enabled: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl UpdateSource for WireSource {
    fn fetch_releases(&self) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            wb_gateway::github::fetch_releases_at(
                &self.client,
                &self.api_url,
                &self.latest_url,
                None,
            )
            .await
        })
    }

    fn now(&self) -> f64 {
        1_800_000_000.0
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn load_state(&self) -> Value {
        json!({})
    }

    fn save_state(&self, state: &Value) -> Result<(), String> {
        let _ = state;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 开关
// ---------------------------------------------------------------------------

/// 开关缺省 ⇒ 一个请求都不发（Python `test_disabled_mode_sends_no_request`）。
#[tokio::test]
async fn a_missing_switch_means_no_request_at_all() {
    let source = FakeSource::new(false, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(source.calls(), 0, "开关关着：一个请求都不能发");
    assert_eq!(status["skipped"], "disabled");
    assert_eq!(status["enabled"], false);
    assert!(status["latest_version"].is_null());
    assert_eq!(status["update_available"], false);
}

/// 开关打开 ⇒ 检查真的跑了，并报出更高的版本。
#[tokio::test]
async fn the_switch_on_lets_the_check_through() {
    let source = FakeSource::new(true, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(source.calls(), 1);
    assert_eq!(status["latest_version"], "1.6.18");
    assert_eq!(status["update_available"], true);
    assert_eq!(status["last_error"], "");
}

// ---------------------------------------------------------------------------
// 24 小时窗口
// ---------------------------------------------------------------------------

/// 同一天内重复触发不再发请求（Python `test_enabled_mode_respects_the_24h_gate`）。
#[tokio::test]
async fn the_24_hour_window_blocks_a_second_pass() {
    let source = FakeSource::new(true, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    let first = checker.check().await;
    assert_eq!(source.calls(), 1);
    assert_eq!(first["latest_version"], "1.6.18");

    let second = checker.check().await;
    assert_eq!(source.calls(), 1, "窗口没过 ⇒ 不能再发请求");
    assert_eq!(second["skipped"], "not due yet");
}

/// 手动检查绕过开关与窗口（Python `test_manual_check_ignores_the_switch_and_the_window`）。
#[tokio::test]
async fn the_manual_check_bypasses_the_switch_and_the_window() {
    let source = FakeSource::new(false, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    // 关着：后台路径一个请求都不发。
    let _ = checker.check().await;
    assert_eq!(source.calls(), 0);

    // 操作员点了「立即检查」⇒ 真的去问一次。
    let status = checker.check_manual().await;
    assert_eq!(source.calls(), 1);
    assert_eq!(status["latest_version"], "1.6.18");
    assert_eq!(status["update_available"], true);

    // 手动也绕过 24h 窗口。
    let _ = checker.check_manual().await;
    assert_eq!(source.calls(), 2);
}

// ---------------------------------------------------------------------------
// 失败
// ---------------------------------------------------------------------------

/// 抓取失败记 `last_error`，**不**写 `last_success`（Python
/// `test_a_failed_attempt_is_not_recorded_as_a_success`）。
#[tokio::test]
async fn a_failed_check_records_the_error_and_no_success() {
    let source = FakeSource::new(true, Err("network error".to_string()));
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(status["last_error"], "network error");
    assert!(
        status["last_success"].is_null(),
        "没成功过 ⇒ last_success 是空的"
    );
    assert!(status["latest_version"].is_null());
    assert_eq!(status["update_available"], false);

    let saved = source.saved();
    assert_eq!(saved.len(), 1, "失败也要记账（last_attempt）");
    assert_eq!(saved[0]["last_success"], 0.0);
    assert!(
        saved[0]["last_attempt"].as_f64().unwrap_or(0.0) > 0.0,
        "last_attempt 必须落盘"
    );
}

/// 失败保留上一次的结论（Python `test_a_failed_check_keeps_the_last_known_answer`）。
#[tokio::test]
async fn a_failed_check_keeps_the_last_known_answer() {
    let source = FakeSource::new(true, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());
    let _ = checker.check().await;

    // 换成一次失败（内存里的 outcome 改掉）。
    *source.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Err("HTTP 403".to_string());
    // 把窗口打开（Python 测试里改的是 `checker_._last_attempt`）。
    {
        let mut now = source.now.lock().unwrap_or_else(|e| e.into_inner());
        *now += 25.0 * 3600.0;
    }

    let status = checker.check().await;

    assert_eq!(status["last_error"], "HTTP 403");
    assert_eq!(
        status["latest_version"], "1.6.18",
        "失败不能抹掉上一次的结论"
    );
    assert_eq!(status["update_available"], true);
}

/// 非数组的文档 ⇒ `unexpected release document`（Python
/// `test_an_unexpected_document_is_non_fatal`）。
#[tokio::test]
async fn an_unexpected_document_is_not_fatal() {
    let source = FakeSource::new(true, Ok(json!({"message": "Not Found"})));
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(status["last_error"], "unexpected release document");
    assert!(status["latest_version"].is_null());
}

// ---------------------------------------------------------------------------
// 记账
// ---------------------------------------------------------------------------

/// 成功后写 settings 的 `update_check` 键，且**只含三个字段**
/// （Python `test_only_operational_fields_are_persisted`）。
#[tokio::test]
async fn a_successful_check_writes_three_fields_to_the_update_check_key() {
    let source = FakeSource::new(true, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    let _ = checker.check().await;

    let saved = source.saved();
    assert_eq!(saved.len(), 1);
    let object = saved[0].as_object().expect("记账是一个对象");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["last_attempt", "last_success", "latest_version"],
        "只写这三个字段"
    );
    assert_eq!(saved[0]["latest_version"], "1.6.18");
    assert!(saved[0]["last_attempt"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(saved[0]["last_success"].as_f64().unwrap_or(0.0) > 0.0);
}

// ---------------------------------------------------------------------------
// 状态形状
// ---------------------------------------------------------------------------

/// `status()` 的十个键与 Python 逐字一致（Python `status()`，`:257-271`）。
#[tokio::test]
async fn the_status_carries_the_python_keys() {
    let source = FakeSource::new(true, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    let status = checker.status();
    let keys: Vec<&str> = status
        .as_object()
        .expect("status 是一个对象")
        .keys()
        .map(String::as_str)
        .collect();

    assert_eq!(
        keys,
        vec![
            "current_version",
            "latest_version",
            "update_available",
            "enabled",
            "checking",
            "last_attempt",
            "last_success",
            "last_error",
            "release_url",
            "published_at",
        ]
    );
    assert_eq!(status["checking"], false);
    assert_eq!(status["last_error"], "");
    assert!(
        status["last_attempt"].is_null(),
        "没查过 ⇒ last_attempt 是空的"
    );
}

/// 查过一次之后，状态里带上版本、发布页与发布时间。
#[tokio::test]
async fn the_status_reports_what_the_last_check_found() {
    let source = FakeSource::new(true, Ok(releases_document()));
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(status["latest_version"], "1.6.18");
    assert_eq!(
        status["release_url"],
        "https://github.com/ardeyouxipianyi/workbuddy2api-hub/releases/tag/v1.6.18"
    );
    assert_eq!(status["published_at"], "2026-10-08T00:00:00Z");
    assert!(
        status["last_attempt"].is_string(),
        "面板显示的是本地时间文本"
    );
}

// ---------------------------------------------------------------------------
// 后台线程
// ---------------------------------------------------------------------------

/// 后台线程自己跑一次（Python `test_an_enabled_worker_checks_on_its_own`）。
#[tokio::test]
async fn the_worker_checks_on_its_own_once_the_switch_is_on() {
    let source = FakeSource::new(true, Ok(releases_document()));
    // 首次延迟压到 10ms（生产是 30s）；轮询间隔 60s ⇒ 一轮里只会查一次
    // （24h 窗口也会挡掉后续）。
    let checker = Arc::new(UpdateChecker::with_timings(source.clone(), 0.01, 60.0));

    checker.start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while source.calls() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    checker.shutdown_and_wait().await;

    assert_eq!(source.calls(), 1, "到点后检查了一次");
}

/// 开关关着时，后台线程**一个请求都不发**（Python
/// `test_a_disabled_worker_never_fetches`）。
#[tokio::test]
async fn a_disabled_worker_never_fetches() {
    let source = FakeSource::new(false, Ok(releases_document()));
    let checker = Arc::new(UpdateChecker::with_timings(source.clone(), 0.01, 60.0));

    checker.start();
    tokio::time::sleep(Duration::from_millis(200)).await;
    checker.shutdown_and_wait().await;

    assert_eq!(source.calls(), 0);
}

// ---------------------------------------------------------------------------
// 网关那一半（wiremock，不碰真实上游）
// ---------------------------------------------------------------------------

/// 端点常量在两个 crate 里各存一份（依赖方向：tasks → gateway）⇒ 用一条
/// 断言盯住它们不许漂。
#[test]
fn the_gateway_and_the_judgement_layer_agree_on_the_endpoints() {
    assert_eq!(
        wb_gateway::github::RELEASES_URL,
        wb_tasks::update::RELEASES_URL
    );
    assert_eq!(
        wb_gateway::github::LATEST_RELEASE_URL,
        wb_tasks::update::LATEST_RELEASE_URL
    );
    assert_eq!(wb_gateway::github::TOKEN_ENV, wb_tasks::update::TOKEN_ENV);
    assert_eq!(
        wb_gateway::github::REQUEST_TIMEOUT,
        wb_tasks::update::REQUEST_TIMEOUT
    );
    assert_eq!(
        wb_tasks::update::FIRST_DELAY_SECONDS,
        30.0,
        "Python wb_updates.py:50"
    );
    assert_eq!(
        wb_tasks::update::POLL_SECONDS,
        300.0,
        "Python wb_updates.py:52"
    );
    assert_eq!(
        wb_tasks::update::SCHEDULED_INTERVAL_SECONDS,
        24.0 * 3600.0,
        "Python wb_updates.py:48"
    );
}

/// 整条链路（API → 判版本）跑在 mock 上：mock 出一份 releases 文档，
/// 检查器报出更高的版本。
#[tokio::test]
async fn the_checker_reports_a_newer_release_found_over_the_wire() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(releases_document()))
        .mount(&server)
        .await;

    let source = WireSource::new(
        format!("{}/releases", server.uri()),
        format!("{}/latest", server.uri()),
    );
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(source.calls(), 1);
    assert_eq!(status["latest_version"], "1.6.18");
    assert_eq!(status["update_available"], true);
    assert_eq!(status["last_error"], "");
}

/// API 配额被同出口的邻居耗光（403）⇒ 自动退回网页端点，结论照样拿回来
/// （Python `test_an_api_quota_error_falls_back_to_the_web_endpoint`）。
#[tokio::test]
async fn a_quota_error_falls_back_to_the_web_endpoint_end_to_end() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/latest"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/releases/tag/v1.6.20"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/releases/tag/v1.6.20"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html/>"))
        .mount(&server)
        .await;

    let source = WireSource::new(
        format!("{}/releases", server.uri()),
        format!("{}/latest", server.uri()),
    );
    let checker = UpdateChecker::new(source.clone());

    let status = checker.check().await;

    assert_eq!(status["last_error"], "");
    assert_eq!(status["latest_version"], "1.6.20");
    assert_eq!(status["update_available"], true);
}
