//! 关卡 4.20（线 B）产出物：用量 / 活动新路由的契约测试。
//!
//! 覆盖五条新路由：
//! - `GET /v1/usage`（与 `/usage` 同一份实现 ⇒ 同一份响应体）
//! - `GET /usage/recent`
//! - `GET /usage/timeseries`
//! - `GET /usage/perf`
//! - `GET /activity/history`
//!
//! 全程 tempdir + wiremock 假上游，**不发任何真实网络请求**；用量 / 活动
//! 两条路由只读本地文件。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_tasks::api_routes::{usage_dir_from, TaskRuntime};
use wb_tasks::checkin::{checkin, CheckinCtx, CHECKIN_PATH};
use wb_tasks::growth_api::{TaskAccount, TaskEndpoints};
use wb_tasks::growth_runner::{Clock, RunnerAccount};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "test-key";

/// 五条新路由。
const NEW_ROUTES: [&str; 5] = [
    "/v1/usage",
    "/usage/recent",
    "/usage/timeseries",
    "/usage/perf",
    "/activity/history",
];

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().join("accounts"),
        dashboard: dir.path().join("dashboard.html"),
        api_key: Some(API_KEY.to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "cn".to_string(),
    })
}

fn make_task(cfg: &Arc<Config>) -> Arc<TaskRuntime> {
    TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    )
}

fn make_app(cfg: &Arc<Config>) -> Router {
    app_with(
        cfg.clone(),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        make_task(cfg),
    )
}

fn usage_dir(cfg: &Config) -> PathBuf {
    usage_dir_from(&cfg.accounts_dir)
}

/// 发一次 GET，返回 (状态码, 解析后的 JSON)。
async fn get(app: &Router, path: &str, with_key: bool) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(path);
    if with_key {
        builder = builder.header("authorization", format!("Bearer {API_KEY}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// 一条成功用量行（`at` 可指定，便于窗口测试）。
fn usage_row(at: f64, model: &str, total_tokens: i64) -> Value {
    json!({
        "at": at,
        "iso": "2026-10-21 10:00:00",
        "model": model,
        "stream": false,
        "outcome": "completed",
        "elapsed_ms": 120,
        "prompt_tokens": total_tokens / 2,
        "completion_tokens": total_tokens - total_tokens / 2,
        "total_tokens": total_tokens,
        "account": "acc-1",
        "realm": "cn",
        "key": "",
    })
}

/// 一条带延迟样本的用量行（perf 用）。
fn perf_row(at: f64, model: &str, ttft: f64, gen: f64, wall: f64) -> Value {
    json!({
        "at": at,
        "iso": "2026-10-21 10:00:00",
        "model": model,
        "outcome": "completed",
        "elapsed_ms": wall,
        "ttft_ms": ttft,
        "gen_ms": gen,
        "tokens_per_sec": 12.5,
        "cache_hit_pct": 33.3,
        "prompt_tokens": 10,
        "completion_tokens": 10,
        "total_tokens": 20,
        "account": "acc-1",
        "realm": "cn",
    })
}

/// 基准时刻（epoch 秒）。
const T0: f64 = 1_700_000_000.0;

/// 顶层键（按响应里的出现顺序）。
fn top_keys(body: &Value) -> Vec<&str> {
    body.as_object()
        .expect("顶层是对象")
        .keys()
        .map(String::as_str)
        .collect()
}

// ---------------------------------------------------------------------------
// 1. 鉴权
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_usage_routes_require_auth() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    for path in NEW_ROUTES {
        let (status, body) = get(&app, path, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} 未鉴权应回 401");
        assert_eq!(body["error"]["code"], 401);
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
}

// ---------------------------------------------------------------------------
// 2. /v1/usage 与 /usage 同一份内容
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_route_has_the_expected_top_level_keys() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);
    wb_tasks::usage::append_usage_row(&usage, &usage_row(T0, "m1", 10)).unwrap();

    // 快照（Python `_empty_stats` + 装饰组；`usd_cny` 因无定价表而省略）。
    let (_, body) = get(&app, "/usage", true).await;
    assert_eq!(
        top_keys(&body),
        vec![
            "requests",
            "total_requests",
            "errors",
            "prompt_tokens",
            "completion_tokens",
            "reasoning_tokens",
            "cached_tokens",
            "total_tokens",
            "by_model",
            "started",
            "since",
            "log_file",
            "realm",
            "accounts_map",
            "account",
        ]
    );

    // 最近行（Python `recent_usage`；`usd_cny` 省略）。
    let (_, body) = get(&app, "/usage/recent", true).await;
    assert_eq!(
        top_keys(&body),
        vec!["total", "page", "limit", "total_pages", "rows"]
    );

    // 时间序列（Python `_usage_timeseries_scan`）。
    let (_, body) = get(&app, "/usage/timeseries", true).await;
    assert_eq!(
        top_keys(&body),
        vec![
            "ok",
            "realm",
            "bucket_seconds",
            "since",
            "until",
            "series",
            "credits",
        ]
    );

    // 性能统计（Python `_perf_stats_uncached`）。
    let (_, body) = get(&app, "/usage/perf", true).await;
    assert_eq!(
        top_keys(&body),
        vec![
            "sampled",
            "sample_from",
            "sample_capped",
            "success",
            "errors",
            "client_aborted",
            "success_rate_pct",
            "ttft_ms",
            "generation_ms",
            "wall_ms",
            "tokens_per_sec",
            "cache_hit_pct",
            "by_model",
        ]
    );

    // 活动历史（Python `wb_activity.query`）。
    let (_, body) = get(&app, "/activity/history", true).await;
    assert_eq!(
        top_keys(&body),
        vec!["rows", "total", "limit", "range", "filters"]
    );
}

#[tokio::test]
async fn v1_usage_returns_the_same_body_as_usage() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    wb_tasks::usage::append_usage_row(&usage, &usage_row(T0, "m1", 10)).unwrap();
    wb_tasks::usage::append_usage_row(&usage, &usage_row(T0 + 1.0, "m2", 30)).unwrap();

    let (status, plain) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, v1) = get(&app, "/v1/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(plain, v1, "两条路径必须返回同一份内容");
    assert_eq!(plain["total_requests"], 2);
    assert_eq!(plain["total_tokens"], 40);
}

#[tokio::test]
async fn usage_snapshot_carries_decoration_keys() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    let (status, body) = get(&app, "/usage", true).await;
    assert_eq!(status, StatusCode::OK);
    // 装饰键：`started` / `since` / `log_file` / `realm`；`usd_cny`（定价）
    // 与 `accounts_map` / `account`（池为空时）不出现。
    assert!(body["started"].as_f64().unwrap() > 0.0);
    assert!(body["since"].as_str().unwrap().len() >= 19);
    assert!(body["log_file"].as_str().unwrap().ends_with("usage.jsonl"));
    assert_eq!(body["realm"], "cn");
    assert!(
        body.get("usd_cny").is_none(),
        "没有定价表就不能写假的 usd_cny"
    );
}

// ---------------------------------------------------------------------------
// 3. /usage/recent 分页
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_recent_paginates_newest_first() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    for index in 0..5i64 {
        wb_tasks::usage::append_usage_row(
            &usage,
            &usage_row(T0 + index as f64, &format!("m{index}"), index + 1),
        )
        .unwrap();
    }

    let (status, body) = get(&app, "/usage/recent?limit=2&page=1", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["page"], 1);
    assert_eq!(body["total_pages"], 3);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // 最新在前：最后写入的那行排第一。
    assert_eq!(rows[0]["model"], "m4");
    assert_eq!(rows[1]["model"], "m3");

    // 第 3 页 = 最旧的一页。
    let (status, body) = get(&app, "/usage/recent?limit=2&page=3", true).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["model"], "m0");

    // 页码越界 ⇒ 夹回最后一页。
    let (status, body) = get(&app, "/usage/recent?limit=2&page=99", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 3);
    assert_eq!(body["rows"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn usage_recent_clamps_limit_and_falls_back_on_bad_input() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);

    // limit 上限 1000、下限 1；解析不了回默认 100。
    let (_, body) = get(&app, "/usage/recent?limit=99999", true).await;
    assert_eq!(body["limit"], 1000);
    let (_, body) = get(&app, "/usage/recent?limit=0", true).await;
    assert_eq!(body["limit"], 1);
    let (_, body) = get(&app, "/usage/recent?limit=abc", true).await;
    assert_eq!(body["limit"], 100);
    let (_, body) = get(&app, "/usage/recent?page=abc", true).await;
    assert_eq!(body["page"], 1);
}

// ---------------------------------------------------------------------------
// 4. /usage/timeseries 桶粒度
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_timeseries_bucket_granularity_scales_with_window() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);
    wb_tasks::usage::append_usage_row(&usage, &usage_row(T0, "m1", 10)).unwrap();

    // 1 小时窗口 ⇒ 60s 桶。
    let (_, body) = get(
        &app,
        &format!(
            "/usage/timeseries?range=custom&since={}&until={}",
            T0,
            T0 + 3600.0
        ),
        true,
    )
    .await;
    assert_eq!(body["bucket_seconds"], 60);
    assert_eq!(body["ok"], true);
    assert_eq!(body["since"], T0);
    assert_eq!(body["until"], T0 + 3600.0);
    assert_eq!(body["series"].as_array().unwrap().len(), 1);

    // 7 天窗口 ⇒ 3600s 桶。
    let (_, body) = get(
        &app,
        &format!(
            "/usage/timeseries?range=custom&since={}&until={}",
            T0,
            T0 + 7.0 * 86400.0
        ),
        true,
    )
    .await;
    assert_eq!(body["bucket_seconds"], 3600);

    // 30 天窗口 ⇒ 86400s 桶。
    let (_, body) = get(
        &app,
        &format!(
            "/usage/timeseries?range=custom&since={}&until={}",
            T0,
            T0 + 30.0 * 86400.0
        ),
        true,
    )
    .await;
    assert_eq!(body["bucket_seconds"], 86400);
}

#[tokio::test]
async fn usage_timeseries_bucket_parameter_overrides_granularity() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);

    let (_, body) = get(
        &app,
        &format!(
            "/usage/timeseries?range=custom&since={}&until={}&bucket=300",
            T0,
            T0 + 3600.0
        ),
        true,
    )
    .await;
    assert_eq!(body["bucket_seconds"], 300);

    // 下限 60 秒（Python `max(60, int(bucket_seconds))`）。
    let (_, body) = get(
        &app,
        &format!(
            "/usage/timeseries?range=custom&since={}&until={}&bucket=5",
            T0,
            T0 + 3600.0
        ),
        true,
    )
    .await;
    assert_eq!(body["bucket_seconds"], 60);
}

#[tokio::test]
async fn usage_timeseries_bucket_shape_matches_python() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);
    wb_tasks::usage::append_usage_row(&usage, &usage_row(T0, "m1", 40)).unwrap();

    let (_, body) = get(
        &app,
        &format!(
            "/usage/timeseries?range=custom&since={}&until={}",
            T0,
            T0 + 3600.0
        ),
        true,
    )
    .await;
    let bucket = &body["series"][0];
    assert_eq!(bucket["at"], T0);
    assert_eq!(bucket["requests"], 1);
    assert_eq!(bucket["errors"], 0);
    assert_eq!(bucket["prompt_tokens"], 20);
    assert_eq!(bucket["completion_tokens"], 20);
    assert_eq!(bucket["total_tokens"], 40);
    assert!(bucket.get("credit").is_some());
}

// ---------------------------------------------------------------------------
// 5. /usage/perf 指标块
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_perf_blocks_have_python_shape() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let usage = usage_dir(&cfg);
    let app = make_app(&cfg);

    for (index, ttft) in [100.0f64, 200.0, 300.0].iter().enumerate() {
        wb_tasks::usage::append_usage_row(
            &usage,
            &perf_row(T0 + index as f64, "m1", *ttft, 10.0, 50.0),
        )
        .unwrap();
    }

    let (status, body) = get(&app, "/usage/perf", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sampled"], 3);
    assert_eq!(body["success"], 3);
    assert_eq!(body["errors"], 0);
    assert_eq!(body["client_aborted"], 0);
    assert_eq!(body["success_rate_pct"], 100.0);
    assert_eq!(body["sample_capped"], false);

    // 指标块形状：{avg, p50, p90, p99, max, samples}。
    let ttft = &body["ttft_ms"];
    let keys: Vec<&String> = ttft.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["avg", "p50", "p90", "p99", "max", "samples"]);
    assert_eq!(ttft["avg"], 200.0);
    assert_eq!(ttft["p50"], 200.0);
    assert_eq!(ttft["p90"], 300.0);
    assert_eq!(ttft["p99"], 300.0);
    assert_eq!(ttft["max"], 300.0);
    assert_eq!(ttft["samples"], 3);

    // by_model 的桶形状相同。
    let model = &body["by_model"]["m1"];
    assert_eq!(model["requests"], 3);
    assert_eq!(model["errors"], 0);
    assert_eq!(model["client_aborted"], 0);
    assert_eq!(model["success_rate_pct"], 100.0);
    assert_eq!(model["ttft_ms"]["samples"], 3);
}

#[tokio::test]
async fn usage_perf_blocks_are_null_without_samples() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    // 用量日志不存在 ⇒ 一条样本都没有。
    let (status, body) = get(&app, "/usage/perf", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sampled"], 0);
    assert!(body["ttft_ms"].is_null(), "无样本时整块必须是 null");
    assert!(body["generation_ms"].is_null());
    assert!(body["wall_ms"].is_null());
    assert!(body["tokens_per_sec"].is_null());
    assert!(body["cache_hit_pct"].is_null());
    assert!(body["success_rate_pct"].is_null());
    assert!(body["by_model"].as_object().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 6. /activity/history
// ---------------------------------------------------------------------------

/// 往活动历史里塞几行（直接调写入点，绕开上游）。
fn seed_activity(cfg: &Config) {
    let usage = usage_dir(cfg);
    wb_tasks::activity::record_attempt(
        &usage,
        "acc-1",
        "tester",
        "cn",
        "checkin",
        "manual",
        true,
        "签到成功",
    )
    .expect("写入活动历史必须成功");
    wb_tasks::activity::record_attempt(
        &usage,
        "acc-1",
        "tester",
        "cn",
        "daily_chat",
        "manual",
        false,
        "网页通道失败",
    )
    .expect("写入活动历史必须成功");
}

#[tokio::test]
async fn activity_history_rejects_invalid_filters() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);

    // 非法 task ⇒ 400。
    let (status, body) = get(&app, "/activity/history?task=nope", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], 400);
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // 非法 result ⇒ 400。
    let (status, body) = get(&app, "/activity/history?result=weird", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], 400);

    // 非法 range ⇒ 400。
    let (status, body) = get(&app, "/activity/history?range=nope", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], 400);
}

#[tokio::test]
async fn activity_history_returns_python_shape() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    seed_activity(&cfg);

    let (status, body) = get(&app, "/activity/history", true).await;
    assert_eq!(status, StatusCode::OK);
    // 顶层键序与 Python 逐字一致：rows / total / limit / range / filters。
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["rows", "total", "limit", "range", "filters"]);
    assert_eq!(body["total"], 2);
    assert_eq!(body["limit"], 100);
    assert_eq!(body["range"], "7d");
    assert_eq!(body["filters"]["uid"], "");
    assert_eq!(body["filters"]["task"], "");
    assert_eq!(body["filters"]["result"], "");
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // 最新在前。
    assert_eq!(rows[0]["task"], "daily_chat");
    assert_eq!(rows[0]["ok"], false);
    assert_eq!(rows[1]["task"], "checkin");
    assert_eq!(rows[1]["ok"], true);
    assert_eq!(rows[1]["uid"], "acc-1");
    assert_eq!(rows[1]["nickname"], "tester");
    assert_eq!(rows[1]["realm"], "cn");
    assert_eq!(rows[1]["trigger"], "manual");
    assert_eq!(rows[1]["message"], "签到成功");
}

#[tokio::test]
async fn activity_history_filters_by_task_and_result() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);
    seed_activity(&cfg);

    let (_, body) = get(&app, "/activity/history?task=checkin", true).await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["filters"]["task"], "checkin");
    assert_eq!(body["rows"][0]["task"], "checkin");

    let (_, body) = get(&app, "/activity/history?result=failed", true).await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["filters"]["result"], "failed");
    assert_eq!(body["rows"][0]["task"], "daily_chat");
}

// ---------------------------------------------------------------------------
// 7. 端到端：签到 ⇒ 活动历史能读到
// ---------------------------------------------------------------------------

/// 固定时间的时钟。
struct FixedClock;

impl Clock for FixedClock {
    fn local_hour(&self) -> u32 {
        9
    }
    fn local_now(&self) -> String {
        "2026-10-21 09:00:00".to_string()
    }
}

/// 一个 cn、非企业的账号。
fn cn_account() -> RunnerAccount<'static> {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer tok".to_string());
    let headers: &'static HashMap<String, String> = Box::leak(Box::new(headers));
    RunnerAccount {
        api: TaskAccount {
            uid: "acc-e2e",
            token: "tok",
            enterprise_id: "",
            chat_headers: headers,
        },
        realm: "cn",
        nickname: Some("e2e-user"),
        is_enterprise: false,
    }
}

#[tokio::test]
async fn checkin_writes_an_activity_row_that_history_can_read() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);

    // 账号文件（签到要往里写 lastCheckin）。
    let accounts = cfg.accounts_dir.clone();
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(
        accounts.join("acc-e2e.json"),
        json!({"nickname": "e2e-user"}).to_string(),
    )
    .unwrap();

    // 假上游：签到成功。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "msg": "签到成功",
        })))
        .mount(&server)
        .await;
    let billing_base = server.uri();

    let ctx = CheckinCtx {
        dir: &accounts,
        account_name: "acc-e2e.json",
        clock: &FixedClock,
        device_token: None,
    };
    let outcome = checkin(
        &wb_gateway::outbound::UpstreamClient::new(),
        &cn_account(),
        &billing_base,
        &ctx,
    )
    .await;
    assert!(outcome.ok, "签到应当成功：{:?}", outcome.to_json());

    // 活动历史文件落在用量目录里（与 Python 同一个文件）。
    let history = usage_dir(&cfg).join("activity_history.jsonl");
    assert!(history.exists(), "签到应当写一行活动历史");

    // 端到端：从 HTTP 接口读回来。
    let (status, body) = get(&app, "/activity/history?range=all", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    let row = &body["rows"][0];
    assert_eq!(row["uid"], "acc-e2e");
    assert_eq!(row["nickname"], "e2e-user");
    assert_eq!(row["realm"], "cn");
    assert_eq!(row["task"], "checkin");
    assert_eq!(row["ok"], true);
    assert_eq!(row["message"], "签到成功");
    // 时间戳是可解析的 ISO 串（区间过滤靠它）。
    assert!(row["ts"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn failed_checkin_also_writes_an_activity_row() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let app = make_app(&cfg);

    let accounts = cfg.accounts_dir.clone();
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("acc-e2e.json"), "{}").unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 1,
            "msg": "风控拦截",
        })))
        .mount(&server)
        .await;
    let billing_base = server.uri();

    let ctx = CheckinCtx {
        dir: &accounts,
        account_name: "acc-e2e.json",
        clock: &FixedClock,
        device_token: None,
    };
    let outcome = checkin(
        &wb_gateway::outbound::UpstreamClient::new(),
        &cn_account(),
        &billing_base,
        &ctx,
    )
    .await;
    assert!(!outcome.ok);

    let (status, body) = get(&app, "/activity/history?range=all", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["rows"][0]["ok"], false);
    assert_eq!(body["rows"][0]["message"], "风控拦截");
}
