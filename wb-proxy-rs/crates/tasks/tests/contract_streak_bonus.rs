//! 关卡 4.17：连登管家（`wb_tasks::streak_bonus`）四步契约测试。
//!
//! 权威源：`wb_tasks.py:771-879`（`run_streak_bonus`）。
//!
//! 全程 wiremock + 两个 mock 上游（chat 腿 / billing 腿），**不向真实上游发
//! 任何请求**。
//!
//! ## 为什么整份重写
//!
//! 移植前本文件测的是 `POST {chat_base}/activity/growth/streak/claim` ——
//! **这个端点 Python 从不调用**（`grep -rn "streak/claim" --include=*.py .`
//! 为空），所以那一版 Rust 连登管家是打在一个不存在的端点上，功能从来没
//! 真正工作过。本文件改为覆盖 Python 真实的四步：补签保连登 → 礼包/补偿 →
//! 7d/14d/28d 档位兑换 → 自动抽奖闭环。
//!
//! `api_routes` 层的接线（账号查找 → 本模块）由主会话在集成时补，本文件只
//! 覆盖模块自身语义。
//!
//! ## 挂载约定
//!
//! wiremock 对同一请求**先挂载的 mock 优先**，所以每个测试只挂自己需要的
//! 端点（不设「默认全挂」），避免出现「后挂的概览被先挂的覆盖」这种假绿/假红。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_gateway::outbound::UpstreamClient;
use wb_tasks::growth_runner::{Clock, NoopSleeper};
use wb_tasks::streak_bonus::{
    run_streak_bonus, StreakBonusAccount, StreakBonusConfig, StreakBonusCtx, MSG_CN_ONLY,
    MSG_ENTERPRISE_SKIP, MSG_IDLE,
};

/// 固定「今天」= 2026-10-20，故「昨天」= 2026-10-19。
const TODAY: &str = "2026-10-20 08:00:00";
const YESTERDAY: &str = "2026-10-19";

struct FixedClock(&'static str);

impl Clock for FixedClock {
    fn local_hour(&self) -> u32 {
        8
    }
    fn local_now(&self) -> String {
        self.0.to_string()
    }
}

fn chat_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("Authorization".into(), "Bearer chat-tok".into());
    h
}

fn billing_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("Authorization".into(), "Bearer bill-tok".into());
    h.insert("X-Product".into(), "SaaS".into());
    h
}

/// 指向两个 mock 上游的配置。`client_token` 用计数器，便于断言每次请求的
/// 令牌互不相同。
fn config<'a>(chat: &'a str, bill: &'a str) -> StreakBonusConfig<'a> {
    let mut n = 0u32;
    StreakBonusConfig {
        chat_base: chat,
        bill_base: bill,
        timeout: Duration::from_secs(5),
        draw_interval: 1.0,
        client_token: Box::new(move || {
            n += 1;
            format!("tok-{n}")
        }),
    }
}

fn account<'a>(
    realm: &'a str,
    enterprise_id: &'a str,
    chat: &'a HashMap<String, String>,
    bill: &'a HashMap<String, String>,
) -> StreakBonusAccount<'a> {
    StreakBonusAccount {
        realm,
        enterprise_id,
        chat_headers: chat,
        billing_headers: bill,
    }
}

/// 已记录的请求摘要：`"GET /path"` / `"POST /path"`。
async fn calls(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method.as_str(), r.url.path()))
        .collect()
}

/// 已记录的请求体（逐个解析为 JSON）。
async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap_or(Value::Null))
        .collect()
}

// ---------------------------------------------------------------------------
// mock 上游装配（按路径参数化）
// ---------------------------------------------------------------------------

/// `GET /activity/growth/heatmap`：昨天格子的 score。
async fn mount_heatmap(server: &MockServer, yesterday_score: i64) {
    Mock::given(method("GET"))
        .and(path("/activity/growth/heatmap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"cells": [{"date": YESTERDAY, "score": yesterday_score}]},
        })))
        .mount(server)
        .await;
}

/// `GET /activity/growth/streak`：补签卡余额 + 兑换状态。
async fn mount_streak(server: &MockServer, cards: i64, redemption: Value) {
    Mock::given(method("GET"))
        .and(path("/activity/growth/streak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {
                "makeup_cards": {"balance": cards},
                "redemption_status": redemption,
            },
        })))
        .mount(server)
        .await;
}

/// `GET /activity/growth/lottery/summary`：抽奖次数。
async fn mount_lottery_summary(server: &MockServer, chances: i64) {
    Mock::given(method("GET"))
        .and(path("/activity/growth/lottery/summary"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"code": 0, "data": {"chances": chances}})),
        )
        .mount(server)
        .await;
}

/// `POST /activity/growth/makeup-cards/use`：消耗一张补签卡。
async fn mount_makeup_use(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/activity/growth/makeup-cards/use"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(server)
        .await;
}

/// `POST /activity/growth/redeem`：档位兑换成功。
async fn mount_redeem(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/activity/growth/redeem"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(server)
        .await;
}

/// `POST /activity/growth/lottery/draw`：抽中。
async fn mount_draw_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/activity/growth/lottery/draw"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .mount(server)
        .await;
}

/// `POST /activity/growth/lottery/draw`：抽失败（HTTP 500）。
async fn mount_draw_fail(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/activity/growth/lottery/draw"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(server)
        .await;
}

/// billing 腿：礼包与补偿都领到（`code == 0` + credit）。
async fn mount_bill_taken(server: &MockServer, gift: i64, compensation: i64) {
    for (p, credit) in [
        ("/billing/meter/claim-gift", gift),
        ("/billing/meter/claim-compensation", compensation),
    ] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": credit}})),
            )
            .mount(server)
            .await;
    }
}

/// billing 腿：业务错误（已领过），静默跳过。
async fn mount_bill_idle(server: &MockServer) {
    for p in [
        "/billing/meter/claim-gift",
        "/billing/meter/claim-compensation",
    ] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 1})))
            .mount(server)
            .await;
    }
}

/// chat 腿的「什么都没做」三件套：昨天已签、无补签卡、无抽奖次数。
async fn mount_chat_quiet(server: &MockServer) {
    mount_heatmap(server, 10).await;
    mount_streak(server, 0, json!({"tiers": []})).await;
    mount_lottery_summary(server, 0).await;
}

// ---------------------------------------------------------------------------
// 门禁
// ---------------------------------------------------------------------------

/// 非国内版账号：直接拒绝，返回体**没有 `logs` 键**，且不发任何上游请求。
#[tokio::test]
async fn non_cn_realm_is_rejected_without_logs_key() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("intl", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;
    let body = out.to_json();

    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["msg"], json!(MSG_CN_ONLY));
    assert!(body.get("logs").is_none(), "门禁早退时不得有 logs 键");
    assert!(
        calls(&chat).await.is_empty() && calls(&bill).await.is_empty(),
        "门禁拦截时不得发上游请求"
    );
}

/// 企业版账号：跳过（ok=true），同样**没有 `logs` 键**、不发请求。
#[tokio::test]
async fn enterprise_account_is_skipped_without_logs_key() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "ent-123", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;
    let body = out.to_json();

    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["msg"], json!(MSG_ENTERPRISE_SKIP));
    assert!(body.get("logs").is_none(), "门禁早退时不得有 logs 键");
    assert!(
        calls(&chat).await.is_empty() && calls(&bill).await.is_empty(),
        "企业版门禁不得发上游请求"
    );
}

// ---------------------------------------------------------------------------
// 第 0 步：补签保连登
// ---------------------------------------------------------------------------

/// 昨天漏签（score==0）且有补签卡 ⇒ 消耗一张卡并记日志。
#[tokio::test]
async fn makeup_card_is_used_for_a_missed_yesterday() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 0).await;
    mount_streak(&chat, 2, json!({"tiers": []})).await;
    mount_lottery_summary(&chat, 0).await;
    mount_makeup_use(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert_eq!(
        out.logs,
        Some(vec![format!("已消耗 1 张补签卡补签昨日 ({YESTERDAY})")]),
        "补签日志文案必须与 Python 逐字一致"
    );
    let chat_calls = calls(&chat).await;
    assert!(
        chat_calls.contains(&"POST /activity/growth/makeup-cards/use".to_string()),
        "必须调用补签卡端点，实际：{chat_calls:?}"
    );
    // 请求体是 {"target_date": "YYYY-MM-DD"}
    let bodies = bodies(&chat).await;
    let use_body = bodies
        .iter()
        .find(|b| b.get("target_date").is_some())
        .expect("补签请求体");
    assert_eq!(use_body["target_date"], json!(YESTERDAY));
}

/// 昨天已签（score!=0）⇒ 不消耗补签卡。
#[tokio::test]
async fn signed_yesterday_does_not_consume_a_makeup_card() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_chat_quiet(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert!(!calls(&chat)
        .await
        .contains(&"POST /activity/growth/makeup-cards/use".to_string()));
    assert_eq!(out.msg, MSG_IDLE);
}

/// 昨天漏签但**没有补签卡** ⇒ 不消耗（也就没有补签日志）。
#[tokio::test]
async fn missing_makeup_card_consumes_nothing() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 0).await;
    mount_streak(&chat, 0, json!({"tiers": []})).await;
    mount_lottery_summary(&chat, 0).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert!(!calls(&chat)
        .await
        .contains(&"POST /activity/growth/makeup-cards/use".to_string()));
    assert_eq!(out.logs, Some(vec![]));
}

// ---------------------------------------------------------------------------
// 第 1 步：新手礼包 + 活动补偿（billing 腿）
// ---------------------------------------------------------------------------

/// 两个 billing 端点都领到 ⇒ 两条日志；请求体是空对象 `{}`。
///
/// 同时验证 **chat 腿与 billing 腿用的是不同的 base**（两个独立 mock 上游）。
#[tokio::test]
async fn gift_and_compensation_are_claimed_from_the_billing_base() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_chat_quiet(&chat).await;
    mount_bill_taken(&bill, 100, 50).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert_eq!(
        out.logs,
        Some(vec![
            "领取新手礼包 +100积分".to_string(),
            "领取活动补偿 +50积分".to_string(),
        ]),
        "礼包/补偿日志文案必须与 Python 逐字一致"
    );
    // 两个 billing 请求体都是 {}
    for b in bodies(&bill).await {
        assert_eq!(b, json!({}), "billing 请求体必须是空对象");
    }
    // 聊天腿一个 billing 请求都没有
    assert!(
        !calls(&chat).await.iter().any(|c| c.contains("/billing/")),
        "billing 请求不得打到 chat base"
    );
}

// ---------------------------------------------------------------------------
// 第 2 步：7d / 14d / 28d 档位兑换
// ---------------------------------------------------------------------------

/// `locked` / `claimed` 的档位跳过；其余档位逐个兑换。
#[tokio::test]
async fn locked_and_claimed_tiers_are_skipped() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 10).await;
    mount_streak(
        &chat,
        0,
        json!({
            "tiers": [{"tier": "7d"}, {"tier": "14d"}, {"tier": "28d"}],
            "tier_7d_status": "locked",
            "tier_14d_status": "claimed",
            "tier_28d_status": "unlocked",
        }),
    )
    .await;
    mount_lottery_summary(&chat, 0).await;
    mount_redeem(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert_eq!(out.logs, Some(vec!["成功兑换连登 28d 档位".to_string()]));
    let redeem_bodies: Vec<Value> = bodies(&chat)
        .await
        .into_iter()
        .filter(|b| b.get("tier").is_some())
        .collect();
    assert_eq!(redeem_bodies.len(), 1, "只有 28d 档位该被兑换");
    assert_eq!(redeem_bodies[0]["tier"], json!("28d"));
    assert!(
        redeem_bodies[0].get("client_token").is_some(),
        "兑换请求体必须带 client_token"
    );
}

/// 档位状态缺失（既非 locked 也非 claimed）⇒ 仍然尝试兑换。
#[tokio::test]
async fn tier_without_status_is_still_attempted() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 10).await;
    mount_streak(&chat, 0, json!({"tiers": [{"tier": "7d"}]})).await;
    mount_lottery_summary(&chat, 0).await;
    mount_redeem(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert_eq!(out.logs, Some(vec!["成功兑换连登 7d 档位".to_string()]));
}

// ---------------------------------------------------------------------------
// 第 3 步：自动抽奖闭环
// ---------------------------------------------------------------------------

/// 有 3 次机会就抽 3 次；每次抽完 sleep 1.0（含最后一次）。
#[tokio::test]
async fn lottery_draws_every_chance_and_sleeps_between_draws() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 10).await;
    mount_streak(&chat, 0, json!({"tiers": []})).await;
    mount_lottery_summary(&chat, 3).await;
    mount_draw_ok(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert_eq!(out.logs, Some(vec!["完成成长抽奖 3 次".to_string()]));
    let draws = calls(&chat)
        .await
        .iter()
        .filter(|c| *c == "POST /activity/growth/lottery/draw")
        .count();
    assert_eq!(draws, 3, "有 3 次机会就要抽 3 次");
    assert_eq!(
        sleeper.calls(),
        vec![1.0, 1.0, 1.0],
        "Python 每次迭代末尾都 sleep 1.0（含最后一次）"
    );
    // 每次抽的 client_token 都不同
    let tokens: Vec<String> = bodies(&chat)
        .await
        .into_iter()
        .filter_map(|b| {
            b.get("client_token")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(tokens.len(), 3);
    let mut uniq = tokens.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 3, "每次抽奖的 client_token 必须互不相同");
}

/// 抽失败 ⇒ 中断循环（不再抽、不再 sleep），且不计入成功次数。
#[tokio::test]
async fn lottery_stops_at_the_first_failed_draw() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 10).await;
    mount_streak(&chat, 0, json!({"tiers": []})).await;
    mount_lottery_summary(&chat, 3).await;
    mount_draw_fail(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    let draws = calls(&chat)
        .await
        .iter()
        .filter(|c| *c == "POST /activity/growth/lottery/draw")
        .count();
    assert_eq!(draws, 1, "抽失败后必须中断，不再抽第二次");
    assert!(
        sleeper.calls().is_empty(),
        "break 之后不再 sleep（Python 的 sleep 在 try 之后）"
    );
    assert_eq!(out.logs, Some(vec![]), "一次都没抽成功 ⇒ 不记抽奖日志");
    assert!(
        !out.warnings.is_empty(),
        "失败原因必须可见（Python 写 _log，这里进 warnings）"
    );
}

// ---------------------------------------------------------------------------
// 汇总与失败可见性
// ---------------------------------------------------------------------------

/// 什么都没做 ⇒ `logs` 是空数组，`msg` 是「连登状态正常」文案。
#[tokio::test]
async fn idle_message_when_nothing_to_do() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_chat_quiet(&chat).await;
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;
    let body = out.to_json();

    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["logs"], json!([]));
    assert_eq!(body["msg"], json!(MSG_IDLE));
}

/// 多条日志用 `; ` 连接（Python `"; ".join(logs)`）。
#[tokio::test]
async fn msg_joins_all_logs_with_semicolons() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    mount_heatmap(&chat, 10).await;
    mount_streak(&chat, 0, json!({"tiers": []})).await;
    mount_lottery_summary(&chat, 1).await;
    mount_draw_ok(&chat).await;
    mount_bill_taken(&bill, 100, 50).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;

    assert_eq!(
        out.msg,
        "领取新手礼包 +100积分; 领取活动补偿 +50积分; 完成成长抽奖 1 次"
    );
}

/// 上游整体挂掉：返回体仍是 `ok:true`（Python 同），失败原因进 `warnings`，
/// **不进返回体**（Python 是 `_log`，面板日志里才看得到）。
#[tokio::test]
async fn upstream_failure_is_visible_in_warnings_not_in_body() {
    let chat = MockServer::start().await;
    let bill = MockServer::start().await;
    for p in [
        "/activity/growth/heatmap",
        "/activity/growth/streak",
        "/activity/growth/lottery/summary",
    ] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&chat)
            .await;
    }
    mount_bill_idle(&bill).await;

    let client = UpstreamClient::new();
    let ch = chat_headers();
    let bh = billing_headers();
    let acc = account("cn", "", &ch, &bh);
    let chat_uri = chat.uri();
    let bill_uri = bill.uri();
    let mut cfg = config(&chat_uri, &bill_uri);
    let clock = FixedClock(TODAY);
    let sleeper = NoopSleeper::default();
    let ctx = StreakBonusCtx {
        sleeper: &sleeper,
        clock: &clock,
    };

    let out = run_streak_bonus(&client, &acc, &mut cfg, &ctx).await;
    let body = out.to_json();

    assert_eq!(
        body["ok"],
        json!(true),
        "上游失败不改变 ok（与 Python 一致）"
    );
    assert_eq!(body["logs"], json!([]));
    assert_eq!(body["msg"], json!(MSG_IDLE));
    assert!(
        !out.warnings.is_empty(),
        "失败原因必须可见：{:?}",
        out.warnings
    );
    assert!(
        body.get("warnings").is_none(),
        "warnings 是 Rust 侧独有的，不得进入返回体"
    );
}
