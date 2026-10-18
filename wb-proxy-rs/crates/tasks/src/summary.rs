//! 关卡 4.7：成长汇总（`wb_tasks.fetch_growth_summary`，:140-186）。
//!
//! 四段逐个查询，**任一环节失败只记日志、不影响已有字段**（Python 每段各包一层
//! `try/except Exception`），因此本函数**不返回错误**：失败段保持
//! [`GrowthSummary::default`] 的缺省值（`0` / `{"state":"unknown"}` / `None`）。
//!
//! ## 路径口径（R1 路径二，逐字）
//! | 环节 | 路径 |
//! |---|---|
//! | 能量 | `/v2/activity/growth/energy` |
//! | 连登 + 档位 | `/activity/growth/streak`（**无 `/v2`**） |
//! | 抽奖次数 | `/activity/growth/lottery/summary` |
//! | 猫猫旅行 | `/v2/activity/growth/buddy/travel/status`（**带 `/v2`**） |
//!
//! 与 `events::growth_summary_travel`（同样是路径二）**不共享常量**：本模块按
//! Python 原样写字面量，避免「统一前缀」时静默打断另一条路。

use std::collections::HashMap;

use serde_json::Value;

use crate::growth_api::{TaskAccount, TaskEndpoints};
use crate::{default_tier_statuses, GrowthSummary, TravelStatus};

/// `fetch_growth_summary()`：汇总四段，失败段保持缺省值。
pub async fn fetch_growth_summary(
    client: &wb_gateway::outbound::UpstreamClient,
    acc: &TaskAccount<'_>,
    ep: &TaskEndpoints<'_>,
) -> GrowthSummary {
    let mut out = GrowthSummary::default();

    // 1. 能量：`(d.get("data") or {}).get("balance", 0)`
    if let Some(doc) = get_json(
        client,
        &format!("{}/v2/activity/growth/energy", ep.chat_base),
        acc.chat_headers,
    )
    .await
    {
        if let Some(n) = doc
            .get("data")
            .and_then(|d| d.get("balance"))
            .and_then(Value::as_i64)
        {
            out.energy = n;
        }
    }

    // 2. 连登 / 补签卡 / 兑换档位
    if let Some(doc) = get_json(
        client,
        &format!("{}/activity/growth/streak", ep.chat_base),
        acc.chat_headers,
    )
    .await
    {
        let data = doc.get("data").cloned().unwrap_or(Value::Null);
        if let Some(n) = data
            .get("streak")
            .and_then(|s| s.get("days"))
            .and_then(Value::as_i64)
        {
            out.streak_days = n;
        }
        if let Some(n) = data
            .get("makeup_cards")
            .and_then(|c| c.get("balance"))
            .and_then(Value::as_i64)
        {
            out.makeup_cards = Some(n);
        }
        let redemp = data
            .get("redemption_status")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(arr) = redemp.get("tiers").and_then(Value::as_array) {
            out.tiers = Some(arr.iter().filter_map(Value::as_i64).collect());
        }
        let mut statuses = default_tier_statuses();
        for (slot, key) in
            statuses
                .iter_mut()
                .zip(["tier_7d_status", "tier_14d_status", "tier_28d_status"])
        {
            if let Some(s) = redemp.get(key).and_then(Value::as_str) {
                slot.1 = s.to_string();
            }
        }
        out.tier_statuses = statuses;
    }

    // 3. 抽奖次数：`(d.get("data") or {}).get("chances", 0)`
    if let Some(doc) = get_json(
        client,
        &format!("{}/activity/growth/lottery/summary", ep.chat_base),
        acc.chat_headers,
    )
    .await
    {
        if let Some(n) = doc
            .get("data")
            .and_then(|d| d.get("chances"))
            .and_then(Value::as_i64)
        {
            out.lottery_chances = Some(n);
        }
    }

    // 4. 猫猫旅行：`out["travel"] = d.get("data") or {}`（非对象回落 unknown）
    if let Some(doc) = get_json(
        client,
        &format!("{}/v2/activity/growth/buddy/travel/status", ep.chat_base),
        acc.chat_headers,
    )
    .await
    {
        out.travel = TravelStatus::from_upstream_data(doc.get("data"));
    }

    out
}

/// 单段 GET + 解析；非 2xx / 网络错 / 解析失败一律 `None`（对齐 Python 的
/// `except Exception` 只记日志）。`code` 不参与判定：Python 直接读 `data`。
async fn get_json(
    client: &wb_gateway::outbound::UpstreamClient,
    url: &str,
    headers: &HashMap<String, String>,
) -> Option<Value> {
    let resp = client.get(url, headers).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}
