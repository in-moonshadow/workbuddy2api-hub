//! 关卡 4.24（线 B）产出物：契约 —— 账户管理路由（api_routes.rs 的 16 条
//! `route_accounts_*`）。
//!
//! Python 权威源：wb_proxy.py `_route_accounts_*`（:17144-17860）。
//!
//! 覆盖：
//! 1. `/accounts/product` 身分切换（落盘 + 视图 + 非法值 + 空串恢复默认）。
//! 2. `/accounts/set` 的 enabled/weight/note 字段（落盘 + 校验）。
//! 3. `/accounts/set-all` 统一启停。
//! 4. `/accounts/delete` 删凭据档（WARN 走面板日志）。
//! 5. `/accounts/import` 的 dryRun / commit / overwrite / 文档内重复。
//! 6. `/accounts/export` 导出文档形状。
//! 7. `/accounts/credits/grants` 摊平表 + 汇总。
//! 8. `/accounts/test` 本地点检（不发上游）。
//! 9. `/accounts/sync-profile` 本地昵称读取。
//! 10. `/accounts/login/cancel` 与登录注册表 TTL。
//! 11. wiremock 模拟上游的 OAuth 三跳（auth/state → auth/token → login/account）——
//!     处理器层把 `chat_upstream` 定为 realm 常量（同 Python REALM_CONFIGS），
//!     无法注入 mock 基址，故网络边界按线 A 的 oauth.rs 契约函数级测；
//!     本文件用 wiremock 复测同一路径，确认与处理器契约常量一致。
//!
//! 全程 tempfile 真实账号目录 + wiremock mock 上游，**不向真实上游发任何请求**。

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;

use wb_tasks::api_routes::{
    route_accounts_credits_grants, route_accounts_delete, route_accounts_export,
    route_accounts_import, route_accounts_login_cancel, route_accounts_product, route_accounts_set,
    route_accounts_set_all, route_accounts_sync_profile, route_accounts_test, PooledTaskAccount,
    ReadyAccount, TaskRuntime,
};
use wb_tasks::daily_chat::DailyChatConfig;
use wb_tasks::growth_api::TaskEndpoints;
use wb_tasks::growth_runner::{LocalClock, RealSleeper};
use wb_tasks::scheduler::{AccountView, LocalScheduleClock, PanelSink};
use wb_tasks::snapshot_cache::SnapshotCache;

// ---------------------------------------------------------------------------
// 测试支架
// ---------------------------------------------------------------------------

struct NullPanel;
impl PanelSink for NullPanel {
    fn add_log_entry(&self, _msg: &str, _tag: &str) {}
}

/// 装配一个指向真实临时账号目录的运行时（不进池，直接给 ready 账号）。
fn build_runtime(dir: &Path, ready: Vec<ReadyAccount>) -> Arc<TaskRuntime> {
    Arc::new(TaskRuntime {
        accounts_dir: dir.to_path_buf(),
        usage_dir: dir.to_path_buf(),
        pool: vec![],
        pool_present: true,
        ready,
        cache: Arc::new(SnapshotCache::with_ttl(0.0)),
        billing_base: String::new(),
        endpoints: TaskEndpoints::production(),
        daily_chat_config: DailyChatConfig::default(),
        client: wb_gateway::outbound::UpstreamClient::new(),
        clock: Arc::new(LocalClock),
        schedule_clock: LocalScheduleClock,
        sleeper: RealSleeper,
        panel: Arc::new(NullPanel),
        exec: std::sync::OnceLock::new(),
        scheduler: std::sync::OnceLock::new(),
        credits_refresher: std::sync::OnceLock::new(),
        price_refresher: std::sync::OnceLock::new(),
        update_checker: std::sync::OnceLock::new(),
    })
}

/// 写一个账号凭据档并构造 ready 账号（`extra` 键并入文件 JSON）。
fn seed(dir: &Path, uid: &str, realm: &str, extra: Option<Value>) -> ReadyAccount {
    let name = format!("{uid}.json");
    let mut doc = json!({
        "uid": uid,
        "nickname": format!("nick-{uid}"),
        "realm": realm,
        "accessToken": format!("{uid}-token"),
        "product": "cli",
        "enabled": true,
        "weight": 1,
    });
    if let Some(Value::Object(map)) = extra {
        if let Some(obj) = doc.as_object_mut() {
            for (k, v) in map {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    std::fs::write(dir.join(&name), serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let mut state = wb_gateway::pool::AccountState::new(uid, realm);
    state.credential_file = name;
    state.access_token = format!("{uid}-token");
    state.enabled = doc.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    state.product = doc
        .get("product")
        .and_then(Value::as_str)
        .unwrap_or("workbuddy")
        .to_string();
    let view = AccountView::from_state(&state);
    ReadyAccount {
        state,
        view,
        creds: PooledTaskAccount {
            uid: uid.to_string(),
            token: format!("{uid}-token"),
            enterprise_id: String::new(),
            realm: realm.to_string(),
            chat_headers: Default::default(),
        },
        nickname: Some(format!("nick-{uid}")),
        device_token: None,
    }
}

/// 读账号文件（断言落盘用）。
fn read_doc(dir: &Path, file: &str) -> Value {
    let text = std::fs::read_to_string(dir.join(file)).expect("account file readable");
    serde_json::from_str(&text).expect("account file is valid JSON")
}

// ---------------------------------------------------------------------------
// 合成 JWT（与 identity/tests/common 同形状：只解码不验签）
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url(data: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let a = data[i];
        let b = data.get(i + 1).copied().unwrap_or(0);
        let c = data.get(i + 2).copied().unwrap_or(0);
        out.push(B64[((a >> 2) & 0x3f) as usize] as char);
        out.push(B64[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(B64[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        }
        if i + 2 < data.len() {
            out.push(B64[(c & 0x3f) as usize] as char);
        }
        i += 3;
    }
    out
}

fn jwt(iss: &str, sub: &str) -> String {
    let header = json!({"alg": "RS256", "typ": "JWT"});
    let claims = json!({"iss": iss, "sub": sub, "exp": 4102444800_u64});
    format!(
        "{}.{}.sig",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    )
}

// ---------------------------------------------------------------------------
// /accounts/product
// ---------------------------------------------------------------------------

#[tokio::test]
async fn product_switch_updates_file_and_views() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);
    let res = route_accounts_product(&rt, &json!({"uid": "u1", "product": "vscode"})).await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["product"], "vscode");
    assert_eq!(res["changed"], json!(["u1"]));
    assert_eq!(res["accounts"][0]["product"], "vscode");
    assert_eq!(read_doc(dir.path(), "u1.json")["product"], "vscode");
}

#[tokio::test]
async fn product_rejects_unknown_and_restores_default_on_empty() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);
    let res = route_accounts_product(&rt, &json!({"uid": "u1", "product": "nope"})).await;
    assert_eq!(res["ok"], false);
    // 空串 = 恢复默认桌面端
    let res = route_accounts_product(&rt, &json!({"uid": "u1", "product": ""})).await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["product"], "workbuddy");
    assert_eq!(read_doc(dir.path(), "u1.json")["product"], "workbuddy");
}

// ---------------------------------------------------------------------------
// /accounts/set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_enabled_weight_note_persist_and_validate() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);

    let res = route_accounts_set(&rt, &json!({"uid": "u1", "enabled": false})).await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["account"]["enabled"], false);
    assert_eq!(read_doc(dir.path(), "u1.json")["enabled"], false);

    let res = route_accounts_set(&rt, &json!({"uid": "u1", "weight": 42})).await;
    assert_eq!(res["ok"], true);
    // Rust 的 public_view 不含 weight 键（Python public() 有）——落盘为准
    assert_eq!(read_doc(dir.path(), "u1.json")["weight"], 42);

    let _res = route_accounts_set(&rt, &json!({"uid": "u1", "note": "hello"})).await;
    assert_eq!(read_doc(dir.path(), "u1.json")["note"], "hello");

    let res = route_accounts_set(&rt, &json!({"uid": "u1", "weight": 101})).await;
    assert_eq!(res["ok"], false);

    let res = route_accounts_set(&rt, &json!({"uid": "missing"})).await;
    assert_eq!(res["ok"], false);

    let res = route_accounts_set(&rt, &json!({"uid": "u1"})).await;
    assert_eq!(res["ok"], false);

    // 同载荷多字段：enabled + weight 一起改，视图与文件都要反映两者
    let res = route_accounts_set(&rt, &json!({"uid": "u1", "enabled": true, "weight": 7})).await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["account"]["enabled"], true);
    assert_eq!(read_doc(dir.path(), "u1.json")["enabled"], true);
    assert_eq!(read_doc(dir.path(), "u1.json")["weight"], 7);
}

// ---------------------------------------------------------------------------
// /accounts/set-all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_all_toggles_every_account() {
    let dir = TempDir::new().unwrap();
    let a = seed(dir.path(), "u1", "intl", None);
    let b = seed(dir.path(), "u2", "cn", None);
    let rt = build_runtime(dir.path(), vec![a, b]);

    let res = route_accounts_set_all(&rt, &json!({"enabled": false})).await;
    let views = res["accounts"].as_array().unwrap();
    assert_eq!(views.len(), 2);
    for v in views {
        assert_eq!(v["enabled"], false);
    }
    assert_eq!(read_doc(dir.path(), "u1.json")["enabled"], false);
    assert_eq!(read_doc(dir.path(), "u2.json")["enabled"], false);
}

// ---------------------------------------------------------------------------
// /accounts/delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_removes_file_and_reports() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);

    let res = route_accounts_delete(&rt, &json!({"uid": "u1"})).await;
    assert_eq!(res["deleted"], true);
    assert!(!dir.path().join("u1.json").exists());

    let res = route_accounts_delete(&rt, &json!({"uid": "nope"})).await;
    assert_eq!(res["ok"], false);
    let res = route_accounts_delete(&rt, &json!({})).await;
    assert_eq!(res["ok"], false);
}

// ---------------------------------------------------------------------------
// /accounts/import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_dry_run_then_commit_then_skip_then_overwrite() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);
    let token = jwt("https://www.workbuddy.ai", "u2");
    let row = json!({"accessToken": token, "uid": "u2", "realm": "intl", "nickname": "n2"});
    let body = json!({"data": row.clone(), "dryRun": true});

    let res = route_accounts_import(&rt, &body).await;
    assert_eq!(res["dryRun"], true);
    assert_eq!(res["result"]["added"][0], "u2");
    assert!(!dir.path().join("u2.json").exists(), "dryRun 不得落盘");

    let res = route_accounts_import(&rt, &json!({"data": row.clone()})).await;
    assert_eq!(res["result"]["added"][0], "u2");
    assert!(dir.path().join("u2.json").exists());
    assert_eq!(read_doc(dir.path(), "u2.json")["uid"], "u2");

    // 已存在且未 overwrite → skipped
    let res = route_accounts_import(&rt, &json!({"data": row.clone()})).await;
    assert_eq!(res["result"]["skipped"][0]["reason"], "already exists");
    assert_eq!(res["result"]["added"].as_array().unwrap().len(), 0);

    // overwrite → updated
    let res = route_accounts_import(&rt, &json!({"data": row.clone(), "overwrite": true})).await;
    assert_eq!(res["result"]["updated"][0], "u2");
}

#[tokio::test]
async fn import_invalid_row_and_duplicate_inside_document() {
    let dir = TempDir::new().unwrap();
    let rt = build_runtime(dir.path(), vec![]);
    // 无 accessToken → invalid
    let res = route_accounts_import(&rt, &json!({"data": [{"uid": "x1", "realm": "intl"}]})).await;
    assert_eq!(res["result"]["invalid"][0]["index"], 1);
    // 同一文档内两个相同 uid → 第二个 duplicate
    let token = jwt("https://www.workbuddy.ai", "x2");
    let body = json!({"data": [
        {"accessToken": token, "uid": "x2", "realm": "intl"},
        {"accessToken": token, "uid": "x2", "realm": "intl"},
    ]});
    let res = route_accounts_import(&rt, &body).await;
    assert_eq!(res["result"]["added"][0], "x2");
    assert_eq!(
        res["result"]["skipped"][0]["reason"],
        "duplicate inside the document"
    );
}

// ---------------------------------------------------------------------------
// /accounts/export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_returns_document_with_expected_envelope() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);
    let res = route_accounts_export(&rt, &json!({"uid": "u1", "secrets": true})).await;
    assert_eq!(res["exported"], true);
    assert_eq!(res["count"], 1);
    let doc = &res["account"];
    assert_eq!(doc["format"], "workbuddy-accounts");
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["accounts"][0]["uid"], "u1");
    assert!(doc["accounts"][0].get("accessToken").is_some());
}

// ---------------------------------------------------------------------------
// /accounts/credits/grants
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grants_flattens_local_snapshots() {
    let dir = TempDir::new().unwrap();
    let extra = json!({
        "credits": {
            "updated_at": 1750000000.0,
            "packages": [{
                "name": "P1", "size": 100.0, "remain": 60.0, "used": 40.0,
                "unit": "credit", "create_time": "2026-10-10 00:42:45",
                "expire_time": "2026-12-31 00:00:00", "days_left": 68
            }]
        }
    });
    let ra = seed(dir.path(), "u1", "intl", Some(extra));
    let rt = build_runtime(dir.path(), vec![ra]);
    let res = route_accounts_credits_grants(&rt, &json!({})).await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["rows"][0]["name"], "P1");
    assert_eq!(res["rows"][0]["status"], "available");
    assert!(res["rows"][0]["create_at"].is_number());
    assert_eq!(res["summary"]["count"], 1);
    assert_eq!(res["summary"]["size"], 100.0);
    assert_eq!(res["summary"]["remain"], 60.0);
    assert_eq!(res["fetched_at"], 1750000000.0);
}

// ---------------------------------------------------------------------------
// /accounts/test（本地点检，不发上游）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_checks_local_account_state() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);
    let res = route_accounts_test(&rt, &json!({"uid": "u1", "model": "m1"})).await;
    assert_eq!(res["ok"], true);
    assert_eq!(res["checks"]["credentialFileExists"], true);
    assert_eq!(res["checks"]["enabled"], true);
    // 未知账号
    let res = route_accounts_test(&rt, &json!({"uid": "missing"})).await;
    assert_eq!(res["ok"], false);
}

// ---------------------------------------------------------------------------
// /accounts/sync-profile（本地昵称）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_profile_reads_local_nickname() {
    let dir = TempDir::new().unwrap();
    let ra = seed(dir.path(), "u1", "intl", None);
    let rt = build_runtime(dir.path(), vec![ra]);
    let res = route_accounts_sync_profile(&rt, &json!({"uid": "u1"})).await;
    assert_eq!(res["updated"][0]["nickname"], "nick-u1");
}

// ---------------------------------------------------------------------------
// /accounts/login/cancel + 登录注册表
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_cancel_and_registry_ttl() {
    let dir = TempDir::new().unwrap();
    let rt = build_runtime(dir.path(), vec![]);
    // 未登记 → false
    let res = route_accounts_login_cancel(&rt, &json!({"state": "s1"})).await;
    assert_eq!(res["cancelled"], false);
    // 登记后 → true
    wb_tasks::login::login_start_register("s2", "CLI", "intl", 0.0);
    assert!(wb_tasks::login::login_peek("s2").is_some());
    let res = route_accounts_login_cancel(&rt, &json!({"state": "s2"})).await;
    assert_eq!(res["cancelled"], true);
    assert!(wb_tasks::login::login_peek("s2").is_none());
    // TTL 过期自动移除
    wb_tasks::login::login_start_register("s3", "CLI", "intl", 1000.0);
    let got = wb_tasks::login::login_next("s3", 1000.0 + wb_tasks::login::LOGIN_TTL_SECONDS + 1.0);
    assert!(got.is_none());
    assert!(wb_tasks::login::login_peek("s3").is_none());
}

// ---------------------------------------------------------------------------
// wiremock：模拟上游的 OAuth 三跳（处理器层无法注入 mock 基址，此处按
// 线 A 的 oauth.rs 契约函数级复测，确认与本文件处理器用的常量一致）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oauth_three_hop_flow_against_wiremock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let base = server.uri();

    // 1. auth/state → {data:{state, authUrl}}
    Mock::given(method("POST"))
        .and(path("/v2/plugin/auth/state"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"state": "st-1", "authUrl": "https://sso.example/auth?state=st-1"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    // 2. auth/token：第一次 LOGIN_PENDING，第二次 ok（同一 mock，闭包计数应答）
    let token_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let token_calls_2 = token_calls.clone();
    Mock::given(method("GET"))
        .and(path("/v2/plugin/auth/token"))
        .respond_with(move |_req: &wiremock::Request| {
            let n = token_calls_2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200).set_body_json(json!({
                    "code": 11217, "msg": "waiting for browser login"
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "code": 0,
                    "data": {"accessToken": jwt("https://www.workbuddy.ai", "u9"), "refreshToken": "rt"}
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    // 3. login/account → {data:{nickname}}
    Mock::given(method("GET"))
        .and(path("/v2/plugin/login/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"nickname": "wiremock-nick"}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = wb_gateway::outbound::UpstreamClient::new();
    let ua = "WorkBuddy/5.5.2";
    let origin = "https://www.workbuddy.ai";

    // 第一步：start（处理器把 chat_upstream 定为 realm 常量，故这里直接喂 mock 基址）
    let info = wb_gateway::oauth::start_oauth_state(&client, &base, "CLI", ua, origin)
        .await
        .expect("start");
    assert_eq!(info.state, "st-1");
    wb_tasks::login::login_start_register(&info.state, "CLI", "intl", 0.0);

    // 第二步：pending
    let pending = wb_gateway::oauth::poll_oauth_token(&client, &base, &info.state, ua, origin)
        .await
        .unwrap();
    assert_eq!(pending["code"], 11217);
    // 第三次（mock 的第二个 token 应答）→ ok
    let ok = wb_gateway::oauth::poll_oauth_token(&client, &base, &info.state, ua, origin)
        .await
        .unwrap();
    assert_eq!(ok["code"], 0);

    // 第三步：profile
    let profile =
        wb_gateway::oauth::fetch_login_account(&client, &base, &info.state, "tok", ua, origin)
            .await
            .unwrap();
    assert_eq!(profile["data"]["nickname"], "wiremock-nick");

    // 处理器与 oauth 契约的常量一致
    assert_eq!(
        wb_tasks::login::LOGIN_PENDING,
        wb_gateway::oauth::LOGIN_PENDING
    );
}
