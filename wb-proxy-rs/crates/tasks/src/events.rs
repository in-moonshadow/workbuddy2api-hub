//! 关卡 4.3：事件构造与上报（契约 6 / 7 + R1 双路径）。
//!
//! 权威参照 `wb_tasks.py`：
//! - `build_event`（:321-387）—— 8 种事件形状 + 专家/团队 id 池轮换 + 未知 kind 兜底；
//! - `report_events`（:390-408）—— `POST /v2/report`，body 为**裸数组**（不是 `{"events": [...]}`）；
//! - `_travel_status`（:411-416）与 `fetch_growth_summary` 内的旅行段 —— **R1 双路径**。
//!
//! ## R1（用户 2026-10-17 裁定「都保留、各带测试」）
//!
//! Python 有两处取旅行状态，路径前缀不同，两者都真实存在、都不能删：
//!
//! | 调用点 | 路径 |
//! |---|---|
//! | `_travel_status()`（:412） | `/activity/growth/buddy/travel/status` |
//! | `fetch_growth_summary()`（:180） | `/v2/activity/growth/buddy/travel/status` |
//!
//! 因此本模块**刻意不抽公共路径常量**：两个函数各写各的字符串字面量，各自带一个
//! 逐字断言 path 的用例。合并成常量后一旦「统一」就会静默打断其中一条。

use std::time::Duration;

use serde_json::json;

use crate::{GrowthSummary, TravelStatus};

/// 事件上报的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_tasks.py:404` 的 `timeout=15`。
const REPORT_TIMEOUT: Duration = Duration::from_secs(15);

/// 猫猫旅行状态的 HTTP 超时（秒）。
///
/// 对齐 Python `wb_tasks.py:414`（`/activity/growth/buddy/travel/status`）与
/// `:181`（`/v2/activity/growth/buddy/travel/status`）的 `timeout=10`。
const TRAVEL_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// chat 侧上游（`wb_tasks.CHAT_BASE`，:17）。
pub const CHAT_BASE: &str = "https://copilot.tencent.com";
/// 网页侧上游（`wb_tasks.WEB_BASE`，:19）—— claim 的 HTTP 400 降级目标。
pub const WEB_BASE: &str = "https://www.workbuddy.cn";
/// 计费侧上游（`wb_tasks.BILL_BASE`，:18）。
pub const BILL_BASE: &str = "https://www.codebuddy.cn";

/// 专家 id 池（`wb_tasks.EXPERT_ID_POOL`，:86-96，18 条，顺序即 Python 列表序）。
///
/// `_progress_one`（:589）按 `id_pool[(cur + i) % len(id_pool)]` 取——
/// 起点是任务的**当前进度**而不是 0，所以池的顺序是行为的一部分。
pub const EXPERT_ID_POOL: [(&str, &str); 18] = [
    ("ex_PZw8Gu81HfN4", "运维工程师"),
    ("ex_ROsDtJbzADFV", "产品经理"),
    ("ex_SMUnl0nJbPix", "UI设计师"),
    ("ex_ZTR062oVBOCW", "数据分析师"),
    ("ex_a3sSSFBy8qaC", "后端架构师"),
    ("ex_aG1kvKbq8lPx", "文案策划"),
    ("ex_al1vxtUOYQ10", "测试专家"),
    ("ex_cZfiyuET9UQP", "安全顾问"),
    ("ex_eggOvQuVP0hq", "算法工程师"),
    ("ex_hSwsQjkSKnkX", "前端工程师"),
    ("ex_mMbwwmFA9n9P", "项目管理专家"),
    ("ex_uAQE5POfk7Zh", "增长运营专家"),
    ("ex_uZzSAScSy7FZ", "行业研究员"),
    ("ex_LHywGrZOtG7G", "数据分析师"),
    ("ex_NX5C8GBciVed", "测试架构师"),
    ("ex_DdCsaoq4AtcO", "云端运维专家"),
    ("ex_KzqKQguubrNQ", "内容创作专家"),
    ("ex_2cvvUZQhDyeJ", "腾讯轻量云专家"),
];

/// 团队 id 池（`wb_tasks.TEAM_ID_POOL`，:99-104，8 条）。
pub const TEAM_ID_POOL: [(&str, &str); 8] = [
    ("CloudOpsTeam", "运维专家团队"),
    ("CloudContentTeam", "内容专家团队"),
    ("CloudDevTeam", "研发专家团队"),
    ("ProductStrategyTeam", "产品战略团队"),
    ("MarketingCampaignTeam", "营销活动团队"),
    ("SalesBattleTeam", "销售作战团队"),
    ("DesignEngineTeam", "设计引擎团队"),
    ("HrOperationsTeam", "人力运营团队"),
];

/// 为一次上报生成去重标识（契约 6：上游按 `(eventCode, id)` 去重）。
///
/// Python：`cid = f"wb-task-{now_ms}-{idx}"`、`rid = f"{cid}-req"`（:327-329）。
/// `now_ms` 由调用方传入而非函数内取时钟——测试要能钉住值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventIds {
    /// `wb-task-{ms}-{idx}`：`conversationId`。
    pub conversation_id: String,
    /// `{cid}-req`：`requestId`。
    pub request_id: String,
}

impl EventIds {
    /// `wb-task-{now_ms}-{idx}` / `wb-task-{now_ms}-{idx}-req`。
    pub fn new(now_ms: i64, idx: usize) -> Self {
        let conversation_id = format!("wb-task-{now_ms}-{idx}");
        let request_id = format!("{conversation_id}-req");
        Self {
            conversation_id,
            request_id,
        }
    }
}

/// 专家 / 团队事件的身份对（对应 Python `expert=(ex_id, name)`）。
pub type Expert = (&'static str, &'static str);

/// 构造一条上报事件（`wb_tasks.build_event`，:321-387）。
///
/// - `kind`：**不做枚举校验**，与 Python 一致——未知值走 `heartbeat` 兜底。
/// - `idx`：同一批内的序号，只影响 `cid` / `rid` 与 `template.id`。
/// - `expert`：仅 `expert`/`team` 生效；`lighthouse` 忽略该参数
///   （对应 Python :346-351 的分支顺序）。
///
/// 返回值的**顶层键序与 Python dict 字面量逐字一致**（依赖 `preserve_order`）。
pub fn build_event(
    uid: &str,
    kind: &str,
    idx: usize,
    now_ms: i64,
    expert: Option<Expert>,
) -> serde_json::Value {
    let ids = EventIds::new(now_ms, idx);
    let cid = &ids.conversation_id;
    let rid = &ids.request_id;

    match kind {
        "canvas" => json!({
            "eventCode": "wbx_design_canvas_task_create",
            "timestamp": now_ms,
            "reportDelay": 0,
            "conversationId": cid,
            "requestId": rid,
            "source": "summon_keyword",
            "isCustomModel": false,
            "name": "",
            "inputLength": 12,
            // canvas 的 id 不用 cid，而是独立的 `wbx-canvas-{ms}`（:336）
            "id": format!("wbx-canvas-{now_ms}"),
            "cost": 0,
            "isSuccessful": true,
            "userId": uid,
        }),
        "template" => json!({
            "eventCode": "agent_task_created_with_template",
            "timestamp": now_ms,
            "reportDelay": 0,
            "isCustomModel": true,
            // Python `str(idx)`：字符串而非数字（:340）
            "id": idx.to_string(),
            "name": "幻灯片",
            "requestId": rid,
            "conversationId": cid,
            "userId": uid,
        }),
        "expert" | "team" | "lighthouse" => {
            // Python :343：team → "team"，其余（expert/lighthouse）→ "agent"
            let etype = if kind == "team" { "team" } else { "agent" };
            let (ex_id, name) = match (kind, expert) {
                // lighthouse 的默认值优先级高于传入的 expert（Python :346-351）
                ("lighthouse", _) => ("ex_2cvvUZQhDyeJ", "腾讯轻量云专家"),
                ("team", Some(e)) => e,
                ("team", None) => ("CloudOpsTeam", "运维专家团队"),
                (_, Some(e)) => e,
                (_, None) => ("ContentCreator", "内容创作专家"),
            };
            json!({
                "eventCode": "expert_actual_use",
                "timestamp": now_ms,
                "reportDelay": 0,
                "mode": "CLOUD",
                "id": ex_id,
                "name": name,
                "expertTitle": name,
                "type": "02-Engineering",
                "expertType": etype,
                "source": "builtin",
                "version": "1.0.2",
                "cost": 0,
                "characterCount": 12,
                "conversationId": cid,
                "requestId": rid,
                "messageId": rid,
                "requestModelId": "deepseek-v4-flash",
                "requestModelName": "DeepSeek V4 Flash",
                "userId": uid,
            })
        }
        "skill" => json!({
            "eventCode": "skill_info",
            "timestamp": now_ms,
            "reportDelay": 0,
            "skillId": "skill_2096525080079265792",
            "name": "pptx",
            "userId": uid,
        }),
        "automation" => json!({
            "eventCode": "automated_task_create_suc",
            "timestamp": now_ms,
            "reportDelay": 0,
            "name": "每周工作整理",
            "type": "cron",
            "source": "manually",
            "modelId": "deepseek-v4-flash",
            "modelIsThinking": false,
            "conversationId": cid,
            "requestId": rid,
            "schedule": {
                "type": "recurring",
                // Python 是普通字符串，原样保留
                "rrule": "FREQ=WEEKLY;BYDAY=FR;BYHOUR=9;BYMINUTE=0",
            },
            "prompt": "每周五自动整理本周工作",
            "userId": uid,
        }),
        "playbook" => json!({
            "eventCode": "playbook_prompt_send",
            "timestamp": now_ms,
            "reportDelay": 0,
            "id": "worker-ledger-freedom-dashboard",
            "name": "打工人小账本",
            "type": "other",
            "promptLength": 10,
            // Python 是整数 1，不是 true（:371）
            "isOfficial": 1,
            "source": "discover",
            "conversationId": cid,
            "requestId": rid,
            "userId": uid,
        }),
        "skin" => json!({
            "eventCode": "appearance_skin_apply",
            "timestamp": now_ms,
            "reportDelay": 0,
            "action": "apply",
            "source": "settings_close",
            "id": "theme-tkmw7j",
            "vipLevel": "free",
            "series": "craft",
            "type": "unknown",
            "name": "和平精英激战金秋",
            "userId": uid,
        }),
        "chat" | "glmchat" | "cat" => {
            // Python :379-381：glmchat/cat 用 GLM，chat 用 DeepSeek；仅 cat 是 night
            let is_glm = kind == "glmchat" || kind == "cat";
            let (m_id, m_nm) = if is_glm {
                ("glm-5.2", "GLM-5.2")
            } else {
                ("deepseek-v4-flash", "DeepSeek V4 Flash")
            };
            let mode = if kind == "cat" { "night" } else { "craft" };
            json!({
                "eventCode": "chat_request_send",
                "timestamp": now_ms,
                "reportDelay": 0,
                "mode": mode,
                "conversationId": cid,
                "requestId": rid,
                "inputLength": 12,
                "requestModelId": m_id,
                "requestModelName": m_nm,
                "isPlan": false,
                "agentName": "default",
                "agentType": "conversation",
                "userId": uid,
            })
        }
        // 未知 kind 兜底（:387）：**只有 3 个键**，不要按其他分支补全字段
        _ => json!({
            "eventCode": "heartbeat",
            "timestamp": now_ms,
            "userId": uid,
        }),
    }
}

/// `_progress_one` 里专家/团队 id 的取法（`wb_tasks.py:584-590`）。
///
/// `kind` 为 `expert` → `EXPERT_ID_POOL`；`team` → `TEAM_ID_POOL`；其余 → `None`。
/// 下标是 `(current + i) % len(pool)`——起点取自任务当前进度，不是 0，
/// 因此同一任务跨轮次会自然换人，这正是上游按 `(eventCode, id)` 去重所需的。
pub fn rotating_expert(kind: &str, current: i64, i: i64) -> Option<Expert> {
    let pool: &[(&str, &str)] = match kind {
        "expert" => &EXPERT_ID_POOL,
        "team" => &TEAM_ID_POOL,
        _ => return None,
    };
    // 用 i128 避免 current+i 为负时取模得到负下标
    let len = pool.len() as i128;
    let idx = ((current as i128 + i as i128) % len + len) % len;
    Some(pool[idx as usize])
}

/// 上报事件数组（`wb_tasks.report_events`，:390-408）。
///
/// - `POST {url_base}/v2/report`，body 为**裸数组**（`json.dumps(events)`）；
/// - 头走 `purpose="chat"`（由调用方传入）；
/// - 成功判定只有 `d.get("code") == 0`；**任何异常（含非 2xx、JSON 解析失败）→ `false`**，
///   不区分原因（对齐 Python :407-408 的 `except Exception: return False`）。
pub async fn report_events(
    client: &wb_gateway::outbound::UpstreamClient,
    url_base: &str,
    headers: &std::collections::HashMap<String, String>,
    events: &[serde_json::Value],
) -> bool {
    let url = format!("{url_base}/v2/report");
    // Python `json.dumps(events)`：数组根，不是对象
    let body = serde_json::Value::Array(events.to_vec());
    match client
        .post_with_timeout(&url, headers, &body, REPORT_TIMEOUT)
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(d) => d.get("code").and_then(serde_json::Value::as_i64) == Some(0),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 旅行状态查询（R1 路径一）：`_travel_status`（:411-416）。
///
/// 路径**逐字**为 `/activity/growth/buddy/travel/status`（**无** `/v2` 前缀）。
/// 与 [`growth_summary_travel`] 是两条独立路径，**不共享常量**。
pub async fn travel_status(
    client: &wb_gateway::outbound::UpstreamClient,
    url_base: &str,
    headers: &std::collections::HashMap<String, String>,
) -> Result<TravelStatus, String> {
    let url = format!("{url_base}/activity/growth/buddy/travel/status");
    match client
        .get_with_timeout(&url, headers, TRAVEL_STATUS_TIMEOUT)
        .await
    {
        Ok(resp) => {
            // Python `urlopen` 对非 2xx 抛 HTTPError；这里对齐为 Err
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status().as_u16()));
            }
            match resp.json::<serde_json::Value>().await {
                Ok(d) => Ok(TravelStatus::from_upstream_data(d.get("data"))),
                Err(e) => Err(format!("travel status json: {e}")),
            }
        }
        Err(e) => Err(format!("travel status: {e}")),
    }
}

/// growth 汇总的**旅行段**（`fetch_growth_summary`，:143-190）：R1 路径二。
///
/// 路径**逐字**为 `/v2/activity/growth/buddy/travel/status`（**带** `/v2` 前缀）。
/// 其余三段（energy / streak+tiers / lottery）在 4.7 快照缓存一并接入；
/// 本函数返回的 `GrowthSummary` 只有 `travel` 有值，其余为 `Default`。
pub async fn growth_summary_travel(
    client: &wb_gateway::outbound::UpstreamClient,
    url_base: &str,
    headers: &std::collections::HashMap<String, String>,
) -> Result<GrowthSummary, String> {
    let url = format!("{url_base}/v2/activity/growth/buddy/travel/status");
    match client
        .get_with_timeout(&url, headers, TRAVEL_STATUS_TIMEOUT)
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status().as_u16()));
            }
            match resp.json::<serde_json::Value>().await {
                Ok(d) => Ok(GrowthSummary {
                    travel: TravelStatus::from_upstream_data(d.get("data")),
                    ..GrowthSummary::default()
                }),
                Err(e) => Err(format!("travel summary json: {e}")),
            }
        }
        Err(e) => Err(format!("travel summary: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const UID: &str = "u_test";
    const NOW: i64 = 1_760_000_000_000;

    /// 契约 6：`cid` / `rid` 的字面格式。
    #[test]
    fn event_ids_follow_python_format() {
        let ids = EventIds::new(NOW, 3);
        assert_eq!(ids.conversation_id, format!("wb-task-{NOW}-3"));
        assert_eq!(ids.request_id, format!("wb-task-{NOW}-3-req"));
    }

    /// 契约 7：8 种 kind 的 eventCode 与 Python 逐一对应。
    #[test]
    fn each_kind_maps_to_python_event_code() {
        let cases = [
            ("canvas", "wbx_design_canvas_task_create"),
            ("template", "agent_task_created_with_template"),
            ("expert", "expert_actual_use"),
            ("team", "expert_actual_use"),
            ("lighthouse", "expert_actual_use"),
            ("skill", "skill_info"),
            ("automation", "automated_task_create_suc"),
            ("playbook", "playbook_prompt_send"),
            ("skin", "appearance_skin_apply"),
            ("chat", "chat_request_send"),
            ("glmchat", "chat_request_send"),
            ("cat", "chat_request_send"),
        ];
        for (kind, expected) in cases {
            let ev = build_event(UID, kind, 0, NOW, None);
            assert_eq!(ev["eventCode"], expected, "kind={kind}");
        }
    }

    /// 未知 kind 兜底 `heartbeat`：**只有 3 个键**（Python :387）。
    #[test]
    fn unknown_kind_falls_back_to_three_key_heartbeat() {
        let ev = build_event(UID, "no-such-kind", 0, NOW, None);
        assert_eq!(
            ev,
            json!({"eventCode": "heartbeat", "timestamp": NOW, "userId": UID})
        );
        let mut keys = ev.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, vec!["eventCode", "timestamp", "userId"]);
    }

    /// `preserve_order`：canvas 的键序与 Python 字面量逐字一致。
    #[test]
    fn canvas_key_order_matches_python_literal() {
        let ev = build_event(UID, "canvas", 0, NOW, None);
        let keys: Vec<&str> = ev.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "eventCode",
                "timestamp",
                "reportDelay",
                "conversationId",
                "requestId",
                "source",
                "isCustomModel",
                "name",
                "inputLength",
                "id",
                "cost",
                "isSuccessful",
                "userId",
            ]
        );
        // canvas 的 id 是独立的 `wbx-canvas-{ms}`
        assert_eq!(ev["id"], format!("wbx-canvas-{NOW}"));
        assert_eq!(ev["timestamp"], NOW);
    }

    /// 类型细节：`template.id` 是**字符串**、`playbook.isOfficial` 是**整数 1**。
    #[test]
    fn template_id_is_string_and_playbook_is_official_is_int() {
        let t = build_event(UID, "template", 7, NOW, None);
        assert_eq!(t["id"], "7");
        assert!(t["id"].is_string());

        let p = build_event(UID, "playbook", 0, NOW, None);
        assert_eq!(p["isOfficial"], 1);
        assert!(p["isOfficial"].is_i64());
    }

    /// 专家/团队/lighthouse 的默认值与 `expertType`。
    #[test]
    fn expert_defaults_and_expert_type() {
        let e = build_event(UID, "expert", 0, NOW, None);
        assert_eq!(e["id"], "ContentCreator");
        assert_eq!(e["expertType"], "agent");

        let t = build_event(UID, "team", 0, NOW, None);
        assert_eq!(t["id"], "CloudOpsTeam");
        assert_eq!(t["expertType"], "team");

        let l = build_event(UID, "lighthouse", 0, NOW, None);
        assert_eq!(l["id"], "ex_2cvvUZQhDyeJ");
        assert_eq!(l["expertType"], "agent");

        // 传入 expert 时 expert/team 采用；lighthouse 仍用默认（Python :346-351）
        let ex = Some(("ex_custom", "自定义专家"));
        assert_eq!(build_event(UID, "expert", 0, NOW, ex)["id"], "ex_custom");
        assert_eq!(build_event(UID, "team", 0, NOW, ex)["id"], "ex_custom");
        assert_eq!(
            build_event(UID, "lighthouse", 0, NOW, ex)["id"],
            "ex_2cvvUZQhDyeJ"
        );
    }

    /// chat 三兄弟的模型与 mode 差异。
    #[test]
    fn chat_glmchat_cat_differ_in_model_and_mode() {
        let chat = build_event(UID, "chat", 0, NOW, None);
        assert_eq!(chat["mode"], "craft");
        assert_eq!(chat["requestModelId"], "deepseek-v4-flash");

        let glm = build_event(UID, "glmchat", 0, NOW, None);
        assert_eq!(glm["mode"], "craft");
        assert_eq!(glm["requestModelId"], "glm-5.2");
        assert_eq!(glm["requestModelName"], "GLM-5.2");

        let cat = build_event(UID, "cat", 0, NOW, None);
        assert_eq!(cat["mode"], "night");
        assert_eq!(cat["requestModelId"], "glm-5.2");
    }

    /// 契约 6：同一批内 `(eventCode, id)` 不重复。
    ///
    /// 断言范围刻意只在**同一毫秒批次**内：跨批次的同 idx 可能合法重名
    /// （毫秒分辨率），不作为契约。
    ///
    /// ⚠️ `canvas` 的 `id` 是 `wbx-canvas-{ms}`、**不含 idx**，因此同一毫秒内
    /// 多条 canvas 会撞——这与 Python 一致（`_progress_one` 每条之间 `sleep(gap)`，
    /// `gap` 默认 1.0 秒，天然跨毫秒）。故 canvas 用递增毫秒单独验证。
    ///
    /// 契约 6：同一批内 `(eventCode, id)` 不重复 —— **仅对带轮换 id 的分支**。
    ///
    /// 逐 kind 的「去重键」事实（全部来自 `wb_tasks.build_event` 字面量）：
    ///
    /// | kind | 去重键 | 同批多条是否唯一 |
    /// |---|---|---|
    /// | `template` | `id = str(idx)` | ✅ |
    /// | `expert` / `team` | `id = 池轮换` | ✅（池长内） |
    /// | `chat` / `glmchat` / `cat` | `conversationId = cid(idx)` | ✅ |
    /// | `automation` | `conversationId = cid(idx)` | ✅ |
    /// | `canvas` | `id = wbx-canvas-{ms}` | ❌ 只随毫秒变，不随 idx |
    /// | `playbook` | `id` 为**固定常量** | ❌ 固定值 |
    /// | `skill` / `skin` | 无 `id`、无 `conversationId` | ❌ 无键 |
    ///
    /// 后三类的行为与 Python 一致（`_progress_one` 每条之间 `sleep(gap)`，
    /// `gap` 默认 1.0 秒；且这三类任务的目标次数本身就不靠 id 区分）。
    #[test]
    fn ids_are_unique_within_one_batch() {
        // 只有这些 kind 的去重键随 idx 变
        let kinds = ["template", "expert", "team", "automation", "chat"];
        let mut seen = HashSet::new();
        for kind in kinds {
            let bound = match kind {
                "expert" => EXPERT_ID_POOL.len(),
                "team" => TEAM_ID_POOL.len(),
                _ => EXPERT_ID_POOL.len(),
            };
            for idx in 0..bound {
                let expert = rotating_expert(kind, 0, idx as i64);
                let ev = build_event(UID, kind, idx, NOW, expert);
                let code = ev["eventCode"].as_str().unwrap().to_string();
                let id = ev
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        ev.get("conversationId")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                assert!(
                    seen.insert((code.clone(), id.clone())),
                    "duplicate ({code}, {id}) at kind={kind} idx={idx}"
                );
            }
        }

        // canvas：id 只由毫秒决定，跨毫秒必须唯一
        let mut canvas_ids = HashSet::new();
        for step in 0..5 {
            let ev = build_event(UID, "canvas", step, NOW + step as i64, None);
            assert!(
                canvas_ids.insert(ev["id"].as_str().unwrap().to_string()),
                "canvas id must change with ms"
            );
        }
    }

    /// 专家池轮换：下标从任务当前进度起算，且绕圈回到池首。
    #[test]
    fn rotating_expert_starts_from_current_progress_and_wraps() {
        let len = EXPERT_ID_POOL.len() as i64;
        assert_eq!(rotating_expert("expert", 0, 0), Some(EXPERT_ID_POOL[0]));
        assert_eq!(rotating_expert("expert", 3, 0), Some(EXPERT_ID_POOL[3]));
        assert_eq!(
            rotating_expert("expert", len - 1, 1),
            Some(EXPERT_ID_POOL[0]),
            "跨圈应回到池首"
        );
        assert_eq!(rotating_expert("team", 1, 0), Some(TEAM_ID_POOL[1]));
        assert_eq!(rotating_expert("canvas", 0, 0), None);
    }

    fn chat_headers() -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("X-Domain".into(), "copilot.tencent.com".into());
        h
    }

    /// 契约 3：`/v2/report` 的 body 是**裸数组**，不是对象。
    #[tokio::test]
    async fn report_events_posts_bare_array_and_reads_code_zero() {
        let server = MockServer::start().await;
        let expected = json!([{
            "eventCode": "heartbeat",
            "timestamp": NOW,
            "userId": UID,
        }]);
        Mock::given(method("POST"))
            .and(path("/v2/report"))
            .and(header("X-Domain", "copilot.tencent.com"))
            .and(body_json(expected))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;

        let client = wb_gateway::outbound::UpstreamClient::new();
        let events = vec![build_event(UID, "heartbeat", 0, NOW, None)];
        let ok = report_events(&client, &server.uri(), &chat_headers(), &events).await;
        assert!(ok);

        // 再确认落到 wire 上的根确实是数组
        let reqs: Vec<Request> = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body.is_array(), "report body must be a bare array");
    }

    /// 契约 3：`code != 0` → false；非 2xx → false；两者都不 panic。
    #[tokio::test]
    async fn report_events_returns_false_on_nonzero_code_and_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/report"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 10086})))
            .mount(&server)
            .await;
        let client = wb_gateway::outbound::UpstreamClient::new();
        let events = vec![build_event(UID, "heartbeat", 0, NOW, None)];
        assert!(!report_events(&client, &server.uri(), &chat_headers(), &events).await);

        let server2 = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server2)
            .await;
        assert!(!report_events(&client, &server2.uri(), &chat_headers(), &events).await);
    }

    // -----------------------------------------------------------------
    // R1（用户裁定「都保留、各带测试」）：两条旅行路径各一个用例
    // -----------------------------------------------------------------

    /// R1 路径一：`_travel_status` → `/activity/growth/buddy/travel/status`（无 `/v2`）。
    #[tokio::test]
    async fn r1_travel_status_uses_non_v2_prefix() {
        let server = MockServer::start().await;
        // 只挂无 /v2 的路径；若实现走错前缀，wiremock 会 404
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "traveling"}})),
            )
            .mount(&server)
            .await;

        let client = wb_gateway::outbound::UpstreamClient::new();
        let t = travel_status(&client, &server.uri(), &chat_headers())
            .await
            .expect("travel_status must succeed");
        assert_eq!(t.to_json()["state"], "traveling");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].url.path(),
            "/activity/growth/buddy/travel/status",
            "R1 路径一必须是不带 /v2 的前缀"
        );
    }

    /// R1 路径二：`fetch_growth_summary` 的旅行段 → `/v2/activity/growth/buddy/travel/status`。
    #[tokio::test]
    async fn r1_growth_summary_travel_uses_v2_prefix() {
        let server = MockServer::start().await;
        // 只挂带 /v2 的路径；若实现走错前缀，wiremock 会 404
        Mock::given(method("GET"))
            .and(path("/v2/activity/growth/buddy/travel/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"state": "resting"}})),
            )
            .mount(&server)
            .await;

        let client = wb_gateway::outbound::UpstreamClient::new();
        let s = growth_summary_travel(&client, &server.uri(), &chat_headers())
            .await
            .expect("growth_summary_travel must succeed");
        assert_eq!(s.travel.to_json()["state"], "resting");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].url.path(),
            "/v2/activity/growth/buddy/travel/status",
            "R1 路径二必须是带 /v2 的前缀"
        );
    }

    /// 上游缺 `data` 时回落到 `{"state": "unknown"}`（TravelStatus 缺省语义）。
    #[tokio::test]
    async fn travel_status_missing_data_falls_back_to_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/activity/growth/buddy/travel/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
            .mount(&server)
            .await;

        let client = wb_gateway::outbound::UpstreamClient::new();
        let t = travel_status(&client, &server.uri(), &chat_headers())
            .await
            .expect("must succeed");
        assert_eq!(t.to_json(), json!({"state": "unknown"}));
    }
}
