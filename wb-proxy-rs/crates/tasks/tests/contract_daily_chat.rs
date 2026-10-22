//! 关卡 4.14：国际版每日活跃（daily_chat）契约测试。
//!
//! 端点（**不是**旧的臆造端点 `/v2/activity/daily-chat`）：
//! - 桌面腿：`POST {chat_base}/v2/chat/completions`
//! - 网页腿：`POST {web_base}/console/as/conversations/`
//!   → `GET {web_base}/console/as/conversations/{id}/session`
//!   → ACP：`GET/POST {link}`（SSE + JSON-RPC）
//!   → `GET {web_base}/console/as/conversations/{id}`（状态轮询）
//!
//! 必测语义（对齐 Python `wb_accounts.py:1560-1661`）：
//! 1. 两腿都成功 → `ok:true` + **写** `lastDailyChat`
//! 2. 桌面腿成功、网页腿失败 → `ok:true`（桌面腿的结论）+ **不写** `lastDailyChat`
//!    （否则当天的 30/50 积分静默丢掉且不再重试）
//! 3. 桌面腿被渠道校验（`code:11128`）拦下 → **降级**，网页腿单独跑
//! 4. 11128 + 网页腿也失败 → `ok:false`，不写盘
//! 5. 桌面腿其他失败 → 直接失败，**不跑**网页腿
//! 6. 非国际版账号 → 拒绝，不发任何请求
//! 7. `daily_chat_web:false` → 只跑桌面腿；桌面腿成功即算完成（旧语义）
//!
//! 全程 wiremock + tempdir，**不向真实上游发任何请求**。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_tasks::api_routes::{PooledTaskAccount, ReadyAccount, TaskRuntime};
use wb_tasks::daily_chat::DailyChatConfig;
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::scheduler::{AccountView, CycleExecutor, PanelSink};

const ACCOUNT_NAME: &str = "acct.json";
const CONV: &str = "conv-1";

struct NullPanel;

impl PanelSink for NullPanel {
    fn add_log_entry(&self, _msg: &str, _tag: &str) {}
}

/// 建账号目录：一个账号文件（`{}`）+ 可选的 `settings.json`。
fn account_dir(daily_chat_web: Option<bool>) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(accounts_dir(dir.path()).join(ACCOUNT_NAME), "{}").expect("seed account file");
    if let Some(flag) = daily_chat_web {
        std::fs::write(
            accounts_dir(dir.path()).join("settings.json"),
            json!({"daily_chat_web": flag}).to_string(),
        )
        .expect("write settings");
    }
    dir
}

/// 建账号目录，`settings.json` 写**原始文本**（用来喂损坏的 JSON）。
fn account_dir_with_raw_settings(raw: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(accounts_dir(dir.path()).join(ACCOUNT_NAME), "{}").expect("seed account file");
    std::fs::write(accounts_dir(dir.path()).join("settings.json"), raw).expect("write settings");
    dir
}

/// 读 `lastDailyChat`（缺失 → None）。
fn read_last_daily_chat(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(ACCOUNT_NAME)).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    doc.get("lastDailyChat")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn acc_view(uid: &str, realm: &str) -> AccountView {
    let mut s = wb_gateway::pool::AccountState::new(uid, realm);
    s.access_token = format!("{uid}-tok");
    AccountView::from_state(&s).with_last_records(None, None, None, false)
}

/// 装配一个指向 mock 上游的执行器。
fn build_exec(server: &MockServer, uid: &str, realm: &str, dir: &Path) -> Arc<dyn CycleExecutor> {
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    let endpoints = TaskEndpoints {
        chat_base: base,
        web_base: base,
    };
    // 两条腿都指向 mock 上游（M4 硬约束：不得对真实上游发任何请求）。
    let daily_chat_config = DailyChatConfig {
        chat_base: Some(base.to_string()),
        web_base: Some(base.to_string()),
        web_turn_timeout: 5,
        web_poll_interval: 0,
    };

    let mut chat_headers = HashMap::new();
    chat_headers.insert("Authorization".into(), format!("Bearer {uid}-tok"));

    let mut state = wb_gateway::pool::AccountState::new(uid, realm);
    state.access_token = format!("{uid}-tok");
    // 幂等字段回写需要账号文件名（`wb_storage` 只认 `<name>.json`）。
    state.credential_file = ACCOUNT_NAME.into();
    let view = AccountView::from_state(&state).with_last_records(None, None, None, false);
    let creds = PooledTaskAccount {
        uid: uid.into(),
        token: format!("{uid}-tok"),
        enterprise_id: "".into(),
        realm: realm.into(),
        chat_headers,
    };
    let ready = vec![ReadyAccount {
        state,
        view,
        creds,
        nickname: None,
        device_token: None,
    }];

    TaskRuntime::test_with_ready_dir(
        ready,
        endpoints,
        daily_chat_config,
        dir.to_path_buf(),
        Arc::new(NullPanel),
    )
}

/// 挂桌面腿的 mock。
async fn mount_desktop(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
        .mount(server)
        .await;
}

/// 挂一条**完整可用**的网页腿：建会话 → 取沙箱 → ACP → 状态 completed。
async fn mount_web_ok(server: &MockServer, base: &str) {
    mount_web_ok_with(server, base, CONV, json!(CONV)).await;
}

/// 同上，但建会话返回的 id 由 `conv_id` 指定（可以是数字）。
///
/// `conv_path` 是这个 id 出现在 URL 里的形态（`str(id)`）。
async fn mount_web_ok_with(server: &MockServer, base: &str, conv_path: &str, conv_id: Value) {
    Mock::given(method("POST"))
        .and(path("/console/as/conversations/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"id": conv_id}})),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/console/as/conversations/{conv_path}/session"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "link": format!("{base}/acp"),
                "token": "sandbox-tok",
                "sessionId": "sess-1",
                "cwd": "/workspace"
            }
        })))
        .mount(server)
        .await;

    // ACP 的 SSE 通道：必须回 `Acp-Connection-Id`，否则客户端起不来。
    Mock::given(method("GET"))
        .and(path("/acp"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Acp-Connection-Id", "conn-1")
                .set_body_string(
                    "data: {\"method\":\"session/update\",\"params\":{\"update\":\
{\"sessionUpdate\":\"agent_message_chunk\"}}}\n\n",
                ),
        )
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/acp"))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/console/as/conversations/{conv_path}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"status": "completed"}})),
        )
        .mount(server)
        .await;
}

/// 网页腿「建会话就失败」的 mock。
async fn mount_web_broken(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/console/as/conversations/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// 1：两腿都成功 → 写 lastDailyChat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_and_web_success_writes_last_daily_chat() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_desktop(&server, 200, "{}").await;
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    let msg = res.get("msg").and_then(Value::as_str).unwrap_or_default();
    assert!(msg.contains("网页通道 completed"), "msg={msg}");
    assert!(msg.contains("段输出"), "msg={msg}");

    let web = res.get("web").expect("web 段");
    assert_eq!(web.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(web.get("status").and_then(Value::as_str), Some("completed"));
    assert_eq!(web.get("conversation").and_then(Value::as_str), Some(CONV));

    assert!(
        read_last_daily_chat(&accounts_dir(dir.path())).is_some(),
        "网页腿成功必须写 lastDailyChat"
    );
}

// ---------------------------------------------------------------------------
// 2：桌面腿成功、网页腿失败 → 不写盘（当天还要重试）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_success_web_failure_does_not_write_last_daily_chat() {
    let server = MockServer::start().await;
    mount_desktop(&server, 200, "{}").await;
    mount_web_broken(&server).await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    let msg = res.get("msg").and_then(Value::as_str).unwrap_or_default();
    assert!(msg.contains("网页通道失败"), "msg={msg}");
    assert!(
        read_last_daily_chat(&accounts_dir(dir.path())).is_none(),
        "网页腿失败时不得写 lastDailyChat，否则当天积分静默丢掉"
    );
}

// ---------------------------------------------------------------------------
// 3：桌面腿被渠道校验拦下 → 降级到网页腿
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_channel_blocked_falls_back_to_web_leg() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_desktop(
        &server,
        400,
        r#"{"code":11128,"msg":"这次调用没通过渠道校验"}"#,
    )
    .await;
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    let msg = res.get("msg").and_then(Value::as_str).unwrap_or_default();
    assert!(msg.contains("桌面腿被渠道校验拦截"), "msg={msg}");
    assert!(msg.contains("网页通道 completed"), "msg={msg}");
    assert!(read_last_daily_chat(&accounts_dir(dir.path())).is_some());
}

// ---------------------------------------------------------------------------
// 4：11128 + 网页腿也失败 → 整体失败，不写盘
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_channel_blocked_and_web_failed_is_failure() {
    let server = MockServer::start().await;
    mount_desktop(&server, 400, r#"{"code":11128}"#).await;
    mount_web_broken(&server).await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    let err = res.get("error").and_then(Value::as_str).unwrap_or_default();
    assert!(err.contains("桌面腿被渠道校验拦截"), "err={err}");
    assert!(err.contains("网页通道也失败"), "err={err}");
    assert!(read_last_daily_chat(&accounts_dir(dir.path())).is_none());
}

// ---------------------------------------------------------------------------
// 5：桌面腿其他失败 → 直接失败，不跑网页腿
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_400_without_11128_fails_without_web_leg() {
    let server = MockServer::start().await;
    mount_desktop(&server, 400, r#"{"code":1,"msg":"参数错误"}"#).await;
    Mock::given(method("POST"))
        .and(path("/console/as/conversations/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    let err = res.get("error").and_then(Value::as_str).unwrap_or_default();
    assert!(err.starts_with("HTTP 400: "), "err={err}");
    assert!(read_last_daily_chat(&accounts_dir(dir.path())).is_none());
}

// ---------------------------------------------------------------------------
// 6：非国际版账号 → 拒绝，不发任何请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cn_realm_is_rejected_without_requests() {
    let server = MockServer::start().await;
    let dir = account_dir(None);
    let exec = build_exec(&server, "u-cn", "cn", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-cn", "cn")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("daily chat is only for international accounts")
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "非国际版账号不得发上游请求"
    );
}

// ---------------------------------------------------------------------------
// 7：`daily_chat_web:false` → 只跑桌面腿，桌面腿成功即算完成
// ---------------------------------------------------------------------------

#[tokio::test]
async fn web_leg_disabled_keeps_desktop_only_semantics() {
    let server = MockServer::start().await;
    mount_desktop(&server, 200, "{}").await;
    Mock::given(method("POST"))
        .and(path("/console/as/conversations/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let dir = account_dir(Some(false));
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        res.get("msg").and_then(Value::as_str),
        Some("每日活跃对话成功完成")
    );
    assert!(res.get("web").is_none(), "关了网页腿就不该有 web 段");
    assert!(
        read_last_daily_chat(&accounts_dir(dir.path())).is_some(),
        "没开网页腿时桌面腿成功即算完成（旧语义）"
    );
}

// ---------------------------------------------------------------------------
// 8：桌面腿的请求体与出站头逐字对齐 Python
// ---------------------------------------------------------------------------

#[tokio::test]
async fn desktop_request_body_matches_python() {
    let server = MockServer::start().await;
    mount_desktop(&server, 200, "{}").await;

    let dir = account_dir(Some(false));
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let _ = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    let reqs = server.received_requests().await.expect("requests");
    let desktop = reqs
        .iter()
        .find(|r| r.url.path() == "/v2/chat/completions")
        .expect("桌面腿请求");
    let body: Value = serde_json::from_slice(&desktop.body).expect("json body");
    assert_eq!(
        body.get("model").and_then(Value::as_str),
        Some("deepseek-v4.1-flash")
    );
    assert_eq!(body.get("max_tokens").and_then(Value::as_i64), Some(10));
    assert_eq!(
        body.get("reasoning_effort").and_then(Value::as_str),
        Some("none")
    );
    assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
    assert_eq!(
        desktop
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer u-chat-tok"
    );
}

// ---------------------------------------------------------------------------
// 9：账号不找到 → 不发上游请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn account_not_found_sends_no_request() {
    let server = MockServer::start().await;
    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("no-such-uid", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        res.get("error").and_then(Value::as_str),
        Some("account not found")
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "账号不找到时不得发上游请求"
    );
}

// ---------------------------------------------------------------------------
// 10：写盘后账号视图要能看到新值
//
// 否则调度器下一轮仍判「今天没做过」，同一天会反复跑每日活跃对话——
// 每次都在上游建一次真实 agent 会话。
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accounts_view_refreshes_last_daily_chat_after_write() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_desktop(&server, 200, "{}").await;
    mount_web_ok(&server, &base).await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;
    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));

    let views = exec.accounts();
    let view = views.iter().find(|v| v.uid == "u-chat").expect("账号视图");
    let stamp = view
        .last_daily_chat
        .clone()
        .expect("视图里的 last_daily_chat 应已刷新");
    // 落盘格式是 `%Y-%m-%d %H:%M:%S`；前 10 位即当天日期。
    let today = &stamp[..10];
    assert!(
        !view.can_daily_chat(today),
        "今天已做过时不得再排一次每日活跃"
    );
}

// ---------------------------------------------------------------------------
// 11：数字型会话 id
//
// Python `(payload.get("data") or {}).get("id")` 不限定类型，URL 里用
// `quote(str(conversation))`；Rust 以前只认字符串，数字 id 会被当成「没有 id」，
// 整条网页腿直接失败——上游一旦改发数字，积分就整天拿不到。
// ---------------------------------------------------------------------------

#[tokio::test]
async fn numeric_conversation_id_is_accepted() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_desktop(&server, 200, "{}").await;
    mount_web_ok_with(&server, &base, "987654", json!(987654)).await;

    let dir = account_dir(None);
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    let web = res.get("web").expect("web 段");
    assert_eq!(
        web.get("ok").and_then(Value::as_bool),
        Some(true),
        "数字型 id 必须被接受：{web}"
    );
    // Python 把原值塞回结果，数字就是数字。
    assert_eq!(
        web.get("conversation").and_then(Value::as_i64),
        Some(987654)
    );
    assert!(read_last_daily_chat(&accounts_dir(dir.path())).is_some());
}

// ---------------------------------------------------------------------------
// 12：`settings.json` 损坏 → 按默认「开网页腿」处理
//
// Python `wb_settings.load` 读坏 JSON 时返回 `{}` ⇒ `daily_chat_web` 为 True。
// 读坏就当「关」的话，等于静默关掉唯一能拿到积分的那条腿。
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_settings_json_falls_back_to_web_leg() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_desktop(&server, 200, "{}").await;
    mount_web_ok(&server, &base).await;

    let dir = account_dir_with_raw_settings("{ this is not json");
    let exec = build_exec(&server, "u-chat", "intl", &accounts_dir(dir.path()));
    let res = exec.daily_chat(&acc_view("u-chat", "intl")).await;

    assert_eq!(res.get("ok").and_then(Value::as_bool), Some(true));
    assert!(
        res.get("web").is_some(),
        "settings.json 损坏时按默认「开网页腿」处理"
    );
}

/// 账号目录：tempdir 内的 `accounts/` 子目录（并建出来）。
///
/// 活动历史与用量目录都是从**账号目录的同级**目录推导的（`usage/` 与
/// `accounts/` 同级，对齐 Python `wb_proxy.py:402-403`）。直接用 tempdir 根
/// 目录 ⇒ 同级目录变成 `/tmp` ⇒ 测试会往 tempdir 外面写（`/tmp/usage`）。
fn accounts_dir(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("accounts");
    let _ = std::fs::create_dir_all(&path);
    path
}
