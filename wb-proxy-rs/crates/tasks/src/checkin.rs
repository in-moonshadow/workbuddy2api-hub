//! 国内版每日签到与对话活跃上报（关卡 4.6）。
//!
//! 权威参照 `wb_accounts.py` 的 `checkin` / `_checkin_upstream`（:1596-1641）、
//! `report_activity`（:1643-1670）、`can_checkin`（:1319-1327）、
//! `can_report_activity`（:1371-1380），以及 `wb_tasks.fetch_streak_days`
//! （:189-203）。
//!
//! ## R4 修正（与 Python 的有意差异）
//!
//! Python 的 `_checkin_upstream` 在**拿到响应之后无条件**先写
//! `last_checkin = now` 再判断 code（:1620-1623）——上游拒绝（例如 `code=1`
//! 风控）时本地也被标记成「今天已签」，`can_checkin()` 当天即返回 False，
//! 账号整整一天不会再重试。这是待修正的缺陷。
//!
//! 本实现改为「**先判成功、仅 `ok == true` 时写盘**」：
//!
//! - 正常 200：`code == 0 || code == 10001` → 写盘 + `ok:true`；其余 code →
//!   **不写盘** → `ok:false`
//! - 非 2xx 且 body 可解析：`code == 10001` 或 msg 含「今天已签到」/「已签到」
//!   → 写盘 + `ok:true`；其余 → 不写盘 → `ok:false`
//! - 网络 / 超时 / 解析失败 → 不写盘 → `ok:false`
//!
//! 写盘失败**不得翻转**签到结果：调用方看到的仍是 `ok:true`，
//! 失败原因放在单独可见的 [`CheckinOutcome::persist_error`]。
//!
//! 由此带来的可重试性是本关卡的验收核心：签到失败后 `can_checkin` 必须仍为
//! true，下一轮调度可以重新尝试。
//!
//! ## 活动历史（关卡 4.20 线 B）
//!
//! Python 在 `checkin()` 里调 `wb_activity.record_attempt(...)`
//! （`wb_accounts.py:1674`）记录活动历史；Rust 侧在本关接上了这一条写入点：
//! 整次尝试记一行（成功 / 失败都记），写不进去只打日志——活动历史是运维
//! 元数据，写不进去不该把一次成功的签到变成失败。
//!
//! ⚠️ Python 的 `report_activity()`（国内版对话活跃上报）**没有**
//! `record_attempt` 调用点（`wb_activity` 的调用点只有 `wb_accounts.py` 的
//! 1478 / 1570 / 1674 三处），Rust 侧同样不写：活动词表里也没有它的位置
//! （`wb_activity.TASKS` 只有 `checkin` 与 `daily_chat`）。

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use wb_gateway::endpoints::realm_config;
use wb_gateway::headers::{billing_request_headers, BillingHeadersParams};

use wb_gateway::outbound::UpstreamClient;
use wb_storage::{write_idempotent_fields, IdempotentFields};

use crate::activity;
use crate::events::{build_event, report_events};
use crate::growth_api::TaskEndpoints;
use crate::growth_runner::{Clock, RunnerAccount};

/// 每日签到的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_accounts.py:1683` 的 `http_json(..., timeout=15)`。
const CHECKIN_TIMEOUT: Duration = Duration::from_secs(15);

/// 连签天数的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_tasks.py:156`（`fetch_streak_days`）的 `timeout=10`。
const STREAK_DAYS_TIMEOUT: Duration = Duration::from_secs(10);

/// `CHECKIN_PATH`（`wb_accounts.py:211`）：拼在 `billing_upstream` 之后。
pub const CHECKIN_PATH: &str = "/v2/billing/meter/daily-checkin";

/// 签到请求的超时（Python `timeout=15`，:1616）。由 `UpstreamClient` 统一约束，
/// 此处仅作契约记录。
pub const CHECKIN_TIMEOUT_SECS: u64 = 15;

/// 签到 / 活跃上报落盘所需的本地上下文。
///
/// 为什么不塞进 `TaskAccount` / `RunnerAccount`：那两个结构是冻结的出站身份
/// 契约（uid / token / enterprise / chat 头），加 `Path` 会把「存储布局」耦合进
/// 「身份」，且让纯 HTTP 的调用方也被迫构造目录。这里显式传入。
pub struct CheckinCtx<'a> {
    /// 账号池目录（`accounts/`）。
    pub dir: &'a Path,
    /// 账号文件名 `<safe_uid>.json`（**不是**裸 uid）——`wb_storage` 只认这个。
    pub account_name: &'a str,
    /// 本地时间源；`lastCheckin` / `lastActivityReport` 用它格式化。
    pub clock: &'a dyn Clock,
    /// 设备令牌，非空时追加 `X-Device-Token`。
    pub device_token: Option<&'a str>,
}

/// 签到结果。
///
/// 键集对齐 Python 的两档返回：
/// - 成功 / 上游给了 code：`{ok, code, msg, data}`
/// - 失败且无 code（网络、HTTP 无 body 等）：`{ok, error}`
///
/// 另外追加 Rust 侧独有的 `persist_error`：仅在「判定成功但落盘失败」时出现，
/// 让调用方既能保留 `ok:true`，又能看见本地记录没写成。
#[derive(Debug, Clone, PartialEq)]
pub struct CheckinOutcome {
    pub ok: bool,
    /// 上游返回的 code；网络失败时为 `None`（Python 这档没有 code 键）。
    pub code: Option<i64>,
    pub msg: Option<String>,
    pub data: Option<Value>,
    /// 无 code 档的错误文案（对应 Python 的 `error` 键）。
    pub error: Option<String>,
    /// 落盘失败原因；`None` 表示无需落盘或已写成。
    pub persist_error: Option<String>,
}

impl CheckinOutcome {
    fn success(code: i64, msg: String, data: Option<Value>) -> Self {
        Self {
            ok: true,
            code: Some(code),
            msg: Some(msg),
            data,
            error: None,
            persist_error: None,
        }
    }

    /// `{ok: False, error}`：无 code 档。
    fn failure(error: String) -> Self {
        Self {
            ok: false,
            code: None,
            msg: None,
            data: None,
            error: Some(error),
            persist_error: None,
        }
    }

    /// 200 但上游给的 code 不是成功码：Python 仍走上面那个
    /// `{"ok": code==0 or code==10001, "code", "msg", "data"}` 形状（`wb_accounts.py:1623`），
    /// 所以这里保留 code/msg，**不写 error**。R4 修正只改变「写不写盘」。
    fn rejected_with_msg(code: i64, msg: String, data: Option<Value>) -> Self {
        Self {
            ok: false,
            code: Some(code),
            msg: Some(msg),
            data,
            error: None,
            persist_error: None,
        }
    }

    /// 非 2xx 且不是「今天已签到」：Python 返回 `{"ok": False, "error": msg}`
    /// （`wb_accounts.py:1637`），不带 code/msg。
    fn rejected(code: i64, error: String) -> Self {
        Self {
            ok: false,
            code: Some(code),
            msg: None,
            data: None,
            error: Some(error),
            persist_error: None,
        }
    }

    /// 转 JSON；`persist_error` 只在 `Some` 时出现。
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("ok".into(), Value::Bool(self.ok));
        if let Some(code) = self.code {
            map.insert("code".into(), json!(code));
        }
        if let Some(msg) = &self.msg {
            map.insert("msg".into(), Value::String(msg.clone()));
        }
        if let Some(data) = &self.data {
            map.insert("data".into(), data.clone());
        }
        if let Some(error) = &self.error {
            map.insert("error".into(), Value::String(error.clone()));
        }
        if let Some(err) = &self.persist_error {
            map.insert("persist_error".into(), Value::String(err.clone()));
        }
        Value::Object(map)
    }
}

// ---------------------------------------------------------------------------
// can_* 判定（纯函数版；`today` 由调用方按本地时区给出 `%Y-%m-%d`）
// ---------------------------------------------------------------------------

/// `can_checkin()`（`wb_accounts.py:1319-1327`）：cn、非企业、今天还没签过。
///
/// `last_checkin` 是 `%Y-%m-%d %H:%M:%S` 串，只比对前 10 位（Python
/// `startswith(today_str)`）；日期相同但时刻不同仍算「今天签过」。
pub fn can_checkin(
    realm: &str,
    is_enterprise: bool,
    last_checkin: Option<&str>,
    today: &str,
) -> bool {
    if realm != "cn" || is_enterprise {
        return false;
    }
    match last_checkin {
        None => true,
        Some(v) => !v.starts_with(today),
    }
}

/// `can_report_activity()`（`wb_accounts.py:1371-1380`）：口径与 [`can_checkin`]
/// 完全一致，只是读 `lastActivityReport`。
pub fn can_report_activity(
    realm: &str,
    is_enterprise: bool,
    last_activity_report: Option<&str>,
    today: &str,
) -> bool {
    if realm != "cn" || is_enterprise {
        return false;
    }
    match last_activity_report {
        None => true,
        Some(v) => !v.starts_with(today),
    }
}

/// 当前本地日期 `%Y-%m-%d`（`time.strftime("%Y-%m-%d")` 的等价）。
pub fn today(clock: &dyn Clock) -> String {
    clock.local_now().chars().take(10).collect()
}

/// 生产环境的签到基址：`get_realm_config("cn").billing_upstream`
/// （`https://www.codebuddy.cn`）。签到守卫已保证只走 cn。
pub fn production_billing_base() -> String {
    realm_config("cn").billing_upstream.to_string()
}

// ---------------------------------------------------------------------------
// 签到
// ---------------------------------------------------------------------------

/// 写 `lastCheckin = clock.local_now()`。
///
/// 失败只回 `Some(原因)`，绝不上抛——调用方不得因此翻转签到结论。
fn persist_last_checkin(ctx: &CheckinCtx<'_>) -> Option<String> {
    let fields = IdempotentFields {
        last_checkin: Some(ctx.clock.local_now()),
        ..IdempotentFields::default()
    };
    write_idempotent_fields(ctx.dir, ctx.account_name, &fields)
        .err()
        .map(|e| e.to_string())
}

/// 上游 body 里的 `msg`：`payload.get("msg") or fallback`（空串 / 缺失都回落）。
fn msg_of(payload: &Value, fallback: &str) -> String {
    payload
        .get("msg")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn code_of(payload: &Value) -> Option<i64> {
    payload.get("code").and_then(Value::as_i64)
}

/// 国内版每日签到，**先判成功再写盘**（R4 修正，见模块文档）。
///
/// - 守卫：非 cn / 企业账号 → `ok:false` + `error`（文案逐字对齐 Python）
/// - 请求：`POST {billing_base}{CHECKIN_PATH}`，body `{}`，billing 头
/// - 任何失败路径都不写 `lastCheckin`，从而保留当天重试的可能
///
/// **活动历史**：整次尝试记一行（Python `wb_accounts.py:1674` 的
/// `wb_activity.record_attempt`）；写不进去只打日志，不翻转签到结论。
///
/// `billing_base` 显式传入而非内部取 `realm_config("cn")`：契约测试要用 mock
/// 上游替换 `https://www.codebuddy.cn`（M4 约束「不得对真实上游发任何请求」）。
/// 生产侧统一传 [`production_billing_base`]。
pub async fn checkin(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    billing_base: &str,
    ctx: &CheckinCtx<'_>,
) -> CheckinOutcome {
    // Python `Account.checkin`（`wb_accounts.py:1663-1674`）：守卫（realm /
    // 企业账号）在 `record_attempt` **之前**早退 ⇒ 这两种情况不记活动历史。
    if acc.realm != "cn" || acc.is_enterprise {
        return checkin_inner(client, acc, billing_base, ctx).await;
    }
    let outcome = checkin_inner(client, acc, billing_base, ctx).await;
    record_checkin(ctx, acc, &outcome);
    outcome
}

/// 签到本体（不含活动历史写入）。
async fn checkin_inner(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    billing_base: &str,
    ctx: &CheckinCtx<'_>,
) -> CheckinOutcome {
    if acc.realm != "cn" {
        return CheckinOutcome::failure("checkin is only available for CN realm accounts".into());
    }
    if acc.is_enterprise {
        return CheckinOutcome::failure("enterprise accounts do not support checkin".into());
    }
    // Python `_checkin_upstream` 写死 get_realm_config("cn")：守卫已保证 realm。
    let url = format!("{billing_base}{CHECKIN_PATH}");
    let headers = billing_request_headers(&BillingHeadersParams {
        uid: acc.api.uid,
        token: acc.api.token,
        realm: acc.realm,
        domain: "",
        enterprise_id: acc.api.enterprise_id,
        device_token: ctx.device_token,
    });

    let resp = match client
        .post_with_timeout(&url, &headers, &json!({}), CHECKIN_TIMEOUT)
        .await
    {
        Ok(r) => r,
        // 网络 / 超时：不写盘
        Err(e) => return CheckinOutcome::failure(e.to_string()),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        let payload: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            // Python 的 `d = resp.json()` 抛异常会被外层 except 收成 ok:false
            Err(_) => return CheckinOutcome::failure("call_failed".into()),
        };
        let code = code_of(&payload).unwrap_or(-1);
        let msg = msg_of(&payload, "ok");
        let data = payload.get("data").cloned();
        if !(code == 0 || code == 10001) {
            // R4：上游拒绝 → 不写盘，可重试；返回形状仍是 200 分支那档
            return CheckinOutcome::rejected_with_msg(code, msg, data);
        }
        let mut outcome = CheckinOutcome::success(code, msg, data);
        outcome.persist_error = persist_last_checkin(ctx);
        return outcome;
    }

    // 非 2xx：Python 读 body，`code == 10001` 或 msg 含「已签到」都算今天已完成。
    let fallback_msg = format!("HTTP {}", status.as_u16());
    let parsed = serde_json::from_str::<Value>(&text).ok();
    let code = parsed.as_ref().and_then(code_of);
    let msg = parsed
        .as_ref()
        .map(|p| msg_of(p, &fallback_msg))
        .unwrap_or_else(|| fallback_msg.clone());
    let already = code == Some(10001) || msg.contains("今天已签到") || msg.contains("已签到");
    if already {
        let data = parsed.as_ref().and_then(|p| p.get("data")).cloned();
        let mut outcome = CheckinOutcome::success(code.unwrap_or(10001), msg, data);
        outcome.persist_error = persist_last_checkin(ctx);
        return outcome;
    }
    match code {
        Some(code) => CheckinOutcome::rejected(code, msg),
        None => CheckinOutcome::failure(fallback_msg),
    }
}

/// 记一次签到尝试（Python `wb_activity.record_attempt`，`wb_accounts.py:1674`）。
///
/// 说明文案取 `msg` → `error` → `"成功"` / `"失败"`，与 Python
/// `record_attempt` 的 `result.get("msg") or result.get("error") or (…)`
/// 同款。
///
/// **写不进去只打日志**（`record_attempt` 内部）：活动历史是运维元数据，
/// 写不进去不该把一次成功的签到变成失败。
fn record_checkin(ctx: &CheckinCtx<'_>, acc: &RunnerAccount<'_>, outcome: &CheckinOutcome) {
    let usage_dir = crate::api_routes::usage_dir_from(ctx.dir);
    let message = outcome
        .msg
        .as_deref()
        .filter(|text| !text.is_empty())
        .or(outcome.error.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if outcome.ok {
                "成功".to_string()
            } else {
                "失败".to_string()
            }
        });
    activity::record_attempt(
        &usage_dir,
        acc.api.uid,
        acc.nickname.unwrap_or(""),
        acc.realm,
        activity::TASK_CHECKIN,
        activity::TRIGGER_UNKNOWN,
        outcome.ok,
        &message,
    );
}

// ---------------------------------------------------------------------------
// 对话活跃上报
// ---------------------------------------------------------------------------

/// 读连登天数（`wb_tasks.fetch_streak_days`，:189-203）。
///
/// `GET {CHAT_BASE}/activity/growth/streak`；任何异常（网络 / 非 2xx / 形状不对）
/// 都回 `None`，与 `Some(0)` 严格区分——后者是「查到了，确实 0 天」。
pub async fn fetch_streak_days(
    client: &UpstreamClient,
    chat_base: &str,
    headers: &std::collections::HashMap<String, String>,
) -> Option<i64> {
    let url = format!("{chat_base}/activity/growth/streak");
    let resp = client
        .get_with_timeout(&url, headers, STREAK_DAYS_TIMEOUT)
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    // `(d.get("data") or {}).get("streak") or {}` → `.get("days")`
    doc.get("data")?
        .get("streak")?
        .get("days")
        .and_then(Value::as_i64)
}

/// 国内版对话活跃上报（`report_activity`，:1643-1670）。
///
/// 一条 `chat` 事件 POST 到 `{chat_base}/v2/report`（chat 头，不是 billing 头），
/// 成功后写 `lastActivityReport`，再读连登天数回显。上报本身失败**不写盘**；
/// 读连登失败只降级文案，不影响 `ok`。
pub async fn report_activity(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    ctx: &CheckinCtx<'_>,
    now_ms: i64,
) -> Value {
    if acc.realm != "cn" {
        return json!({
            "ok": false,
            "error": "activity report is only for CN realm accounts",
        });
    }
    if acc.is_enterprise {
        return json!({
            "ok": false,
            "error": "enterprise accounts do not support activity report",
        });
    }
    let event = build_event(acc.api.uid, "chat", 0, now_ms, None);
    if !report_events(client, ep.chat_base, acc.api.chat_headers, &[event]).await {
        return json!({
            "ok": false,
            "error": "活跃上报被上游拒绝（未返回 code=0）",
        });
    }
    let fields = IdempotentFields {
        last_activity_report: Some(ctx.clock.local_now()),
        ..IdempotentFields::default()
    };
    // Python 在此处不检查 save 是否成功（`self.save(...)` 无返回值），保持同口径；
    // 写盘失败仍想被看见，故挂到响应里（不影响 ok）。
    let persist_error = write_idempotent_fields(ctx.dir, ctx.account_name, &fields)
        .err()
        .map(|e| e.to_string());

    let days = fetch_streak_days(client, ep.chat_base, acc.api.chat_headers).await;
    let mut out = match days {
        None => json!({
            "ok": true,
            "msg": "对话活跃上报成功（连登天数未知）",
        }),
        Some(d) if d <= 0 => json!({
            "ok": true,
            "streak_days": 0,
            "msg": "对话活跃上报成功，但连登仍是 0 天（可能被上游静默丢弃）",
        }),
        Some(d) => json!({
            "ok": true,
            "streak_days": d,
            "msg": format!("对话活跃上报成功（连续打卡 {d} 天）"),
        }),
    };
    if let (Some(err), Some(map)) = (persist_error, out.as_object_mut()) {
        map.insert("persist_error".into(), Value::String(err));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_checkin_matches_python_rules() {
        let today = "2026-10-17";
        assert!(can_checkin("cn", false, None, today));
        assert!(can_checkin("cn", false, Some("2026-10-16 09:00:00"), today));
        // 同日前缀 → 今天已签
        assert!(!can_checkin(
            "cn",
            false,
            Some("2026-10-17 00:00:01"),
            today
        ));
        // 非 cn / 企业账号一律 false
        assert!(!can_checkin("intl", false, None, today));
        assert!(!can_checkin("cn", true, None, today));
    }

    #[test]
    fn can_report_activity_matches_python_rules() {
        let today = "2026-10-17";
        assert!(can_report_activity("cn", false, None, today));
        assert!(!can_report_activity(
            "cn",
            false,
            Some("2026-10-17 21:00:00"),
            today
        ));
        assert!(!can_report_activity("intl", false, None, today));
        assert!(!can_report_activity("cn", true, None, today));
    }

    #[test]
    fn outcome_json_keys_follow_python_shapes() {
        let ok = CheckinOutcome::success(0, "ok".into(), Some(json!({"a": 1})));
        let v = ok.to_json();
        assert_eq!(v["ok"], true);
        assert_eq!(v["code"], 0);
        assert_eq!(v["msg"], "ok");
        assert_eq!(v["data"], json!({"a": 1}));
        assert!(v.get("error").is_none());
        // 未落盘失败时不出现 persist_error
        assert!(v.get("persist_error").is_none());

        let failed = CheckinOutcome::failure("boom".into());
        let f = failed.to_json();
        assert_eq!(f["ok"], false);
        assert_eq!(f["error"], "boom");
        assert!(f.get("code").is_none());
    }

    #[test]
    fn persist_error_is_visible_but_does_not_flip_ok() {
        let mut o = CheckinOutcome::success(0, "ok".into(), None);
        o.persist_error = Some("disk full".into());
        let v = o.to_json();
        assert_eq!(v["ok"], true, "落盘失败不得翻转签到结论");
        assert_eq!(v["persist_error"], "disk full");
    }
}
