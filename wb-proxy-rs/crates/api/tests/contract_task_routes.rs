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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let _token = panels.create();
    let app = app_with(cfg, panels, task);
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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(cfg, panels, task);
    let req = Request::builder()
        .method("POST")
        .uri("/tasks/run")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token.as_str())
        .header("x-panel-token", token.as_str())
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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(cfg, panels, task);
    let req = Request::builder()
        .uri("/tasks")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token.as_str())
        .header("x-panel-token", token.as_str())
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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(cfg, panels, task);
    let req = Request::builder()
        .uri("/scheduler")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token.as_str())
        .header("x-panel-token", token.as_str())
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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(cfg, panels, task);
    let req = Request::builder()
        .method("POST")
        .uri("/tasks/claim-all")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", token.as_str())
        .header("x-panel-token", token.as_str())
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
        realm: realm.into(),
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
    // `billing_base` 必须与 `chat_base` 同指 mock server：连登管家的
    // 「新手礼包 / 活动补偿」两步走的是 billing 域（Python 的 `BILL_BASE`），
    // 若指向别处，这两个请求会绕过 mock 打向真实上游。
    let task = TaskRuntime::new(
        dir.path(),
        Arc::new(ApiPanelSink),
        endpoints,
        chat_base.to_string(),
        None,
    );
    // SAFETY: strong_count == 1，同一线程内独占访问，无并发数据竞争。
    let inner = unsafe { &mut *(Arc::as_ptr(&task) as *mut TaskRuntime) };
    inner.pool_present = pool_present;
    inner.ready = ready;
    task
}

fn streak_req(uid: Option<&str>, panel_token: &str) -> Request<Body> {
    let body = match uid {
        Some(uid) => json!({"uid": uid}),
        None => json!({}),
    };
    Request::builder()
        .method("POST")
        .uri("/tasks/streak-bonus")
        .header("authorization", "Bearer test-key")
        .header("x-panel-token", panel_token)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 挂上 Python `run_streak_bonus` 四步用到的**全部**上游端点。
///
/// 口径逐字对齐 `wb_tasks.py:771-866`：
/// - `GET /activity/growth/heatmap`：`cells` 为空 ⇒ 找不到昨天的格子 ⇒
///   不算漏签 ⇒ **不**查补签卡、**不**发 `makeup-cards/use`；
/// - `GET /activity/growth/streak`：`redemption_status.tiers` 为空 ⇒ 不兑换；
/// - `POST /billing/meter/claim-gift` / `claim-compensation`：`code != 0` ⇒ 不记日志；
/// - `GET /activity/growth/lottery/summary`：`chances = 0` ⇒ 不抽奖。
///
/// 于是「什么都没做」⇒ `msg` = 「连登状态正常（无需兑换/无抽奖次数）」。
async fn mount_streak_bonus_steps(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/activity/growth/heatmap"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"cells": []}})),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/activity/growth/streak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {
                "makeup_cards": {"balance": 0},
                "redemption_status": {"tiers": []}
            }
        })))
        .mount(server)
        .await;

    // Python：两个 billing 端点各 POST 一个空对象 `{}`。
    Mock::given(method("POST"))
        .and(path("/billing/meter/claim-gift"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 1, "msg": "已领取"})))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/billing/meter/claim-compensation"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 1, "msg": "无补偿"})))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/activity/growth/lottery/summary"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"chances": 0}})),
        )
        .mount(server)
        .await;
}

/// 让**指定 token** 的那几步上游全部返回 500。
///
/// 优先级设为 1（wiremock：`1` 最高，同优先级按挂载顺序），确保它压过
/// [`mount_streak_bonus_steps`] 挂的通用 mock。
async fn mount_streak_bonus_failures(server: &MockServer, token: &str) {
    for (m, p) in [
        (method("GET"), "/activity/growth/heatmap"),
        (method("GET"), "/activity/growth/streak"),
        (method("GET"), "/activity/growth/lottery/summary"),
        (method("POST"), "/billing/meter/claim-gift"),
        (method("POST"), "/billing/meter/claim-compensation"),
    ] {
        Mock::given(m)
            .and(path(p))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .with_priority(1)
            .mount(server)
            .await;
    }
}

/// 造一个**状态与凭据 realm 不一致**的账号。
///
/// 生产上账号池的 `state.realm` 与 `creds.realm` 同源，不会不一致；这个构造
/// 只为锁定路由的**聚合分支**：任一账号 `ok == false` ⇒ 整体 `ok == false`
/// 且 `msg` 变为「streak bonus partially failed」（`dashboard.html` 据
/// `r.ok` 显示失败态）。
fn streak_account_split_realm(
    uid: &str,
    token: &str,
    state_realm: &str,
    creds_realm: &str,
    enabled: bool,
) -> ReadyAccount {
    let mut state = AccountState::new(uid, state_realm);
    state.access_token = token.into();
    state.enabled = enabled;
    let view = AccountView::from_state(&state).with_last_records(None, None, None, false);
    let mut chat_headers = HashMap::new();
    chat_headers.insert("Authorization".into(), format!("Bearer {token}"));
    let creds = PooledTaskAccount {
        uid: uid.into(),
        token: token.into(),
        enterprise_id: "".into(),
        realm: creds_realm.into(),
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

/// 空账号池（ready 为空）：未找到已启用的国内版账号，accounts_count 为 0。
#[tokio::test]
async fn streak_bonus_route_empty_pool_msg() {
    let dir = TempDir::new().unwrap();
    let task = streak_runtime(&dir, "http://127.0.0.1:1", true, vec![]);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app.oneshot(streak_req(None, &token)).await.unwrap();
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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app.oneshot(streak_req(None, &token)).await.unwrap();
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
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app
        .oneshot(streak_req(Some("ghost"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["msg"], "未找到指定的国内版账号");
    assert_eq!(v["ok"], false);
}

/// uid 过滤：只选 realm=cn 且 enabled 的账号；intl 与 cn 禁用账号均被排除。
///
/// ⚠️ mock 的是 Python `wb_tasks.run_streak_bonus` 的**四步**端点
/// （`wb_tasks.py:771-866`），不是旧实现那个上游不存在的
/// `POST /activity/growth/streak/claim`。
#[tokio::test]
async fn streak_bonus_route_uid_filters_cn_enabled() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    mount_streak_bonus_steps(&server).await;

    let dir = TempDir::new().unwrap();
    let ready = vec![
        streak_account("u-cn", "cn-tok", "cn", true),
        streak_account("u-intl", "intl-tok", "intl", true),
        streak_account("u-cn-off", "cn-off-tok", "cn", false),
    ];
    let task = streak_runtime(&dir, base, true, ready);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app.oneshot(streak_req(None, &token)).await.unwrap();
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
    // Python `run_streak_bonus` 的 `msg` 是**日志拼接**（无日志时是
    // 「连登状态正常（无需兑换/无抽奖次数）」），不是空串。
    assert_eq!(results[0]["msg"], "连登状态正常（无需兑换/无抽奖次数）");
    // 只应向选中的 cn 启用账号发请求（intl / 停用账号零请求）。
    let reqs = server.received_requests().await.expect("requests");
    assert!(!reqs.is_empty(), "选中的 cn 启用账号应当有上游请求");
    for r in &reqs {
        assert!(
            r.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap_or_default() == "Bearer cn-tok")
                .unwrap_or(false),
            "intl / 停用账号不应有上游请求：{r:?}"
        );
    }
}

/// 单账号成功：走完四步，路由透传 ok 与 msg。
#[tokio::test]
async fn streak_bonus_route_single_success() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    mount_streak_bonus_steps(&server).await;

    let dir = TempDir::new().unwrap();
    let task = streak_runtime(
        &dir,
        base,
        true,
        vec![streak_account("u-streak", "streak-tok", "cn", true)],
    );
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app.oneshot(streak_req(None, &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["msg"], "streak bonus completed");
    assert_eq!(v["accounts_count"], 1);
    assert_eq!(v["results"][0]["uid"], "u-streak");
    assert_eq!(v["results"][0]["nickname"], uid8("u-streak"));
    assert_eq!(v["results"][0]["ok"], true);
    // 本 mock 下：昨天没漏签（不补签）、无兑换档位、无抽奖次数 ⇒ 无日志 ⇒
    // `msg` 是 Python 的「连登状态正常（无需兑换/无抽奖次数）」。
    assert_eq!(
        v["results"][0]["msg"],
        "连登状态正常（无需兑换/无抽奖次数）"
    );
}

/// 上游整片失败**不改 ok**：Python 的 `run_streak_bonus` 里各步失败只跳过
/// 该步、整体仍 `ok: true`（失败原因进 `_log`，返回体里看不到）。
///
/// Rust 侧对应的是 `StreakBonusOutcome.warnings`（**不进 `to_json()`**），
/// 所以路由结果里既不该出现 `ok:false`，也不该泄漏 `warnings`。
#[tokio::test]
async fn streak_bonus_route_upstream_failure_keeps_ok_true() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    mount_streak_bonus_steps(&server).await;
    mount_streak_bonus_failures(&server, "fail-tok").await;

    let dir = TempDir::new().unwrap();
    let ready = vec![
        streak_account("u-ok", "ok-tok", "cn", true),
        streak_account("u-fail", "fail-tok", "cn", true),
    ];
    let task = streak_runtime(&dir, base, true, ready);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app.oneshot(streak_req(None, &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true, "上游失败不改整体 ok（与 Python 一致）：{v}");
    assert_eq!(v["msg"], "streak bonus completed");
    assert_eq!(v["accounts_count"], 2);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["uid"], "u-ok");
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[1]["uid"], "u-fail");
    assert_eq!(results[1]["ok"], true, "该账号的上游全 500，但账号级仍 ok");
    // 失败原因不落在返回体里（Python 写 `_log`，Rust 写 `warnings`）。
    assert!(results[1].get("warnings").is_none(), "{results:?}");
    assert!(results[1].get("logs").is_none(), "{results:?}");
}

/// 路由的**聚合分支**：任一账号 `ok:false` ⇒ 整体 `ok:false` +
/// 「streak bonus partially failed」。
///
/// 唯一能产出账号级 `ok:false` 的是连登管家的**国际版门禁**，而路由筛选
/// 只放 `realm == "cn"` 的账号——生产上两者同源、不会同时发生，所以这里用
/// `state.realm` 与 `creds.realm` 不一致的构造来锁定聚合分支本身。
#[tokio::test]
async fn streak_bonus_route_partial_failure() {
    let server = MockServer::start().await;
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    mount_streak_bonus_steps(&server).await;

    let dir = TempDir::new().unwrap();
    let ready = vec![
        streak_account("u-ok", "ok-tok", "cn", true),
        // 状态是 cn（能过路由筛选），但凭据是 intl ⇒ 撞上国际版门禁。
        streak_account_split_realm("u-gate", "gate-tok", "cn", "intl", true),
    ];
    let task = streak_runtime(&dir, base, true, ready);
    let panels = Arc::new(wb_api::auth::PanelSessions::new(7 * 24 * 3600));
    let token = panels.create();
    let app = app_with(make_cfg(&dir), panels, task);
    let resp = app.oneshot(streak_req(None, &token)).await.unwrap();
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
    assert_eq!(results[1]["uid"], "u-gate");
    // 国际版门禁：`{"ok": false, "msg": "连登管家仅限国内版账号"}`（无 logs 键）。
    assert_eq!(results[1]["ok"].as_bool(), Some(false));
    assert_eq!(results[1]["msg"], "连登管家仅限国内版账号");
    // 门禁早退 ⇒ 不发任何上游请求。
    let reqs = server.received_requests().await.expect("requests");
    assert!(
        reqs.iter().all(|r| r
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default())
            != Some("Bearer gate-tok")),
        "撞门禁的账号不该有上游请求：{reqs:?}"
    );
}
