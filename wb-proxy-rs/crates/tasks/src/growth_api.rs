//! 关卡 4.4：成长任务拉取 / 接取 / 领奖。
//!
//! 权威参照 `wb_tasks.py`：
//! - [`fetch_growth_tasks`] ← `:107-138`（契约 2 的 12 字段归一化 + 企业版短路）
//! - [`accept_tasks`] ← `:205-266`（契约 5：分块 20、逐 code 部分失败）
//! - [`claim_task`] ← `:269-318`（契约 3/4：`{}` 空对象体，**仅 HTTP 400** 降级 web 域）
//!
//! ## 与 Python 的分叉（R8：失败必须可见）
//! Python 的 `accept`/`claim` 在网络异常处宽泛吞掉（只 `_log`）。本模块保留
//! 「单任务失败不中断整轮」的总语义，但把原因放进返回值：
//! [`accept_tasks`] 的 `failed`/`reasons`/`msg`，[`claim_task`] 的 `msg`。
//!
//! ## 路径口径
//! `fetch_growth_tasks` 走 `{CHAT_BASE}/v2/activity/growth/tasks`；领奖 web 降级
//! 走 `{WEB_BASE}/activity/growth/tasks/{code}/claim`（**无 `/v2`**）——两者前缀
//! 不同是 Python 原样，不归一化。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::events::{CHAT_BASE, WEB_BASE};
use crate::TASK_SPECS;

/// web 域降级领奖用的浏览器 UA（`wb_tasks.py:21` 的 `WEB_UA`，逐字）。
pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

/// 上游域名口径。生产恒定用 [`TaskEndpoints::production`]（即 `CHAT_BASE`/`WEB_BASE`
/// 常量）；契约测试注入 wiremock 的 base URL，从而**不向真实上游发任何请求**。
///
/// 之所以要可注入：领奖的 HTTP 400 降级目标是 `WEB_BASE` 这个硬编码常量，
/// 若不可替换，降级分支就只能靠「真的打出去」来测——那违反 M4 计划的
/// 「不得对真实上游发任何请求」约束。
#[derive(Debug, Clone, Copy)]
pub struct TaskEndpoints<'a> {
    pub chat_base: &'a str,
    pub web_base: &'a str,
}

impl TaskEndpoints<'static> {
    /// 生产口径：常量域名，与 Python `wb_tasks.py:17/19` 一致。
    pub fn production() -> Self {
        Self {
            chat_base: CHAT_BASE,
            web_base: WEB_BASE,
        }
    }
}

impl Default for TaskEndpoints<'static> {
    fn default() -> Self {
        Self::production()
    }
}

/// 账号出站需要的身份值，避免把 `wb-gateway::pool::AccountState` 引进签名。
#[derive(Debug, Clone)]
pub struct TaskAccount<'a> {
    pub uid: &'a str,
    pub token: &'a str,
    /// 企业空间 id（非空即企业账号 → 任务列表返回空，契约 2/11）。
    pub enterprise_id: &'a str,
    /// chat 目的地的出站头（`chat_request_headers` 产物）。领奖 web 降级不复用它。
    pub chat_headers: &'a HashMap<String, String>,
}

/// 契约 2：拉取成长任务列表。
///
/// - 企业账号 → 直接 `[]`（不发请求）
/// - 任何异常（网络 / 非 2xx / 解析失败 / `data` 形状不对）→ `[]`，不向上抛
/// - 12 字段顺序逐字对齐 Python dict 字面量（见 `GrowthTask::to_json`）
pub async fn fetch_growth_tasks(
    client: &wb_gateway::outbound::UpstreamClient,
    acc: &TaskAccount<'_>,
    ep: &TaskEndpoints<'_>,
) -> Vec<crate::GrowthTask> {
    if !acc.enterprise_id.is_empty() {
        return Vec::new();
    }
    let url = format!("{}/v2/activity/growth/tasks", ep.chat_base);
    let resp = match client.get(&url, acc.chat_headers).await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    normalize_tasks(&body)
}

/// 纯函数形式，便于不联网单测 Python 的 12 字段归一化规则。
pub(crate) fn normalize_tasks(body: &Value) -> Vec<crate::GrowthTask> {
    let raw = body
        .get("data")
        .and_then(|d| d.get("tasks"))
        .and_then(Value::as_array);
    let raw = match raw {
        Some(a) => a,
        None => return Vec::new(),
    };
    raw.iter()
        .map(|t| {
            let code = t
                .get("task_code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let spec = TASK_SPECS.iter().find(|(c, _)| *c == code).map(|(_, s)| *s);
            let prog = t.get("progress").cloned().unwrap_or(Value::Null);
            // Python: prog.get("current", 0) / prog.get("target", spec.target, 1)
            // 用 dict.get 而非 `or` —— 上游给 0 是合法值，只有键缺失才走缺省。
            let current = prog.get("current").and_then(Value::as_i64).unwrap_or(0);
            let default_target = spec.map(|s| s.target).unwrap_or(1);
            let target = prog
                .get("target")
                .and_then(Value::as_i64)
                .unwrap_or(default_target);
            // reward_credit 用的是 `or`（0 视作假值回退到 spec.reward）。
            let reward_credit = t
                .get("reward_credit")
                .and_then(Value::as_i64)
                .filter(|v| *v != 0)
                .or_else(|| spec.map(|s| s.reward))
                .unwrap_or(0);
            let name = t
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| spec.map(|s| s.name))
                .unwrap_or(&code)
                .to_string();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| t.get("task_desc").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            crate::GrowthTask {
                task_code: code,
                name,
                description,
                jump_url: t
                    .get("jump_url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: t
                    .get("accept_status")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("not_accepted")
                    .to_string(),
                task_type: t
                    .get("task_type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                current,
                target,
                reward_credit,
                reward_energy: t.get("reward_energy").and_then(Value::as_i64).unwrap_or(0),
                unforgeable: spec.map(|s| s.unforgeable).unwrap_or(false),
                reason: spec.map(|s| s.reason).unwrap_or("").to_string(),
            }
        })
        .collect()
}

/// 契约 5：批量接取。`codes` 为空时直接返回 `ok: true`（Python 同）。
///
/// 分块大小 `chunk` 用 `max(1, chunk)` 夹紧——Python 的 `range(0, n, max(1, chunk))`。
pub async fn accept_tasks(
    client: &wb_gateway::outbound::UpstreamClient,
    acc: &TaskAccount<'_>,
    codes: &[String],
    chunk: usize,
    ep: &TaskEndpoints<'_>,
) -> crate::AcceptOutcome {
    let mut out = crate::AcceptOutcome::empty();
    if codes.is_empty() {
        return out;
    }
    let url = format!("{}/v2/activity/growth/tasks/accept", ep.chat_base);
    let step = chunk.max(1);
    for part in codes.chunks(step) {
        let body = json!({ "task_codes": part });
        let resp = match client.post(&url, acc.chat_headers, &body).await {
            Ok(r) => r,
            Err(_) => {
                out.ok = false;
                out.msg = "accept batch failed: network error".to_string();
                out.failed.extend(part.iter().cloned());
                out.reasons.push(out.msg.clone());
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            // Python 读 300 字节再截 200 字符；这里同样截断，避免异常大体进日志。
            let detail = resp.text().await.unwrap_or_default();
            let detail = truncate_chars(&detail, 200);
            out.ok = false;
            out.msg = format!("HTTP {}: {}", status.as_u16(), detail);
            out.failed.extend(part.iter().cloned());
            out.reasons.push(out.msg.clone());
            continue;
        }
        let d: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                out.ok = false;
                out.msg = truncate_chars(&e.to_string(), 200);
                out.failed.extend(part.iter().cloned());
                // Python 的 `except Exception` 分支**不**往 reasons 追加。
                continue;
            }
        };
        if d.get("code").and_then(Value::as_i64) != Some(0) {
            out.ok = false;
            out.msg = d
                .get("msg")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "code={}",
                        d.get("code").map(value_to_text).unwrap_or_default()
                    )
                });
            out.failed.extend(part.iter().cloned());
            out.reasons.push(out.msg.clone());
            continue;
        }
        let results = d
            .get("data")
            .and_then(|v| v.get("results"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut seen: Vec<&str> = Vec::new();
        for item in &results {
            let code = item.get("task_code").and_then(Value::as_str).unwrap_or("");
            let st = item
                .get("status")
                .map(value_to_text)
                .unwrap_or_else(|| "None".to_string());
            seen.push(code);
            if st == "accepted" || st == "already_accepted" {
                out.accepted.push(code.to_string());
            } else {
                out.failed.push(code.to_string());
                let message = item
                    .get("message")
                    .filter(|v| !v.is_null())
                    .map(value_to_text)
                    .unwrap_or_else(|| st.clone());
                out.reasons.push(format!("{code}: {message}"));
            }
        }
        for code in part {
            if !seen.contains(&code.as_str()) {
                out.failed.push(code.clone());
            }
        }
    }
    out
}

/// 契约 3/4：单个任务领奖。
///
/// - `POST {CHAT_BASE}/activity/growth/tasks/{code}/claim`，体为 `{}`（Python `data=b"{}"`）
/// - 200 + `code == 0` → `credit`/`energy`
/// - 200 + 非 0 → `ok: false` + `msg`（**不**降级）
/// - **HTTP 400** → 降级 `{WEB_BASE}/activity/growth/tasks/{code}/claim`（web 头）
/// - 其它状态码 / 网络异常 → `ok: false`，**不**降级
pub async fn claim_task(
    client: &wb_gateway::outbound::UpstreamClient,
    acc: &TaskAccount<'_>,
    code: &str,
    ep: &TaskEndpoints<'_>,
) -> crate::ClaimOutcome {
    let url = format!("{}/activity/growth/tasks/{code}/claim", ep.chat_base);
    let empty = json!({});
    let resp = match client.post(&url, acc.chat_headers, &empty).await {
        Ok(r) => r,
        Err(e) => return crate::ClaimOutcome::failure(format!("claim failed: {e}")),
    };
    let status = resp.status();
    if status.is_success() {
        let d: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return crate::ClaimOutcome::failure(format!("claim failed: {e}")),
        };
        return read_claim_body(&d, None);
    }
    // 错误体可能是 JSON（`{"msg": ...}`），也可能不是。
    let text = resp.text().await.unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let err_msg = parsed
        .as_ref()
        .and_then(|v| v.get("msg"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));

    if status.as_u16() == 400 {
        let web_url = format!("{}/activity/growth/tasks/{code}/claim", ep.web_base);
        let web_hdrs = web_claim_headers(acc, ep.web_base);
        match client.post(&web_url, &web_hdrs, &empty).await {
            Ok(r2) if r2.status().is_success() => match r2.json::<Value>().await {
                // 降级分支的「非 0 码」文案回退到原始 err_msg（Python 同）。
                Ok(d2) => read_claim_body(&d2, Some(&err_msg)),
                Err(e) => crate::ClaimOutcome::failure(format!("web claim failed: {e}")),
            },
            Ok(r2) => crate::ClaimOutcome::failure(format!(
                "web claim failed: HTTP {}",
                r2.status().as_u16()
            )),
            Err(e) => crate::ClaimOutcome::failure(format!("web claim failed: {e}")),
        }
    } else {
        crate::ClaimOutcome::failure(err_msg)
    }
}

/// web 域领奖头（`wb_tasks.py:292-302` 逐字）。`web_base` 供 `Origin`/`Referer`/
/// `X-Domain` 三处使用；生产传 `WEB_BASE`，测试传 mock 地址。
fn web_claim_headers(acc: &TaskAccount<'_>, web_base: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("Authorization".into(), format!("Bearer {}", acc.token));
    h.insert("Accept".into(), "application/json, text/plain, */*".into());
    h.insert("Content-Type".into(), "application/json".into());
    h.insert("Origin".into(), web_base.into());
    h.insert(
        "Referer".into(),
        format!("{web_base}/profile/growth-center"),
    );
    h.insert("x-client-platform".into(), "web".into());
    h.insert("User-Agent".into(), WEB_UA.into());
    h.insert("X-User-Id".into(), acc.uid.into());
    h.insert("X-Domain".into(), web_base.into());
    h
}

/// 解析领奖响应体。`fallback_msg` 仅 web 降级分支用（非 0 码时回退原始 HTTP 文案）。
fn read_claim_body(d: &Value, fallback_msg: Option<&str>) -> crate::ClaimOutcome {
    if d.get("code").and_then(Value::as_i64) == Some(0) {
        let data = d.get("data");
        let credit = data
            .and_then(|v| v.get("credit"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let energy = data
            .and_then(|v| v.get("energy"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return crate::ClaimOutcome::success(credit, energy);
    }
    let msg = d
        .get("msg")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| fallback_msg.map(str::to_string))
        .unwrap_or_else(|| {
            format!(
                "code={}",
                d.get("code").map(value_to_text).unwrap_or_default()
            )
        });
    crate::ClaimOutcome::failure(msg)
}

/// `str(value)` 的近似：字符串原样，其它走 JSON 文本（`None` → `None`）。
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// 按 **字符**（非字节）截断，避免把 UTF-8 切坏。
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wb_gateway::outbound::UpstreamClient;
    use wiremock::matchers::{body_json_string, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 测试用账号：chat 与 web 两侧目的地**都**指向 wiremock，测试绝不触网。
    struct TestAccount {
        uid: String,
        token: String,
        enterprise_id: String,
        chat_headers: HashMap<String, String>,
    }

    /// 把 chat/web 两个 base 都指到同一个 mock server，避免任何真实上游请求。
    fn ep(server: &MockServer) -> TaskEndpoints<'static> {
        // MockServer 的 uri 生命周期与 server 绑定，故这里泄漏一份字符串拿 'static：
        // 测试进程内数量有限（每个测试一个），可接受。
        let base: &'static str = Box::leak(server.uri().into_boxed_str());
        TaskEndpoints {
            chat_base: base,
            web_base: base,
        }
    }

    impl TestAccount {
        fn cn() -> Self {
            let mut h = HashMap::new();
            h.insert("X-Domain".into(), "copilot.tencent.com".into());
            TestAccount {
                uid: "u-test".into(),
                token: "tok-test".into(),
                enterprise_id: String::new(),
                chat_headers: h,
            }
        }

        fn enterprise(id: &str) -> Self {
            let mut a = TestAccount::cn();
            a.enterprise_id = id.into();
            a
        }

        fn params(&self) -> TaskAccount<'_> {
            TaskAccount {
                uid: &self.uid,
                token: &self.token,
                enterprise_id: &self.enterprise_id,
                chat_headers: &self.chat_headers,
            }
        }
    }

    // ---- 契约 2：归一化（纯函数，不联网） ----

    #[test]
    fn normalize_emits_twelve_keys_with_spec_fallbacks() {
        let body = json!({"code": 0, "data": {"tasks": [{
            "task_code": "template_5",
            "task_desc": "来自 task_desc",
            "progress": {"current": 2}
        }]}});
        let tasks = normalize_tasks(&body);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        let spec = crate::task_spec("template_5").expect("spec exists");
        assert_eq!(t.name, spec.name); // 无 title → spec.name
        assert_eq!(t.description, "来自 task_desc"); // description 缺失 → task_desc
        assert_eq!(t.status, "not_accepted");
        assert_eq!(t.target, spec.target);
        assert_eq!(t.current, 2);
        assert_eq!(t.reward_credit, spec.reward);
        assert_eq!(t.reward_energy, 0);
        let obj = t.to_json();
        assert_eq!(obj.as_object().unwrap().len(), 12);
    }

    /// Python 的 `prog.get("target", spec.target)`：上游给 0 是合法值，不得回退。
    /// 而 `reward_credit` 用 `or`：0 视为假值回退到 spec.reward。
    #[test]
    fn normalize_zero_target_is_kept_but_zero_reward_falls_back() {
        let body = json!({"data": {"tasks": [{
            "task_code": "template_5", "title": "T",
            "progress": {"current": 0, "target": 0},
            "reward_credit": 0
        }]}});
        let t = &normalize_tasks(&body)[0];
        assert_eq!(t.target, 0);
        assert_eq!(
            t.reward_credit,
            crate::task_spec("template_5").unwrap().reward
        );
    }

    #[test]
    fn normalize_missing_data_or_tasks_is_empty() {
        assert!(normalize_tasks(&json!({"code": 0})).is_empty());
        assert!(normalize_tasks(&json!({"code": 0, "data": null})).is_empty());
        assert!(normalize_tasks(&json!({"code": 0, "data": {"tasks": null}})).is_empty());
    }

    // ---- 契约 2：企业版短路与异常吞并 ----

    #[tokio::test]
    async fn fetch_enterprise_account_short_circuits_without_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // 一次都不许打上游
            .mount(&server)
            .await;
        let acc = TestAccount::enterprise("ent-1");
        let tasks = fetch_growth_tasks(&UpstreamClient::new(), &acc.params(), &ep(&server)).await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn fetch_any_failure_yields_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/tasks"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        assert!(
            fetch_growth_tasks(&UpstreamClient::new(), &acc.params(), &ep(&server))
                .await
                .is_empty()
        );
    }

    // ---- 契约 5：accept 分块与部分失败 ----

    fn codes(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("task_{i}")).collect()
    }

    #[tokio::test]
    async fn accept_empty_codes_returns_ok_without_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        let out = accept_tasks(&UpstreamClient::new(), &acc.params(), &[], 20, &ep(&server)).await;
        assert!(out.ok);
        assert!(out.accepted.is_empty() && out.failed.is_empty());
    }

    /// 21 个 code、chunk=20 → 两批：第一批 20、第二批 1。
    #[tokio::test]
    async fn accept_splits_into_chunks_of_twenty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/activity/growth/tasks/accept"))
            .respond_with(|req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let part = body
                    .get("task_codes")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let results: Vec<Value> = part
                    .iter()
                    .map(|c| json!({"task_code": c, "status": "accepted"}))
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({
                    "code": 0, "data": {"results": results}
                }))
            })
            .mount(&server)
            .await;

        let acc = TestAccount::cn();
        let out = accept_tasks(
            &UpstreamClient::new(),
            &acc.params(),
            &codes(21),
            20,
            &ep(&server),
        )
        .await;
        assert!(out.ok, "msg={}", out.msg);
        assert_eq!(out.accepted.len(), 21);
        assert!(out.failed.is_empty());
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2);
        let sizes: Vec<usize> = reqs
            .iter()
            .map(|r| {
                serde_json::from_slice::<Value>(&r.body).unwrap()["task_codes"]
                    .as_array()
                    .unwrap()
                    .len()
            })
            .collect();
        assert_eq!(sizes, vec![20, 1]);
    }

    /// 契约 5：单任务 status 非 accepted → 记入 failed + reasons，不影响同批其它任务。
    #[tokio::test]
    async fn accept_partial_failure_is_per_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/activity/growth/tasks/accept"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "data": {"results": [
                    {"task_code": "a", "status": "accepted"},
                    {"task_code": "b", "status": "failed", "message": "prerequisite"}
                ]}
            })))
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        let out = accept_tasks(
            &UpstreamClient::new(),
            &acc.params(),
            &["a".to_string(), "b".to_string(), "c".to_string()],
            20,
            &ep(&server),
        )
        .await;
        assert!(out.ok, "整批仍为 ok（Python 同）");
        assert_eq!(out.accepted, vec!["a".to_string()]);
        // c 未出现在 results → 补记 failed；b 是逐 code 失败
        assert_eq!(out.failed, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(out.reasons, vec!["b: prerequisite".to_string()]);
    }

    /// 非 0 码：整批进 failed，`msg` 取上游 msg。
    #[tokio::test]
    async fn accept_nonzero_code_fails_whole_chunk() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/activity/growth/tasks/accept"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code": 3, "msg": "nope"})),
            )
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        let out = accept_tasks(
            &UpstreamClient::new(),
            &acc.params(),
            &codes(3),
            20,
            &ep(&server),
        )
        .await;
        assert!(!out.ok);
        assert_eq!(out.msg, "nope");
        assert_eq!(out.failed.len(), 3);
        assert_eq!(out.reasons, vec!["nope".to_string()]);
    }

    // ---- 契约 3/4：claim ----

    #[tokio::test]
    async fn claim_posts_empty_object_body_and_reads_code_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/heartbeat/claim"))
            .and(body_json_string("{}"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 5, "energy": 3}})),
            )
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        let out = claim_task(
            &UpstreamClient::new(),
            &acc.params(),
            "heartbeat",
            &ep(&server),
        )
        .await;
        assert!(out.ok);
        assert_eq!((out.credit, out.energy), (5, 3));
        assert!(out.msg.is_none());
    }

    /// 200 + 非 0 码（典型 `task not completed`）：不降级，直接失败。
    #[tokio::test]
    async fn claim_nonzero_code_does_not_degrade() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/x/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 400, "msg": "task not completed"})),
            )
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        let out = claim_task(&UpstreamClient::new(), &acc.params(), "x", &ep(&server)).await;
        assert!(!out.ok);
        assert_eq!(out.msg.as_deref(), Some("task not completed"));
    }

    /// 契约 4：HTTP 400 → 降级 web 域，且降级**成功**时返回 credit/energy。
    ///
    /// 用 `header("X-Domain")` 区分两个分支：chat 分支带 `copilot.tencent.com`
    /// （由 `chat_headers` 注入），web 分支带 mock base（由 `web_claim_headers` 注入）。
    #[tokio::test]
    async fn claim_http_400_degrades_to_web_and_reads_code_zero() {
        let server = MockServer::start().await;
        // chat 分支：400
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/x/claim"))
            .and(header("X-Domain", "copilot.tencent.com"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"msg": "need web"})))
            .mount(&server)
            .await;
        // web 分支：200 + code 0
        Mock::given(method("POST"))
            .and(path("/activity/growth/tasks/x/claim"))
            .and(header("x-client-platform", "web"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"credit": 7, "energy": 2}})),
            )
            .mount(&server)
            .await;

        let acc = TestAccount::cn();
        let out = claim_task(&UpstreamClient::new(), &acc.params(), "x", &ep(&server)).await;
        assert!(out.ok, "降级成功后应 ok；msg={:?}", out.msg);
        assert_eq!((out.credit, out.energy), (7, 2));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "chat 一次 + web 降级一次"
        );
    }

    /// 契约 4：降级分支回非 0 码 → `msg` 取上游 msg；上游无 msg 时回退原始 HTTP 文案。
    #[tokio::test]
    async fn claim_web_degrade_nonzero_code_falls_back_to_original_msg() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("X-Domain", "copilot.tencent.com"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"msg": "need web"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("x-client-platform", "web"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 9})))
            .mount(&server)
            .await;

        let acc = TestAccount::cn();
        let out = claim_task(&UpstreamClient::new(), &acc.params(), "x", &ep(&server)).await;
        assert!(!out.ok);
        // web 体无 msg → 回退 chat 分支的 err_msg（`need web`），而非 `code=9`
        assert_eq!(out.msg.as_deref(), Some("need web"));
    }

    /// 契约 4：web 降级本身也失败（500）→ 明确标注 web claim failed。
    #[tokio::test]
    async fn claim_web_degrade_http_500_reports_web_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("X-Domain", "copilot.tencent.com"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"msg": "need web"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("x-client-platform", "web"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let acc = TestAccount::cn();
        let out = claim_task(&UpstreamClient::new(), &acc.params(), "x", &ep(&server)).await;
        assert!(!out.ok);
        assert_eq!(out.msg.as_deref(), Some("web claim failed: HTTP 500"));
    }

    /// 契约 4：500 不降级 —— chat 分支只发一次，且文案是 `HTTP 500`。
    #[tokio::test]
    async fn claim_http_500_does_not_degrade() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&server)
            .await;
        let acc = TestAccount::cn();
        let out = claim_task(&UpstreamClient::new(), &acc.params(), "x", &ep(&server)).await;
        assert!(!out.ok);
        // 500 的错误体不是 JSON → 文案退回 `HTTP <status>`
        assert_eq!(out.msg.as_deref(), Some("HTTP 500"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// 契约 4：生产口径的 web 头 —— `Origin`/`Referer`/`X-Domain` 必须是
    /// `https://www.workbuddy.cn`（`wb_tasks.py:19/296-301` 逐字）。
    #[test]
    fn web_headers_match_python_literal() {
        let acc = TestAccount::cn();
        let h = web_claim_headers(&acc.params(), TaskEndpoints::production().web_base);
        assert_eq!(h["Authorization"], "Bearer tok-test");
        assert_eq!(h["Origin"], "https://www.workbuddy.cn");
        assert_eq!(
            h["Referer"],
            "https://www.workbuddy.cn/profile/growth-center"
        );
        assert_eq!(h["x-client-platform"], "web");
        assert_eq!(h["X-User-Id"], "u-test");
        assert_eq!(h["X-Domain"], "https://www.workbuddy.cn");
        assert_eq!(h["Accept"], "application/json, text/plain, */*");
        assert_eq!(h["Content-Type"], "application/json");
        assert_eq!(h["User-Agent"], WEB_UA);
        assert_eq!(h.len(), 9);
    }

    /// 生产口径常量不得漂移（`wb_tasks.py:17/19`）。
    #[test]
    fn production_endpoints_are_python_constants() {
        let p = TaskEndpoints::production();
        assert_eq!(p.chat_base, "https://copilot.tencent.com");
        assert_eq!(p.web_base, "https://www.workbuddy.cn");
    }

    #[test]
    fn web_ua_is_browser_ua_not_desktop_ua() {
        assert!(WEB_UA.starts_with("Mozilla/5.0"));
        assert!(WEB_UA.contains("Chrome/152"));
    }
}
