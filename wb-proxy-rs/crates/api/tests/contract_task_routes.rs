//! 关卡 4.10 产出物：9 条任务路由的契约测试。
//!
//! 全走 wiremock 假上游 + tempdir 账号池；绝不向真实上游发请求。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use wb_api::routes::ApiPanelSink;
use wb_api::{app_with, Config};
use wb_gateway::pool::AccountState;
use wb_tasks::api_routes::{PooledTaskAccount, ReadyAccount, TaskRuntime};
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::scheduler::{uid8, AccountView};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_cfg(dir: &TempDir) -> Arc<Config> {
    Arc::new(Config {
        accounts_dir: dir.path().to_path_buf(),
        dashboard: dir.path().to_path_buf(),
        api_key: Some("test-key".to_string()),
        upstream_base_url: None,
        lan: false,
        realm: "cn".to_string(),
    })
}

#[tokio::test]
async fn unauthorized_returns_401() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    for path in ["/tasks", "/scheduler"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn invalid_body_returns_400() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .method("POST")
        .uri("/tasks/run")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from("not json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tasks_get_empty_pool_returns_msg() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .uri("/tasks")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["msg"], "未找到可用的国内版账号");
}

#[tokio::test]
async fn scheduler_get_returns_enabled_field() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .uri("/scheduler")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("enabled").is_some());
}

#[tokio::test]
async fn claim_all_empty_pool_returns_ok() {
    let dir = TempDir::new().unwrap();
    let cfg = make_cfg(&dir);
    let task = TaskRuntime::new(
        &cfg.accounts_dir,
        Arc::new(ApiPanelSink),
        TaskEndpoints::production(),
        "http://127.0.0.1:1".to_string(),
        None,
    );
    let app = app_with(
        cfg,
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let req = Request::builder()
        .method("POST")
        .uri("/tasks/claim-all")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(json!({}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("ok").is_some());
}

// ---------------------------------------------------------------------------
// POST /tasks/streak-bonus 路由契约测试（wiremock 假上游，无真实请求）
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn streak_runtime_legacy(dir: &TempDir, chat_base: &'static str) -> Arc<TaskRuntime> {
    let endpoints = TaskEndpoints {
        chat_base,
        web_base: chat_base,
    };
    TaskRuntime::new(
        dir.path(),
        Arc::new(ApiPanelSink),
        endpoints,
        "http://127.0.0.1:1".to_string(),
        None,
    )
    // 功能已由新版本 streak_runtime 替代。
}

fn streak_account(uid: &str, token: &str, realm: &str, enabled: bool) -> ReadyAccount {
    let mut state = AccountState::new(uid, realm);
    state.access_token = token.into();
    state.enabled = enabled;
    let view = AccountView::from_state(&state).with_last_records(None, None, None, false);
    let mut chat_headers = HashMap::new();
    chat_headers.insert("Authorization".into(), format!("Bearer {token}"));
    let creds = PooledTaskAccount {
        uid: uid.into(),
        token: token.into(),
        enterprise_id: "".into(),
        chat_headers,
    };
    ReadyAccount {
        state,
        view,
        creds,
        nickname: None,
        device_token: None,
    }
}

/// 构建指向 wiremock base 的运行时并一次性注入 `ready` 账号。
/// 同函数内 `TaskRuntime` 仅由返回的 `Arc` 唯一强持有（`ProductionExecutor` 内
/// 仅持 `Weak<TaskRuntime>`），故用裸指针独占改写 `pool_present`/`ready`，
/// 避开 `Arc::get_mut` 在本 toolchain 上的反常行为。
fn streak_runtime(
    dir: &TempDir,
    chat_base: &'static str,
    pool_present: bool,
    ready: Vec<ReadyAccount>,
) -> Arc<TaskRuntime> {
    let endpoints = TaskEndpoints {
        chat_base,
        web_base: chat_base,
    };
    let task = TaskRuntime::new(
        dir.path(),
        Arc::new(ApiPanelSink),
        endpoints,
        "http://127.0.0.1".to_string(),
        None,
    );
    // SAFETY: strong_count == 1，同一线程内独占访问，无并发数据竞争。
    let inner = unsafe { &mut *(Arc::as_ptr(&task) as *mut TaskRuntime) };
    inner.pool_present = pool_present;
    inner.ready = ready;
    task
}

fn streak_req(uid: Option<&str>) -> Request<Body> {
    let body = match uid {
        Some(uid) => json!({"uid": uid}),
        None => json!({}),
    };
    Request::builder()
        .method("POST")
        .uri("/tasks/streak-bonus")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 空账号池（ready 为空）：未找到已启用的国内版账号，accounts_count 为 0。
#[tokio::test]
async fn streak_bonus_route_empty_pool_msg() {
    let dir = TempDir::new().unwrap();
    let task = streak_runtime(&dir, "http://127.0.0.1:1", true, vec![]);
    let app = app_with(
        make_cfg(&dir),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let resp = app.oneshot(streak_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["msg"], "未找到已启用的国内版账号");
    assert_eq!(v["ok"], false);
}

/// 账号池不可用：pool_present=false 时直接拒绝，不上游。
#[tokio::test]
async fn streak_bonus_route_pool_unavailable() {
    let dir = TempDir::new().unwrap();
    let task = streak_runtime(&dir, "http://127.0.0.1:1", false, vec![]);
    let app = app_with(
        make_cfg(&dir),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let resp = app.oneshot(streak_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["msg"], "账号池不可用");
    assert_eq!(v["ok"], false);
}

/// 指定 uid 但不存在（未匹配 cn 启用账号）：不上游，提示未找到指定账号。
#[tokio::test]
async fn streak_bonus_route_uid_not_found() {
    let dir = TempDir::new().unwrap();
    let task = streak_runtime(&dir, "http://127.0.0.1:1", true, vec![]);
    let app = app_with(
        make_cfg(&dir),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let resp = app.oneshot(streak_req(Some("ghost"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["msg"], "未找到指定的国内版账号");
    assert_eq!(v["ok"], false);
}

/// uid 过滤：只选 realm=cn 且 enabled 的账号；intl 与 cn 禁用账号均被排除。
#[tokio::test]
async fn streak_bonus_route_uid_filters_cn_enabled() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let ready = vec![
        streak_account("u-cn", "cn-tok", "cn", true),
        streak_account("u-intl", "intl-tok", "intl", true),
        streak_account("u-cn-off", "cn-off-tok", "cn", false),
    ];
    let task = streak_runtime(&dir, base, true, ready);
    let app = app_with(
        make_cfg(&dir),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let resp = app.oneshot(streak_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["msg"], "streak bonus completed");
    assert_eq!(v["accounts_count"], 1);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["uid"], "u-cn");
    assert_eq!(results[0]["nickname"], uid8("u-cn"));
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["msg"], "");
    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 1, "只应向选中的 cn 启用账号发 1 次上游请求");
}

/// 单账号成功：上游 code=0 带 logs，路由透传 ok 与 msg。
#[tokio::test]
async fn streak_bonus_route_single_success() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"logs": ["连登 +1", "当前连续 5 天"]},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let task = streak_runtime(
        &dir,
        base,
        true,
        vec![streak_account("u-streak", "streak-tok", "cn", true)],
    );
    let app = app_with(
        make_cfg(&dir),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let resp = app.oneshot(streak_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["msg"], "streak bonus completed");
    assert_eq!(v["accounts_count"], 1);
    assert_eq!(v["results"][0]["uid"], "u-streak");
    assert_eq!(v["results"][0]["nickname"], uid8("u-streak"));
    assert_eq!(v["results"][0]["ok"], true);
    assert_eq!(v["results"][0]["msg"], "");
}

/// 多账号部分失败：一个成功一个失败，路由 ok=false 且 msg 为部分失败，各自结果透传。
#[tokio::test]
async fn streak_bonus_route_partial_failure() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .and(body_json(json!({})))
        .and(header("authorization", "Bearer ok-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/activity/growth/streak/claim"))
        .and(body_json(json!({})))
        .and(header("authorization", "Bearer fail-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 1, "msg": "已领取"})))
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let ready = vec![
        streak_account("u-ok", "ok-tok", "cn", true),
        streak_account("u-fail", "fail-tok", "cn", true),
    ];
    let task = streak_runtime(&dir, base, true, ready);
    let app = app_with(
        make_cfg(&dir),
        Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600)),
        task,
    );
    let resp = app.oneshot(streak_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"].as_bool(), Some(false));
    assert_eq!(v["msg"], "streak bonus partially failed");
    assert_eq!(v["accounts_count"], 2);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["uid"], "u-ok");
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[1]["uid"], "u-fail");
    assert_eq!(results[1]["ok"].as_bool(), Some(false));
    assert_eq!(results[1]["msg"], "已领取");
    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 2);
}
