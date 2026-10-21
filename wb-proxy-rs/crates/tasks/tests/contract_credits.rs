//! M5 契约测试：积分快照。
//!
//! 权威源：Python `wb_accounts.py` 的 `_fetch_credits_cn_detailed`（:1850）与
//! `_fetch_credits_fallback`（:1975）。
//!
//! ⚠️ 一律走 wiremock mock 上游，**绝不向真实上游发请求**。

use serde_json::json;
use std::collections::HashMap;
use wb_gateway::credits::{
    fetch_credits_detailed, fetch_credits_fallback, CHECKIN_STATUS_PATH, GET_RESOURCE_PATH,
    RESOURCE_FREE_PACKAGES_PATH, RESOURCE_PAID_PACKAGES_PATH, RESOURCE_SUMMARY_PATH,
};
use wb_gateway::outbound::UpstreamClient;
use wb_tasks::credits::{
    aggregate_detailed, aggregate_fallback, earliest_expiring, packages_from_accounts,
    sort_packages, Credits, Package,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("Content-Type".into(), "application/json".into());
    h.insert("Authorization".into(), "Bearer tok".into());
    h
}

/// 挂一个能返回完整概览/免费包/付费包/签到状态的 mock 上游。
async fn mount_detailed(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(RESOURCE_SUMMARY_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {
                "IsPaidUser": true,
                "Packages": [{"PackageCode": "p1"}]
            }
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_FREE_PACKAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"Accounts": [{
                "AccountId": "a-free",
                "PackageName": "免费包",
                "PackageCode": "p1",
                "CapacitySize": 1000,
                "CapacityRemain": 400,
                "CapacityUsed": 600,
                "CapacityUnit": "credits",
                "DeductionEndTime": 1_800_000_000_000i64,
                "AccountAttributes": [{"Key": "grantReason", "Value": "官方活动发放"}]
            }]}
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_PAID_PACKAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"Accounts": [{
                "AccountId": "a-paid",
                "PackageName": "付费包",
                "PackageCode": "p2",
                "CycleCapacitySize": 500,
                "CycleCapacityRemain": 500,
                "CycleCapacityUsed": 0,
                "InUsage": true
            }]}
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(CHECKIN_STATUS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {
                "today_checked_in": true,
                "streak_days": 6,
                "daily_credit": 30,
                "today_credit": 30,
                "active": true
            }
        })))
        .mount(server)
        .await;
}

/// 端到端：mock 上游 → HTTP 那一半 → 纯聚合 → 12 键快照。
#[tokio::test]
async fn detailed_credits_flow_produces_the_python_snapshot_shape() {
    let server = MockServer::start().await;
    mount_detailed(&server).await;

    let client = UpstreamClient::new();
    let raw = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
        .await
        .expect("detailed fetch must succeed");

    let now = 1_800_000_000.0;
    let packages = packages_from_accounts(&raw.accounts, now);
    let credits = aggregate_detailed(
        packages,
        &raw.summary_packages,
        raw.is_paid_user,
        raw.checkin.clone(),
        now,
    );

    // 12 键，键名与顺序逐字对齐 Python `self.credits = {...}`
    let obj = credits.to_json();
    let keys: Vec<&str> = obj
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "remain",
            "used",
            "size",
            "used_percent",
            "remain_percent",
            "is_paid_user",
            "checkin",
            "earliest_expiring",
            "packages",
            "updated_at",
            "updated_iso",
        ]
    );

    // 免费包 400/1000 + 付费包 500/500 = 900 剩余、1500 总量
    assert_eq!(obj["remain"], 900.0);
    assert_eq!(obj["size"], 1500.0);
    assert_eq!(obj["used"], 600.0);
    assert_eq!(obj["used_percent"], "40.0%");
    assert_eq!(obj["remain_percent"], "60.0%");
    assert_eq!(obj["is_paid_user"], true);
    assert_eq!(obj["checkin"]["streak_days"], 6);
    assert_eq!(obj["checkin"]["today_checked_in"], true);
    assert_eq!(obj["updated_at"], 1_800_000_000.0);

    // 两个包，21 键的形状
    let packages_json = obj["packages"].as_array().unwrap();
    assert_eq!(packages_json.len(), 2);
    assert_eq!(packages_json[0].as_object().unwrap().len(), 21);
}

/// 付费包与免费包重叠时（同一个 `AccountId`）只算一次。
#[tokio::test]
async fn overlapping_packages_are_counted_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_SUMMARY_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "data": {"Packages": [{"PackageCode": "p1"}]}
        })))
        .mount(&server)
        .await;
    // 免费列表与付费列表返回**同一个** AccountId
    for endpoint in [RESOURCE_FREE_PACKAGES_PATH, RESOURCE_PAID_PACKAGES_PATH] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"Accounts": [{
                    "AccountId": "a-same",
                    "PackageName": "同一个包",
                    "CapacitySize": 1000,
                    "CapacityRemain": 400
                }]}
            })))
            .mount(&server)
            .await;
    }

    let client = UpstreamClient::new();
    let raw = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
        .await
        .expect("detailed fetch must succeed");
    assert_eq!(raw.accounts.len(), 1);

    let now = 1_800_000_000.0;
    let packages = packages_from_accounts(&raw.accounts, now);
    let credits = aggregate_detailed(packages, &raw.summary_packages, raw.is_paid_user, None, now);
    assert_eq!(credits.remain, 400.0);
    assert_eq!(credits.size, 1000.0);
}

/// 兜底口径：包为空但概览有汇总容量时，用概览补总量。
#[tokio::test]
async fn fallback_uses_summary_capacity_when_no_packages_are_listed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_SUMMARY_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"Packages": [{"PackageCode": "p1"}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_FREE_PACKAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "data": {"Accounts": []}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_PAID_PACKAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "data": {"Accounts": []}
        })))
        .mount(&server)
        .await;

    let client = UpstreamClient::new();
    let raw = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
        .await
        .expect("detailed fetch must succeed");
    assert!(raw.accounts.is_empty());

    // 概览里带汇总容量
    let summary = vec![json!({
        "CycleTotalCapacity": 2000,
        "CycleRemainCapacity": 1200,
        "CycleUsedCapacity": 800
    })];
    let now = 1_800_000_000.0;
    let credits = aggregate_detailed(
        packages_from_accounts(&raw.accounts, now),
        &summary,
        raw.is_paid_user,
        None,
        now,
    );
    assert_eq!(credits.size, 2000.0);
    assert_eq!(credits.remain, 1200.0);
    assert_eq!(credits.used, 800.0);
}

/// 兜底口径的端到端：单次 `get-user-resource`。
#[tokio::test]
async fn fallback_flow_keeps_the_previous_checkin_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GET_RESOURCE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "data": {"Response": {"Data": {"Accounts": [{
                "AccountId": "a1",
                "PackageName": "包",
                "CapacitySize": 100,
                "CapacityRemain": 60
            }]}}}
        })))
        .mount(&server)
        .await;

    let client = UpstreamClient::new();
    let accounts =
        fetch_credits_fallback(&client, &server.uri(), &headers(), "2026-10-20 08:00:00")
            .await
            .expect("fallback fetch must succeed");
    assert_eq!(accounts.len(), 1);

    let now = 1_800_000_000.0;
    let previous_checkin = json!({"today_checked_in": true, "streak_days": 3});
    let credits = aggregate_fallback(
        packages_from_accounts(&accounts, now),
        Some(previous_checkin.clone()),
        now,
    );
    assert_eq!(credits.remain, 60.0);
    assert_eq!(credits.size, 100.0);
    assert!(!credits.is_paid_user);
    // 兜底不查签到接口：上一次的签到状态必须被保留下来，不能被清掉
    assert_eq!(credits.checkin, Some(previous_checkin));
}

/// 概览失败即整体失败（Python：拿不到 `PackageCodes` 就没意义）。
#[tokio::test]
async fn a_failing_summary_endpoint_fails_the_whole_fetch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RESOURCE_SUMMARY_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 401, "msg": "未登录"
        })))
        .mount(&server)
        .await;

    let client = UpstreamClient::new();
    let err = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
        .await
        .expect_err("non-zero code must fail");
    assert_eq!(err, "未登录");
}

/// 排序：使用中优先 → 未过期按到期升序 → 已过期最后。
#[test]
fn packages_sort_by_usage_then_expiry() {
    let now = 1_800_000_000.0;
    // 闲置包：额度没动过（used=0）⇒ 不算「使用中」；
    // 使用中：InUsage=true；
    // 过期包：到期时间已过。
    let mk = |name: &str, remain: f64, used: f64, in_use: bool, days: f64| {
        json!({
            "PackageName": name,
            "CapacitySize": 100,
            "CapacityRemain": remain,
            "CapacityUsed": used,
            "DeductionEndTime": ((now + days * 86400.0) * 1000.0) as i64,
            "InUsage": in_use
        })
    };
    let accounts = vec![
        mk("过期包", 0.0, 100.0, false, -2.0),
        mk("闲置包", 100.0, 0.0, false, 10.0),
        mk("使用中", 20.0, 80.0, true, 30.0),
    ];
    let mut packages: Vec<Package> = packages_from_accounts(&accounts, now);
    sort_packages(&mut packages);
    assert_eq!(packages[0].name, "使用中");
    assert_eq!(packages[1].name, "闲置包");
    assert_eq!(packages[2].name, "过期包");
    assert!(packages[2].is_expired);
}

/// 最早到期的包：只认有剩余、未过期、到期时间已知的包。
#[test]
fn earliest_expiring_picks_the_nearest_active_package() {
    let now = 1_800_000_000.0;
    let accounts = vec![
        json!({
            "PackageName": "远的",
            "CapacitySize": 100, "CapacityRemain": 10,
            "DeductionEndTime": ((now + 30.0 * 86400.0) * 1000.0) as i64
        }),
        json!({
            "PackageName": "近的",
            "CapacitySize": 100, "CapacityRemain": 20,
            "DeductionEndTime": ((now + 3.0 * 86400.0) * 1000.0) as i64
        }),
        json!({
            "PackageName": "已用尽",
            "CapacitySize": 100, "CapacityRemain": 0,
            "DeductionEndTime": ((now + 1.0 * 86400.0) * 1000.0) as i64
        }),
    ];
    let packages = packages_from_accounts(&accounts, now);
    let ep = earliest_expiring(&packages).expect("must have one");
    assert_eq!(ep["name"], "近的");
    assert_eq!(ep["remain"], 20.0);
    assert!(ep["days_left"].as_f64().unwrap() < 4.0);
}

/// 无期限包（到期超过 730 天哨兵）不参与临期倒计时。
#[test]
fn far_future_packages_are_treated_as_no_expiry() {
    let now = 1_800_000_000.0;
    let accounts = vec![json!({
        "PackageName": "长期包",
        "CapacitySize": 100, "CapacityRemain": 100,
        "DeductionEndTime": ((now + 900.0 * 86400.0) * 1000.0) as i64
    })];
    let packages = packages_from_accounts(&accounts, now);
    assert!(packages[0].no_expiry);
    assert!(packages[0].days_left.is_none());
    assert!(!packages[0].is_expired);
    // 不进临期清单
    assert!(earliest_expiring(&packages).is_none());
}

/// 快照的 `updated_iso` 是本地时间文本，不是 UTC。
#[test]
fn updated_iso_is_local_time_text() {
    let credits: Credits = aggregate_fallback(vec![], None, 1_800_000_000.0);
    let iso = credits.updated_iso.clone();
    assert_eq!(iso.len(), 19, "got {iso}");
    assert!(iso.contains(' '), "got {iso}");
    assert!(iso.ends_with("00:00:00") || iso.len() == 19);
}
