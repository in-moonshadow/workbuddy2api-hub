//! M5 数据侧：积分抓取的 **HTTP 那一半**（`wb-tasks::credits` 负责聚合）。
//!
//! 权威源：Python `wb_accounts.py`
//! - `_fetch_credits_cn_detailed`（:1850）：概览 → 免费包 → 付费包 → 签到状态
//! - `_fetch_credits_fallback`（:1975）：单次 `get-user-resource` 兜底
//!
//! 本模块**只负责取回上游原文**（`serde_json::Value`），不做字段归一化——
//! 归一化住在 `wb-tasks::credits`，那里才是纯数据层。
//!
//! ⚠️ 契约测试一律走 wiremock mock 上游；本模块不接触真实上游。

use crate::outbound::{describe_error, UpstreamClient};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// 兜底口径的资源列表端点。
pub const GET_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
/// 套餐概览（详细口径第一步）。
pub const RESOURCE_SUMMARY_PATH: &str = "/billing/meter/get-user-resource-summary";
/// 免费包列表（详细口径第二步）。
pub const RESOURCE_FREE_PACKAGES_PATH: &str = "/billing/meter/get-user-resource-free-packages";
/// 付费包列表（详细口径第三步）。
pub const RESOURCE_PAID_PACKAGES_PATH: &str = "/billing/meter/get-user-resource-paid-packages";
/// 每日签到状态（详细口径第四步）。
pub const CHECKIN_STATUS_PATH: &str = "/v2/billing/meter/checkin-activity-status";

/// 计费端点的超时（Python `http_json(..., timeout=15)`）。
pub const BILLING_TIMEOUT: Duration = Duration::from_secs(15);
/// 签到状态端点的超时（Python `timeout=10`）。
pub const CHECKIN_TIMEOUT: Duration = Duration::from_secs(10);
/// 兜底资源列表的超时（Python `timeout=30`）。
pub const FALLBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// 兜底口径固定的产品码（Python `_fetch_credits_fallback` 的 `ProductCode`）。
pub const FALLBACK_PRODUCT_CODE: &str = "p_tcaca";
/// 兜底口径的到期区间上界（Python 写死的 "2036-01-01 00:00:00"）。
pub const FALLBACK_END_TIME: &str = "2036-01-01 00:00:00";

/// 详细口径的取数结果（原文）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DetailedCredits {
    /// 概览里的 `Packages` 数组。
    pub summary_packages: Vec<Value>,
    /// 概览里的 `IsPaidUser`。
    pub is_paid_user: bool,
    /// 免费包 + 付费包合并、**按 `AccountId` 去重**后的账户条目。
    pub accounts: Vec<Value>,
    /// 签到状态（`None` = 这一步没取到）。
    pub checkin: Option<Value>,
}

/// POST 一个 JSON body 并解析回 JSON。
///
/// 与 Python `http_json` 对齐的要点：
/// - 非 2xx 抛错（Python 是 `urlopen` 抛 `HTTPError`，调用方 catch）
/// - 响应体不是 JSON 也算失败
/// - TLS 握手被掐 / 超时 / 5xx 值得再试一次（Python `_retryable`），
///   最多 3 次，退避 `backoff * attempt`（Python 默认 `1.0 * attempt` 秒）
async fn post_json(
    client: &UpstreamClient,
    url: &str,
    headers: &HashMap<String, String>,
    body: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 1..=3u32 {
        match client.post_with_timeout(url, headers, body, timeout).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status >= 500 {
                    last = format!("HTTP {status}");
                } else if !(200..300).contains(&status) {
                    // 4xx 不是瞬时故障，重试没有意义。
                    return Err(format!("HTTP {status}"));
                } else {
                    let text = resp.text().await.map_err(|e| describe_error(&e))?;
                    return serde_json::from_str(&text).map_err(|_| "响应不是 JSON".to_string());
                }
            }
            Err(e) => {
                // 连接类与超时类故障值得再试；其余（构造错误等）直接放弃。
                if !(e.is_timeout() || e.is_connect()) {
                    return Err(describe_error(&e));
                }
                last = describe_error(&e);
            }
        }
        if attempt < 3 {
            tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
        }
    }
    Err(last)
}

/// 从 `data` 里取 `Accounts` 数组。
fn accounts_of(payload: &Value) -> Vec<Value> {
    payload
        .get("data")
        .and_then(|d| d.get("Accounts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 按 `AccountId` 去重（Python 的 `seen_account_ids`）。
///
/// 免费包与付费包会重叠（同一个包既在免费列表又在付费列表），不去重会把
/// 同一个包算两遍，积分总额直接翻倍。
fn dedupe_by_account_id(accounts: Vec<Value>) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(accounts.len());
    for acc in accounts {
        let id = acc.get("AccountId").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            out.push(acc);
            continue;
        }
        if seen.iter().any(|s| s == id) {
            continue;
        }
        seen.push(id.to_string());
        out.push(acc);
    }
    out
}

/// 签到状态归一化成 5 键（Python `_fetch_credits_cn_detailed` 的 `checkin_info`）。
fn checkin_info(data: &Value) -> Value {
    let pick = |key: &str, default: Value| data.get(key).cloned().unwrap_or(default);
    json!({
        "today_checked_in": pick("today_checked_in", json!(false)),
        "streak_days": pick("streak_days", json!(0)),
        "daily_credit": pick("daily_credit", json!(0)),
        "today_credit": pick("today_credit", json!(0)),
        "active": pick("active", json!(true)),
    })
}

/// 详细口径（国内版）：四步取数。
///
/// `billing_upstream` 是 `realm_config(realm).billing_upstream`；
/// `chat_upstream` 是 `chat_base_url(realm, "workbuddy")`——签到状态端点挂在
/// **chat** 主机上而不是计费主机上（Python `cfg["chat_upstream"]`）。
///
/// 第一步（概览）失败即整体失败：没有 `PackageCodes` 就拿不到包列表，
/// 后三步都无从谈起。其余三步失败只丢对应的一段。
pub async fn fetch_credits_detailed(
    client: &UpstreamClient,
    billing_upstream: &str,
    chat_upstream: &str,
    headers: &HashMap<String, String>,
) -> Result<DetailedCredits, String> {
    let mut headers = headers.clone();
    headers.insert("X-Client-Platform".into(), "web".into());

    // 1. 套餐概览
    let summary = post_json(
        client,
        &format!("{billing_upstream}{RESOURCE_SUMMARY_PATH}"),
        &headers,
        &json!({}),
        BILLING_TIMEOUT,
    )
    .await?;
    if summary.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = summary
            .get("msg")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("code {}", summary.get("code").unwrap_or(&Value::Null)));
        return Err(msg);
    }
    let s_data = summary.get("data").cloned().unwrap_or(json!({}));
    let summary_packages: Vec<Value> = s_data
        .get("Packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let pkg_codes: Vec<Value> = summary_packages
        .iter()
        .filter_map(|p| p.get("PackageCode").cloned())
        .filter(|c| !c.is_null() && c.as_str() != Some(""))
        .collect();

    let mut free_accounts = Vec::new();
    let mut paid_accounts = Vec::new();
    if !pkg_codes.is_empty() {
        // 2. 免费包
        if let Ok(free) = post_json(
            client,
            &format!("{billing_upstream}{RESOURCE_FREE_PACKAGES_PATH}"),
            &headers,
            &json!({
                "PackageCodes": pkg_codes,
                "PageNumber": 1,
                "PageSize": 200,
                "Status": [0],
            }),
            BILLING_TIMEOUT,
        )
        .await
        {
            free_accounts = accounts_of(&free);
        }
        // 3. 付费包
        if let Ok(paid) = post_json(
            client,
            &format!("{billing_upstream}{RESOURCE_PAID_PACKAGES_PATH}"),
            &headers,
            &json!({
                "PackageCodes": pkg_codes,
                "PageNumber": 1,
                "PageSize": 200,
                "Status": [0, 3],
                "NeedRenewInfo": true,
            }),
            BILLING_TIMEOUT,
        )
        .await
        {
            paid_accounts = accounts_of(&paid);
        }
    }

    // 4. 每日签到状态
    let checkin = match post_json(
        client,
        &format!("{chat_upstream}{CHECKIN_STATUS_PATH}"),
        &headers,
        &json!({}),
        CHECKIN_TIMEOUT,
    )
    .await
    {
        Ok(res) => {
            let c_data = res.get("data").cloned().unwrap_or(json!({}));
            if c_data.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                None
            } else {
                Some(checkin_info(&c_data))
            }
        }
        Err(_) => None,
    };

    let mut accounts = free_accounts;
    accounts.extend(paid_accounts);
    Ok(DetailedCredits {
        summary_packages,
        is_paid_user: s_data
            .get("IsPaidUser")
            .map(|v| match v {
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
                Value::String(s) => !s.is_empty() && s != "false" && s != "0",
                _ => false,
            })
            .unwrap_or(false),
        accounts: dedupe_by_account_id(accounts),
        checkin,
    })
}

/// 兜底口径：一次 `get-user-resource` 取回账户条目。
///
/// `now_text` 是本地的 `"%Y-%m-%d %H:%M:%S"`（Python 用
/// `time.strftime("%Y-%m-%d %H:%M:%S")` 当区间下界）。
pub async fn fetch_credits_fallback(
    client: &UpstreamClient,
    billing_upstream: &str,
    headers: &HashMap<String, String>,
    now_text: &str,
) -> Result<Vec<Value>, String> {
    let res = post_json(
        client,
        &format!("{billing_upstream}{GET_RESOURCE_PATH}"),
        headers,
        &json!({
            "PageNumber": 1,
            "PageSize": 100,
            "ProductCode": FALLBACK_PRODUCT_CODE,
            "Status": [0, 3],
            "PackageEndTimeRangeBegin": now_text,
            "PackageEndTimeRangeEnd": FALLBACK_END_TIME,
        }),
        FALLBACK_TIMEOUT,
    )
    .await?;
    Ok(res
        .get("data")
        .and_then(|d| d.get("Response"))
        .and_then(|r| r.get("Data"))
        .and_then(|d| d.get("Accounts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn headers() -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("Content-Type".into(), "application/json".into());
        h.insert("Authorization".into(), "Bearer tok".into());
        h
    }

    #[tokio::test]
    async fn detailed_fetch_reads_all_four_endpoints() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"IsPaidUser": true, "Packages": [{"PackageCode": "p1"}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(RESOURCE_FREE_PACKAGES_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"Accounts": [{"AccountId": "a1", "PackageName": "free"}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(RESOURCE_PAID_PACKAGES_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"Accounts": [{"AccountId": "a1", "PackageName": "free"}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(CHECKIN_STATUS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"today_checked_in": true, "streak_days": 4}
            })))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let out = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
            .await
            .expect("detailed fetch must succeed");

        assert!(out.is_paid_user);
        assert_eq!(out.summary_packages.len(), 1);
        // a1 在免费与付费列表里都出现 ⇒ 去重后只剩一条
        assert_eq!(out.accounts.len(), 1);
        assert_eq!(out.accounts[0]["AccountId"], "a1");
        let checkin = out.checkin.expect("checkin must be present");
        assert_eq!(checkin["today_checked_in"], true);
        assert_eq!(checkin["streak_days"], 4);
        // 缺省键补齐
        assert_eq!(checkin["daily_credit"], 0);
        assert_eq!(checkin["active"], true);
    }

    #[tokio::test]
    async fn detailed_fetch_fails_when_summary_code_is_not_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 7, "msg": "配额不足"
            })))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
            .await
            .expect_err("non-zero code must fail the whole fetch");
        assert_eq!(err, "配额不足");
    }

    #[tokio::test]
    async fn detailed_fetch_tolerates_a_broken_checkin_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"Packages": []}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(CHECKIN_STATUS_PATH))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let out = fetch_credits_detailed(&client, &server.uri(), &server.uri(), &headers())
            .await
            .expect("checkin failure must not fail the fetch");
        assert!(out.checkin.is_none());
    }

    #[tokio::test]
    async fn fallback_fetch_sends_the_python_body_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(GET_RESOURCE_PATH))
            .and(body_json(json!({
                "PageNumber": 1,
                "PageSize": 100,
                "ProductCode": "p_tcaca",
                "Status": [0, 3],
                "PackageEndTimeRangeBegin": "2026-10-20 08:00:00",
                "PackageEndTimeRangeEnd": "2036-01-01 00:00:00",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"Response": {"Data": {"Accounts": [{"AccountId": "x"}]}}}
            })))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let accounts =
            fetch_credits_fallback(&client, &server.uri(), &headers(), "2026-10-20 08:00:00")
                .await
                .expect("fallback fetch must succeed");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["AccountId"], "x");
    }

    #[tokio::test]
    async fn four_hundred_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = post_json(
            &client,
            &format!("{}/x", server.uri()),
            &headers(),
            &json!({}),
            BILLING_TIMEOUT,
        )
        .await
        .expect_err("403 must fail");
        assert_eq!(err, "HTTP 403");
    }
}
