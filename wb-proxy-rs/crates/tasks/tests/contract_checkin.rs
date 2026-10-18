//! 关卡 4.6 契约测试：签到 / 活跃上报的 **R4 修正**。
//!
//! 核心命题：**签到失败不能标记成已签**。上游未确认成功时不得写
//! `lastCheckin`，否则 `can_checkin()` 当天返回 false，账号白天再不会重试。
//!
//! 七条必测用例（对应计划「R4 修正规格」）：
//! 1. `code=1` → `ok:false` + 不写盘 + `can_checkin` 仍为 true
//! 2. `code=0` → `ok:true` + 写入今天的日期
//! 3. `code=10001` 正常 200 → `ok:true` + 写盘
//! 4. HTTP 400 + msg「今天已签到」→ `ok:true` + 写盘
//! 5. HTTP 400 + 其他 msg → `ok:false` + 不写盘
//! 6. 网络超时 → `ok:false` + 不写盘
//! 7. 落盘失败 → `ok:true` 且 `persist_error` 非空
//!
//! 另有守卫、`can_*` 谓词、billing 请求头、活跃上报与连登天数的补充用例。
//! 全程 wiremock + tempdir，**不向真实上游发任何请求**。

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_gateway::outbound::UpstreamClient;
use wb_tasks::checkin::{
    can_checkin, can_report_activity, checkin, report_activity, CheckinCtx, CHECKIN_PATH,
};
use wb_tasks::growth_api::{TaskAccount, TaskEndpoints};
use wb_tasks::growth_runner::{Clock, RunnerAccount};

// ---------------------------------------------------------------------------
// 测试替身
// ---------------------------------------------------------------------------

/// 固定时间的时钟：`local_now` 恒为 `2026-10-17 09:00:00`。
struct FixedClock;

impl Clock for FixedClock {
    fn local_hour(&self) -> u32 {
        9
    }
    fn local_now(&self) -> String {
        "2026-10-17 09:00:00".to_string()
    }
}

const TODAY: &str = "2026-10-17";
const ACCOUNT_NAME: &str = "acct.json";

/// 建账号文件（空 JSON 对象），返回目录句柄。
fn account_dir() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join(ACCOUNT_NAME), "{}").expect("seed account file");
    dir
}

fn read_field(dir: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(ACCOUNT_NAME)).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    doc.get(key).and_then(Value::as_str).map(str::to_string)
}

/// 读 `lastCheckin`（缺失 → None）。
fn read_last_checkin(dir: &Path) -> Option<String> {
    read_field(dir, "lastCheckin")
}

/// 读 `lastActivityReport`（缺失 → None）。
fn read_last_activity_report(dir: &Path) -> Option<String> {
    read_field(dir, "lastActivityReport")
}

/// 一个 cn、非企业的账号。
///
/// chat 头这里只放 `Authorization`：本文件的断言集中在**签到**的 billing 头，
/// 活跃上报走 `report_events`，其请求形状已由 4.3 契约测试覆盖，此处不重复。
fn cn_account() -> RunnerAccount<'static> {
    let mut h = HashMap::new();
    h.insert("Authorization".to_string(), "Bearer tok".to_string());
    let headers: &'static HashMap<String, String> = Box::leak(Box::new(h));
    RunnerAccount {
        api: TaskAccount {
            uid: "u-checkin",
            token: "tok",
            enterprise_id: "",
            chat_headers: headers,
        },
        realm: "cn",
        nickname: Some("tester"),
        is_enterprise: false,
    }
}

/// 客户端：`UpstreamClient::new()` 不持基址，URL 每次调用显式给出。
fn client_for(_server: &MockServer) -> UpstreamClient {
    UpstreamClient::new()
}

/// mock 上游基址，泄漏成 `&'static str`（测试进程短命，可接受）。
fn billing_base_for(server: &MockServer) -> &'static str {
    Box::leak(server.uri().into_boxed_str())
}

// ---------------------------------------------------------------------------
// 用例 1-3：正常 200 分支
// ---------------------------------------------------------------------------

/// ① `code=1` → `ok:false` + 不写盘 + 当天仍可重试。
#[tokio::test]
async fn rejected_checkin_does_not_mark_done_and_stays_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 1,
            "msg": "risk control",
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(!out.ok, "上游拒绝不得判成功");
    assert_eq!(out.code, Some(1));
    assert_eq!(out.msg.as_deref(), Some("risk control"));
    // R4 核心：不写盘
    assert_eq!(
        read_last_checkin(dir.path()),
        None,
        "失败不得写 lastCheckin"
    );
    // 可重试性
    assert!(
        can_checkin("cn", false, read_last_checkin(dir.path()).as_deref(), TODAY),
        "签到失败后当天必须仍可重试"
    );

    // 返回体形状：200 分支沿用 Python `{ok, code, msg, data}`，无 error / persist_error
    let v = out.to_json();
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], 1);
    assert_eq!(v["msg"], "risk control");
    assert!(v.get("error").is_none(), "200 分支不产出 error 键");
    assert!(v.get("persist_error").is_none());
}

/// ② `code=0` → `ok:true` + 写入今天的日期。
#[tokio::test]
async fn successful_checkin_persists_today() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "msg": "ok",
            "data": {"reward": 10},
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(out.ok);
    assert_eq!(out.code, Some(0));
    assert_eq!(out.data, Some(json!({"reward": 10})));
    assert_eq!(out.persist_error, None);
    assert_eq!(
        read_last_checkin(dir.path()).as_deref(),
        Some("2026-10-17 09:00:00"),
        "成功后写入本地时间戳"
    );
    // 签过之后当天不再可签
    assert!(!can_checkin(
        "cn",
        false,
        read_last_checkin(dir.path()).as_deref(),
        TODAY
    ));
}

/// ③ 正常 200 且 `code=10001` → `ok:true` + 写盘。
#[tokio::test]
async fn code_10001_on_200_counts_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 10001,
            "msg": "already",
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(out.ok);
    assert_eq!(out.code, Some(10001));
    assert_eq!(
        read_last_checkin(dir.path()).as_deref(),
        Some("2026-10-17 09:00:00")
    );
}

// ---------------------------------------------------------------------------
// 用例 4-5：非 2xx 分支
// ---------------------------------------------------------------------------

/// ④ HTTP 400 + msg「今天已签到」→ `ok:true` + 写盘。
#[tokio::test]
async fn http_400_already_checked_in_counts_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 1,
            "msg": "今天已签到",
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(out.ok, "「今天已签到」代表今天已完成，应判成功");
    assert_eq!(
        read_last_checkin(dir.path()).as_deref(),
        Some("2026-10-17 09:00:00")
    );
}

/// ⑤ HTTP 400 + 其他 msg → `ok:false` + 不写盘。
#[tokio::test]
async fn http_400_other_message_is_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 400,
            "msg": "bad request",
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(!out.ok);
    assert_eq!(out.code, Some(400));
    assert_eq!(out.error.as_deref(), Some("bad request"));
    assert_eq!(read_last_checkin(dir.path()), None, "HTTP 失败不得写盘");
    assert!(can_checkin("cn", false, None, TODAY));
}

// ---------------------------------------------------------------------------
// 用例 6：网络失败
// ---------------------------------------------------------------------------

/// ⑥ 连接失败（等价于超时 / 网络不可达）→ `ok:false` + 不写盘。
///
/// 端口由本测试**独占**：先绑一个本地 `TcpListener`，再让它在首个连接上
/// 立刻关闭连接（不发任何字节就 EOF）。这样客户端拿到的是「响应体为空/
/// 无法解析」而不是「连接被拒」，两者在 `checkin` 里都归为 `ok:false` +
/// `code:None` + 不写盘，且**不存在端口被其他并发用例抢走的竞态**。
///
/// 早先的写法是「启动 MockServer 拿到 URI 后立刻 drop」，端口随即回到
/// 操作系统的临时端口池，可能被并行运行的其它 `MockServer` 立刻复用 ——
/// 此时请求会打到一个真实的 mock 上并拿到 200 + `code`，用例间歇性失败。
#[tokio::test]
async fn network_failure_does_not_mark_done() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本地端口");
    let addr = listener.local_addr().expect("取本地地址");
    let base = format!("http://{addr}");

    // 接受一次连接后立即丢弃，客户端读到的就是 EOF（无响应体）。
    let acceptor = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let client = UpstreamClient::new();
    let out = checkin(&client, &acc, &base, &ctx).await;
    let _ = acceptor.await;

    assert!(!out.ok, "网络失败不得判成功");
    assert_eq!(out.code, None, "无响应时不应有 code");
    assert!(out.error.is_some(), "应带 error 文案");
    assert_eq!(read_last_checkin(dir.path()), None, "网络失败不得写盘");
    assert!(can_checkin("cn", false, None, TODAY));
}

/// ⑥b 非 2xx 且 body 不是 JSON → `ok:false` + 不写盘（`error` 回落 `HTTP <status>`）。
#[tokio::test]
async fn http_error_with_non_json_body_is_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(!out.ok);
    assert_eq!(out.code, None);
    assert_eq!(out.error.as_deref(), Some("HTTP 502"));
    assert_eq!(read_last_checkin(dir.path()), None);
}

// ---------------------------------------------------------------------------
// 用例 7：落盘失败
// ---------------------------------------------------------------------------

/// ⑦ 上游成功但账号文件不存在 → `ok:true` 且 `persist_error` 非空。
///
/// 不能因为「本地没写成」就把上游已完成的签到改判失败；也不能静默吞掉写盘错误。
#[tokio::test]
async fn persist_failure_keeps_ok_true_and_reports_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "msg": "ok",
        })))
        .mount(&server)
        .await;

    // 空目录：账号文件不存在 → write_idempotent_fields 报 NotFound
    let dir = TempDir::new().expect("tempdir");
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;

    assert!(out.ok, "落盘失败不得翻转签到结论");
    assert_eq!(out.code, Some(0));
    let err = out.persist_error.as_deref().expect("应有 persist_error");
    assert!(!err.is_empty(), "persist_error 必须非空");

    let j = out.to_json();
    assert_eq!(j["ok"], true);
    assert!(j.get("persist_error").is_some());
    assert_eq!(
        read_last_checkin(dir.path()),
        None,
        "文件不存在，自然读不到"
    );
}

// ---------------------------------------------------------------------------
// 守卫
// ---------------------------------------------------------------------------

/// 守卫：非 cn / 企业账号直接拒绝，且**不发请求**。
#[tokio::test]
async fn guards_reject_without_touching_upstream() {
    let server = MockServer::start().await;
    let dir = account_dir();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let client = client_for(&server);
    let base = billing_base_for(&server);

    let mut intl = cn_account();
    intl.realm = "intl";
    let out = checkin(&client, &intl, base, &ctx).await;
    assert!(!out.ok);
    assert_eq!(
        out.error.as_deref(),
        Some("checkin is only available for CN realm accounts")
    );

    let mut ent = cn_account();
    ent.is_enterprise = true;
    let out = checkin(&client, &ent, base, &ctx).await;
    assert!(!out.ok);
    assert_eq!(
        out.error.as_deref(),
        Some("enterprise accounts do not support checkin")
    );

    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "守卫命中时不得发任何上游请求"
    );
    assert_eq!(read_last_checkin(dir.path()), None);
}

// ---------------------------------------------------------------------------
// billing 请求头
// ---------------------------------------------------------------------------

/// 签到打的是 billing 头：Origin 为 codebuddy.cn、UA 是轻量 WorkBuddy/x.y.z、
/// `X-No-Enterprise-Id: 1`、cn 带 `X-Product: SaaS`，且**没有** chat 侧的 X-IDE-*。
#[tokio::test]
async fn checkin_uses_billing_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: Some("dev-token"),
    };
    let out = checkin(&client_for(&server), &acc, billing_base_for(&server), &ctx).await;
    assert!(out.ok);

    let reqs = server.received_requests().await.expect("requests");
    assert_eq!(reqs.len(), 1);
    let h = &reqs[0].headers;
    let get = |name: &str| h.get(name).map(|v| v.to_str().unwrap().to_string());

    assert_eq!(get("origin").as_deref(), Some("https://www.codebuddy.cn"));
    assert_eq!(get("referer").as_deref(), Some("https://www.codebuddy.cn/"));
    assert_eq!(get("user-agent").as_deref(), Some("WorkBuddy/5.5.6"));
    assert_eq!(get("x-domain").as_deref(), Some("copilot.tencent.com"));
    assert_eq!(get("accept-language").as_deref(), Some("zh-CN"));
    assert_eq!(get("x-codebuddy-request").as_deref(), Some("1"));
    assert_eq!(get("x-user-id").as_deref(), Some("u-checkin"));
    assert_eq!(get("x-no-enterprise-id").as_deref(), Some("1"));
    assert_eq!(get("x-product").as_deref(), Some("SaaS"));
    assert_eq!(get("x-device-token").as_deref(), Some("dev-token"));
    assert_eq!(get("authorization").as_deref(), Some("Bearer tok"));
    // chat 侧的身分头不得出现
    assert!(get("x-ide-type").is_none());
    assert!(get("x-agent-intent").is_none());
    // X-Request-ID = derive_id(uid,"req") + "-" + 6 位
    let rid = get("x-request-id").expect("x-request-id");
    assert_eq!(rid.len(), 39);
    assert_eq!(get("x-machine-id").map(|s| s.len()), Some(32));
    assert_eq!(get("x-session-id").map(|s| s.len()), Some(32));
    // body 是空对象
    assert_eq!(reqs[0].body, b"{}");
}

// ---------------------------------------------------------------------------
// can_* 谓词
// ---------------------------------------------------------------------------

#[test]
fn can_predicates_respect_realm_enterprise_and_today() {
    assert!(can_checkin("cn", false, None, TODAY));
    assert!(can_checkin("cn", false, Some("2026-10-16 23:59:59"), TODAY));
    assert!(!can_checkin(
        "cn",
        false,
        Some("2026-10-17 00:00:00"),
        TODAY
    ));
    assert!(!can_checkin("intl", false, None, TODAY));
    assert!(!can_checkin("cn", true, None, TODAY));

    assert!(can_report_activity("cn", false, None, TODAY));
    assert!(!can_report_activity(
        "cn",
        false,
        Some("2026-10-17 09:00:00"),
        TODAY
    ));
    assert!(!can_report_activity("intl", false, None, TODAY));
    assert!(!can_report_activity("cn", true, None, TODAY));
}

// ---------------------------------------------------------------------------
// 活跃上报
// ---------------------------------------------------------------------------

/// `TaskEndpoints` 持 `&'static str`：泄漏一份 mock 基址（测试进程短命，可接受）。
fn endpoints_for(server: &MockServer) -> TaskEndpoints<'static> {
    let base: &'static str = Box::leak(server.uri().into_boxed_str());
    TaskEndpoints {
        chat_base: base,
        web_base: base,
    }
}

/// 上报成功 → `ok:true` + 写 `lastActivityReport` + 回显连登天数。
#[tokio::test]
async fn report_activity_writes_timestamp_and_returns_streak() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/report"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/activity/growth/streak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"streak": {"days": 7}},
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let ep = endpoints_for(&server);
    let out = report_activity(&client_for(&server), &acc, &ep, &ctx, 1_700_000_000_000).await;

    assert_eq!(out["ok"], true);
    assert_eq!(out["streak_days"], 7);
    assert_eq!(out["msg"], "对话活跃上报成功（连续打卡 7 天）");
    assert_eq!(
        read_last_activity_report(dir.path()).as_deref(),
        Some("2026-10-17 09:00:00")
    );
}

/// 连登查询失败 → 仍是 `ok:true`，只把文案降级为「未知」。
#[tokio::test]
async fn report_activity_survives_streak_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/report"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/activity/growth/streak"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let ep = endpoints_for(&server);
    let out = report_activity(&client_for(&server), &acc, &ep, &ctx, 1_700_000_000_000).await;

    assert_eq!(out["ok"], true);
    assert_eq!(out["msg"], "对话活跃上报成功（连登天数未知）");
    assert!(out.get("streak_days").is_none(), "未知时不得编造天数");
    assert_eq!(
        read_last_activity_report(dir.path()).as_deref(),
        Some("2026-10-17 09:00:00")
    );
}

/// 连登为 0 → 保留 `Some(0)` 的「确实 0 天」语义，不并成未知。
#[tokio::test]
async fn report_activity_reports_zero_streak_explicitly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/report"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/activity/growth/streak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"streak": {"days": 0}},
        })))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let ep = endpoints_for(&server);
    let out = report_activity(&client_for(&server), &acc, &ep, &ctx, 1_700_000_000_000).await;

    assert_eq!(out["ok"], true);
    assert_eq!(out["streak_days"], 0);
    assert_eq!(
        out["msg"],
        "对话活跃上报成功，但连登仍是 0 天（可能被上游静默丢弃）"
    );
}

/// 上报被上游拒绝（`code != 0`）→ `ok:false` 且**不写** `lastActivityReport`。
#[tokio::test]
async fn report_activity_rejection_does_not_persist() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/report"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 5})))
        .mount(&server)
        .await;

    let dir = account_dir();
    let acc = cn_account();
    let ctx = CheckinCtx {
        dir: dir.path(),
        account_name: ACCOUNT_NAME,
        clock: &FixedClock,
        device_token: None,
    };
    let ep = endpoints_for(&server);
    let out = report_activity(&client_for(&server), &acc, &ep, &ctx, 1_700_000_000_000).await;

    assert_eq!(out["ok"], false);
    assert_eq!(out["error"], "活跃上报被上游拒绝（未返回 code=0）");
    assert_eq!(
        read_last_activity_report(dir.path()),
        None,
        "上报失败不得写 lastActivityReport"
    );
    assert!(can_report_activity("cn", false, None, TODAY));
}
