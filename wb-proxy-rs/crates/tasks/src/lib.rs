//! `wb-tasks` —— M4 成长任务 / 签到 / 领奖引擎的**数据形状与常量表**。
//!
//! 权威参照 Python `wb_tasks.py`。本 crate 分层：
//!
//! - 本文件（关卡 4.1）：纯数据 —— `GrowthTask`（契约 2 的 12 字段）、
//!   `AcceptOutcome` / `ClaimOutcome` / `TravelStatus` / `GrowthSummary`，
//!   以及 `TASK_SPECS` / `DESKTOP_ONLY_TASKS` / `NIGHT_TASK_CODES` 等常量表。
//! - `events` 子模块（关卡 4.3）：`build_event` 的 8 种事件形状、专家/团队 id 池轮换、
//!   `wb-task-{ms}-{idx}` 去重 id 生成、`report_events` 裸数组上报。
//! - `checkin` 子模块（关卡 4.6）：国内版签到与对话活跃上报。**R4 修正**——
//!   上游未确认成功时不写 `lastCheckin`/`lastActivityReport`（Python 无条件写盘，
//!   故本处为有意的行为差异，记入 4.11 账本）。
//! - `scheduler` 子模块（关卡 4.8，权威源是 `wb_scheduler.py` 而非 `wb_tasks.py`）：
//!   小时表 `[1, 9, 21, 22]`、30s 轮询 / 65s 命中抑制、`calc_next_fire`、非阻塞
//!   互斥、`trigger_now`、`status()` 七键。执行动作与面板日志均以 trait 注入：
//!   [`scheduler::CycleExecutor`] / [`scheduler::PanelSink`] —— 这样 `wb-tasks`
//!   仍不反向依赖 `wb-api`，测试也不发任何真实上游请求。
//! - `claim_all` 子模块（关卡 4.9，权威源是 `wb_proxy.py:17306-17396` 的
//!   `_route_tasks_claim_all`）：目标集筛选（cn/intl）、**intl 在前**的执行顺序、
//!   三档守卫、每账号五步（签到 → 活跃上报 → 成长任务 → 连登管家 → 猫猫日常）、
//!   单账号失败不中断其余账号、步骤间与账号间的 `sleep`、四键返回体。
//! - 后续关卡：HTTP 抓取（4.4）、接取/领奖（4.5）、领奖编排（4.7）、
//!   路由接线（4.10）。
//!
//! ⚠️ 本文件的 JSON 键名与顺序刻意**逐字对齐 Python dict 字面量**，不用
//! `derive(Serialize)`：`serde` 的字段顺序虽然也按声明序，但改用 `json!` 可以
//! 让键名与 Python 源码并排比对，改动时不易漂移。

use serde_json::{json, Value};

pub mod checkin;
// 关卡 4.19（M5 数据侧）：面板/设置/用量这三条线的数据与纯逻辑。
//   `credits`  积分快照的形状、解析与两条聚合口径（HTTP 那一半在
//               `wb_gateway::credits`）；
//   `activity` 账号每日活动的结构化历史（JSONL 投影、筛选、保留策略）；
//   `cap`      模型额度 cap（code 6004）的判定层；
//   `price`    定价快照与等价成本估算（OpenRouter 价目）；
//   `update`   更新发现（只发现，不安装）。
// 权威源分别是 Python `wb_accounts.py` / `wb_activity.py` / `wb_pricing.py` /
// `wb_updates.py`。
pub mod activity;
pub mod cap;
pub mod credits;
pub mod price;
pub mod update;
// 关卡 4.9：claim-all 编排（`_route_tasks_claim_all`）。步骤全部经
// [`scheduler::CycleExecutor`] 与 `growth_tasks` 接缝注入，测试不发真实请求。
pub mod api_routes;
pub mod claim_all;
// 关卡 4.24（线 B）：OAuth 登录注册表。
pub mod login;
// 关卡 4.14：国际版每日活跃（daily_chat）——桌面腿 + 网页腿 ACP 会话。
// 权威源是 `wb_accounts.py:1449-1661`；ACP 协议在 `wb_gateway::acp`。
pub mod daily_chat;
pub mod events;
pub mod growth_api;
pub mod growth_runner;
pub mod scheduler;
pub mod snapshot_cache;
// 关卡 4.18：夜猫子任务（night_growth）——Python `wb_tasks.run_night_growth`
// 的逐分支移植：夜间窗口判定 → 接取 → 上报夜间事件 → 轮询进度 → 达标领奖。
// 权威源是 `wb_tasks.py:712-770`。
pub mod night_growth;
pub mod summary;
// 关卡 4.17：连登管家（streak_bonus）——Python `wb_tasks.run_streak_bonus`
// 的四步移植：补签保连登 → 礼包/补偿 → 7d/14d/28d 档位兑换 → 自动抽奖闭环。
// 权威源是 `wb_tasks.py:771-879`；旧的 Rust 实现调的是上游不存在的
// `POST /activity/growth/streak/claim`（Python 全仓 0 命中），本模块取代之。
pub mod streak_bonus;
// 关卡 4.18（线 B）：刷新类后台任务 —— `settings` 是刷新类任务用到的
// settings 键位口径（`credits_refresh_hours` / `pricing_refresh_minutes` /
// `pricing_enabled` / `update_check_enabled`），`credits_refresher` 是后台积分
// 刷新线程。权威源分别是 Python `wb_settings.py` 与 `wb_accounts.py` 的
// `CreditsRefresher`。
pub mod credits_refresher;
pub mod settings;
// 面板设置的**读视图与保存**（Python `wb_proxy.runtime_settings_view()` /
// `wb_proxy._handle_settings_save()` 的等价物）。权威源是 `wb_settings.py` 与
// `wb_proxy.py`；本模块只负责落盘 + 校验 + 算出生效值，后台线程的副作用由
// 调用方按返回值里的增量键触发。
pub mod settings_view;
// 关卡 4.26：proxy slots 能力层（Python `wb_proxy._handle_proxy_slots` /
// `proxy_slots_view` / `wb_settings._clean_slot_entry` 等）。只读写
// settings.json 的 `proxy_slots` 数组并做纯数据清洗；POOL 绑定归接线层。
pub mod proxy;
// 关卡 4.19：另外两个后台线程 + 用量聚合。`price_refresher` 是 Python
// `wb_pricing.PriceRefresher` 的移植，`update_checker` 是 Python
// `wb_updates.UpdateChecker` 的移植，`usage` 是用量落盘与聚合
// （Python `wb_proxy.record_usage` 与 `usage_snapshot` 等）。
pub mod price_refresher;
// 关卡 4.21：`pricing_store` 是定价策略库的**读取端**（写端在
// `price_refresher`）；`live_catalog` 是取价清单的 live 目录那一半
// （静态目录在 `catalog`）；`live_source` 是 live 目录的**生产来源**
// （用账号池凭据拉远端产品目录）。三个模块的声明一次性加好，避免并行线抢
// `lib.rs`。
pub mod live_catalog;
pub mod live_source;
// 关卡 4.25：面板日志缓冲（ApiPanelSink 接入；/logs、/logs/export、/logs/clear）。
pub mod logs;
pub mod pricing_store;
// 关卡 4.19（主会话）：`price_refresher` 的**生产数据源**——settings 读总
// 开关（缺键=开）、`usage/` 写盘、OpenRouter 取价。
pub mod price_source;
pub mod update_checker;
pub mod usage;
// 关卡 4.22（D72）：`/usage/remaining` 的剩余额度引擎（能力层）——Python
// `wb_proxy.py` 的 `_remaining_*` 系列 + `remaining_usage()` 移植。冷扫版：
// 每次调用重新 fold usage.jsonl + limit-events.jsonl，跨天归档 estimate-daily.json。
pub mod remaining;
// 关卡 4.18（线 C）：积分刷新的**生产数据源**——把 `wb_gateway::credits`
// （HTTP 那一半）与 `crate::credits`（塑形那一半）接起来，并把快照原子
// 写回账号文件。权威源是 Python `wb_accounts.Account.fetch_credits()`。
pub mod credits_source;
// 关卡 4.20（线 A）：hub 侧**静态模型目录**——Python `wb_catalog.py` 的
// `STATIC_INTL_MODELS` / `STATIC_CN_MODELS` 的 id 清单移植（只搬 id）。
pub mod catalog;
// 关卡 4.26（线 C）：agents 面板能力层——纯视图/校验，不发外部写。
// 权威源是 Python `wb_proxy.py` 的 agents 段与 `wb_agents.py`；写客户端
// config 的 integrate/restore 属集成引擎，留接线层/后续。
pub mod agents;

// ---------------------------------------------------------------------------
// 契约 2：growth task 的 12 字段归一化
// ---------------------------------------------------------------------------

/// 单条成长任务（`wb_tasks.fetch_growth_tasks` 每条 dict 的 12 个键）。
///
/// Python 侧字段来源（`wb_tasks.py:130-158`）：
/// `task_code` ← `task_code`；`name` ← `title` → spec.name → code；
/// `description` ← `description` → `task_desc`；`jump_url`；`status` ←
/// `accept_status`（缺省 `"not_accepted"`）；`task_type`；
/// `current`/`target` ← `progress`（缺省 0 / spec.target）；
/// `reward_credit` ← `reward_credit` → spec.reward；`reward_energy`（缺省 0）；
/// `unforgeable` ← spec.unforgeable 的布尔化；`reason` ← spec.reason（缺省空串）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowthTask {
    pub task_code: String,
    pub name: String,
    pub description: String,
    pub jump_url: String,
    pub status: String,
    pub task_type: String,
    pub current: i64,
    pub target: i64,
    pub reward_credit: i64,
    pub reward_energy: i64,
    pub unforgeable: bool,
    pub reason: String,
}

impl GrowthTask {
    /// 序列化为上游/前端可见的 JSON 对象，键名与 Python dict 逐字一致。
    pub fn to_json(&self) -> Value {
        json!({
            "task_code": self.task_code,
            "name": self.name,
            "description": self.description,
            "jump_url": self.jump_url,
            "status": self.status,
            "task_type": self.task_type,
            "current": self.current,
            "target": self.target,
            "reward_credit": self.reward_credit,
            "reward_energy": self.reward_energy,
            "unforgeable": self.unforgeable,
            "reason": self.reason,
        })
    }

    /// 从归一化后的 JSON 反序列化（用于契约测试与缓存回读）。
    ///
    /// 缺失字段按契约 2 的缺省值补齐，而不是报错 —— 上游随时可能少给键，
    /// 归一化必须容错到「字段齐全」。
    pub fn from_json(value: &Value) -> GrowthTask {
        let s = |key: &str| -> String {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let i = |key: &str, default: i64| -> i64 {
            value.get(key).and_then(Value::as_i64).unwrap_or(default)
        };
        let status = {
            let raw = s("status");
            if raw.is_empty() {
                "not_accepted".to_string()
            } else {
                raw
            }
        };
        GrowthTask {
            task_code: s("task_code"),
            name: s("name"),
            description: s("description"),
            jump_url: s("jump_url"),
            status,
            task_type: s("task_type"),
            current: i("current", 0),
            target: i("target", 1),
            reward_credit: i("reward_credit", 0),
            reward_energy: i("reward_energy", 0),
            unforgeable: value
                .get("unforgeable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reason: s("reason"),
        }
    }
}

// ---------------------------------------------------------------------------
// 任务规格表（wb_tasks.py:TASK_SPECS）
// ---------------------------------------------------------------------------

/// 任务规格：未完成任务时的兜底名称 / 目标值 / 奖励。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSpec {
    /// 完成该任务所需的事件「类」(canonical task type)。
    pub kind: &'static str,
    pub target: i64,
    pub reward: i64,
    pub name: &'static str,
    /// 上游只认真实动作、程序无法伪造（跳过上报，仅给深链）。
    pub unforgeable: bool,
    pub reason: &'static str,
}

const fn spec(
    kind: &'static str,
    target: i64,
    reward: i64,
    name: &'static str,
    unforgeable: bool,
    reason: &'static str,
) -> TaskSpec {
    TaskSpec {
        kind,
        target,
        reward,
        name,
        unforgeable,
        reason,
    }
}

/// 19 条任务规格，顺序与 Python dict 插入序一致（便于逐行比对）。
pub const TASK_SPECS: [(&str, TaskSpec); 19] = [
    (
        "create_canvas",
        spec("canvas", 1, 300, "创建设计任务", false, ""),
    ),
    (
        "template_5",
        spec("template", 5, 200, "模板创建任务", false, ""),
    ),
    (
        "expert_5",
        spec("expert", 5, 200, "使用专家助手", false, ""),
    ),
    (
        "Expert_team_use_3",
        spec("team", 3, 150, "使用专家团队", false, ""),
    ),
    ("skill_1", spec("skill", 1, 100, "体验技能", false, "")),
    (
        "automation_1",
        spec("automation", 1, 100, "创建自动化任务", false, ""),
    ),
    (
        "playbook_prompt",
        spec("playbook", 1, 100, "灵感案例使用", false, ""),
    ),
    (
        "Expert_lighthouse",
        spec("lighthouse", 1, 100, "轻量云专家使用", false, ""),
    ),
    (
        "Buddy_App",
        spec("buddy5", 1, 100, "进入 Buddy 应用", false, ""),
    ),
    (
        "Buddy_App_QQ",
        spec("buddy5", 1, 100, "企鹅教师助手", false, ""),
    ),
    (
        "Hp_Appearance",
        spec("skin", 1, 100, "应用主题外观", false, ""),
    ),
    ("chat_5", spec("chat", 5, 100, "发起 5 次对话", false, "")),
    (
        "Model_chat_GLM5.2",
        spec("glmchat", 1, 100, "体验 GLM-5.2", false, ""),
    ),
    (
        "black_cat",
        spec("cat", 3, 100, "夜猫子任务 (23:00-08:00)", false, ""),
    ),
    (
        "RichMeow_Chat",
        spec("richmeow", 1, 100, "桌面对话事件链", false, ""),
    ),
    (
        "Library_read",
        spec("library", 1, 100, "浏览资料库", false, ""),
    ),
    (
        "first_buddy",
        spec("chat", 1, 300, "领取一只 Buddy", false, ""),
    ),
    (
        "wb_wechat_oa_subscribe_task",
        // Python 该条**没有 `target` 键**，`spec.get("target", 1)` 得到 1；
        // 这里写 1 是该值的可观测等价物（写 100 会与旧的 reward 字段混淆）。
        spec(
            "unforgeable",
            1,
            100,
            "关注官方公众号",
            true,
            "需在微信关注「腾讯 WorkBuddy」公众号满 24 小时",
        ),
    ),
    (
        "Expert_Philanthropy",
        // 同上：Python 无 `target` 键 → 生效值 1（不是 0）。
        spec("unforgeable", 1, 0, "公益爱心捐赠", true, "真实捐款动作"),
    ),
];

/// 按 task_code 查规格。未知任务返回 `None`（Python 走 `.get()`）。
pub fn task_spec(task_code: &str) -> Option<&'static TaskSpec> {
    TASK_SPECS
        .iter()
        .find(|(code, _)| *code == task_code)
        .map(|(_, spec)| spec)
}

// ---------------------------------------------------------------------------
// 只认桌面端真实动作的任务（wb_tasks.py:DESKTOP_ONLY_TASKS）
// ---------------------------------------------------------------------------

/// 这些任务上游只认桌面客户端的真实行为信号（jump_url 均为 workbuddy:// 深链），
/// 伪造 `/v2/report` 事件会被忽略或落到 heartbeat，claim 必然返回 400
/// `task not completed`。因此诚实地跳过上报，只给出深链提示。
pub const DESKTOP_ONLY_TASKS: [(&str, &str); 4] = [
    ("RichMeow_Chat", "在桌面端发起 1 次对话"),
    ("Library_read", "在桌面端打开「资料库」并读完介绍文档"),
    (
        "Buddy_App",
        "在桌面端左上角「发现应用」进入任意一个 Buddy 应用",
    ),
    ("Buddy_App_QQ", "在桌面端「发现应用」进入「企鹅教师助手」"),
];

// ---------------------------------------------------------------------------
// 夜猫子时间窗（wb_tasks.py:NIGHT_TASK_CODES / in_night_window）
// ---------------------------------------------------------------------------

/// 夜猫子任务只在 23:00-08:00 上报才计数，且每天 1 次、累计 3 天。
pub const NIGHT_TASK_CODES: [&str; 1] = ["black_cat"];

/// 小时是否落在夜猫子窗口内（本地时区，`hour ∈ [0, 24)`）。
///
/// Python `in_night_window()` 用 `time.localtime().tm_hour`，即**本地时间**；
/// 调用方必须传入 `chrono::Local` 的小时，不得传 UTC。
pub fn in_night_window(hour: u32) -> bool {
    // Python `h >= 23 or h < 8`；clippy 要求写成区间否定形式，语义等价。
    !(8..23).contains(&hour)
}

// ---------------------------------------------------------------------------
// 接取结果（wb_tasks.accept_tasks）
// ---------------------------------------------------------------------------

/// `accept_tasks()` 的返回形状。
///
/// Python（`wb_tasks.py:250-292`）：`{"ok", "accepted", "failed", "msg", "reasons"}`；
/// 分块（chunk=20）后**部分失败不影响整体继续**，逐块把失败码并入 `failed`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptOutcome {
    pub ok: bool,
    pub accepted: Vec<String>,
    pub failed: Vec<String>,
    pub msg: String,
    pub reasons: Vec<String>,
}

impl AcceptOutcome {
    /// 空入参的返回值：Python 直接 `return out`，`ok` 为真。
    pub fn empty() -> AcceptOutcome {
        AcceptOutcome {
            ok: true,
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "accepted": self.accepted,
            "failed": self.failed,
            "msg": self.msg,
            "reasons": self.reasons,
        })
    }
}

// ---------------------------------------------------------------------------
// 领奖结果（wb_tasks.claim_task）
// ---------------------------------------------------------------------------

/// `claim_task()` 的返回形状。
///
/// Python 成功：`{"ok": True, "credit": …, "energy": …}` —— **没有 `msg` 键**；
/// 失败：额外带 `"msg"`。用 `Option<String>` 表达这个「键是否存在」的差异。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimOutcome {
    pub ok: bool,
    pub credit: i64,
    pub energy: i64,
    pub msg: Option<String>,
}

impl ClaimOutcome {
    pub fn success(credit: i64, energy: i64) -> ClaimOutcome {
        ClaimOutcome {
            ok: true,
            credit,
            energy,
            msg: None,
        }
    }

    pub fn failure(msg: impl Into<String>) -> ClaimOutcome {
        ClaimOutcome {
            ok: false,
            credit: 0,
            energy: 0,
            msg: Some(msg.into()),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "ok": self.ok,
            "credit": self.credit,
            "energy": self.energy,
        });
        if let (Some(msg), Some(obj)) = (&self.msg, out.as_object_mut()) {
            obj.insert("msg".to_string(), Value::String(msg.clone()));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 猫猫旅行状态（wb_tasks.fetch_growth_summary 的 travel 段）
// ---------------------------------------------------------------------------

/// 猫猫旅行状态：上游 `data` 对象**原样透传**，不做字段建模。
///
/// Python 只在两个地方用到它：`summary["travel"] = d.get("data") or {}`，
/// 以及单独接口 `/activity/growth/buddy/travel/status` 的 `data`。缺省值是
/// `{"state": "unknown"}` —— 前端据此区分「查不到」与「真的没在旅行」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelStatus(pub Value);

impl Default for TravelStatus {
    fn default() -> Self {
        TravelStatus(json!({"state": "unknown"}))
    }
}

impl TravelStatus {
    /// 上游返回 `{"data": {...}}`：非对象 / 缺失 → 缺省值。
    pub fn from_upstream_data(data: Option<&Value>) -> TravelStatus {
        match data {
            Some(Value::Object(_)) => TravelStatus(data.cloned().unwrap_or_default()),
            _ => TravelStatus::default(),
        }
    }

    pub fn to_json(&self) -> Value {
        self.0.clone()
    }
}

// ---------------------------------------------------------------------------
// 成长汇总（wb_tasks.fetch_growth_summary）
// ---------------------------------------------------------------------------

/// `fetch_growth_summary()` 的返回形状。
///
/// Python（`wb_tasks.py:170-232`）逐环节填充，**任一环节失败只记日志、
/// 不影响已有字段**。因此除 `energy` / `streak_days` / `travel` 外，其余字段
/// 都是 `Option`：`None` 表示该环节没有拿到数据，序列化时**省略该键**
/// （前端据此判断「数据缺失」而不是「值为 0」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowthSummary {
    pub energy: i64,
    pub streak_days: i64,
    pub travel: TravelStatus,
    /// `/activity/growth/streak` 的 `makeup_cards.balance`。
    pub makeup_cards: Option<i64>,
    /// `redemption_status.tiers`（天数档位，如 `[7, 14, 28]`）。
    pub tiers: Option<Vec<i64>>,
    /// 三个档位的解锁状态，缺省一律 `"locked"`。键序固定为 7d / 14d / 28d。
    pub tier_statuses: [(&'static str, String); 3],
    /// `/activity/growth/lottery/summary` 的 `data.chances`。
    pub lottery_chances: Option<i64>,
}

impl Default for GrowthSummary {
    fn default() -> Self {
        GrowthSummary {
            energy: 0,
            streak_days: 0,
            travel: TravelStatus::default(),
            makeup_cards: None,
            tiers: None,
            tier_statuses: default_tier_statuses(),
            lottery_chances: None,
        }
    }
}

/// `tier_statuses` 的缺省三档，全部 `"locked"`。
pub fn default_tier_statuses() -> [(&'static str, String); 3] {
    [
        ("7d", "locked".to_string()),
        ("14d", "locked".to_string()),
        ("28d", "locked".to_string()),
    ]
}

impl GrowthSummary {
    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("energy".to_string(), json!(self.energy));
        obj.insert("streak_days".to_string(), json!(self.streak_days));
        obj.insert("travel".to_string(), self.travel.to_json());
        if let Some(v) = self.makeup_cards {
            obj.insert("makeup_cards".to_string(), json!(v));
        }
        // Python 只在 streak 接口成功时写这两个键；`tiers` 与 `tier_statuses`
        // 同属一个环节，用 `tiers.is_some()` 统一门控。
        if let Some(tiers) = &self.tiers {
            obj.insert("tiers".to_string(), json!(tiers));
            let mut statuses = serde_json::Map::new();
            for (key, value) in &self.tier_statuses {
                statuses.insert((*key).to_string(), Value::String(value.clone()));
            }
            obj.insert("tier_statuses".to_string(), Value::Object(statuses));
        }
        if let Some(v) = self.lottery_chances {
            obj.insert("lottery_chances".to_string(), json!(v));
        }
        Value::Object(obj)
    }
}

// ---------------------------------------------------------------------------
// 契约 16：`fetch_streak_days` 的 None ≠ 0
// ---------------------------------------------------------------------------

/// 把「上游没答上来」与「连续 0 天」区分开。
///
/// Python `fetch_streak_days()` 异常时 `return None`，正常时返回可能为 0 的
/// 整数。Rust 侧用 `Option<i64>` 承载：`Some(0)` 是**真实值**，不得折叠成
/// `None`，也不得反过来把 `None` 当 0 参与计算（那会把「查不到」当成「断签」，
/// 触发不必要的补签或误报）。
pub fn streak_days_or_none(days: Option<i64>) -> Option<i64> {
    days
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 契约 2：GrowthTask 12 字段 ----

    #[test]
    fn growth_task_full_sample_has_exactly_twelve_keys_in_python_order() {
        let task = GrowthTask {
            task_code: "expert_5".into(),
            name: "使用专家助手".into(),
            description: "累计使用 5 次".into(),
            jump_url: "workbuddy://expert".into(),
            status: "accepted".into(),
            task_type: "expert".into(),
            current: 2,
            target: 5,
            reward_credit: 200,
            reward_energy: 10,
            unforgeable: false,
            reason: String::new(),
        };
        let json = task.to_json();
        let obj = json.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "task_code",
                "name",
                "description",
                "jump_url",
                "status",
                "task_type",
                "current",
                "target",
                "reward_credit",
                "reward_energy",
                "unforgeable",
                "reason",
            ],
            "12 个键的集合与顺序必须与 Python dict 字面量一致"
        );
        assert_eq!(obj.len(), 12);
        assert_eq!(json["current"], 2);
        assert_eq!(json["reward_energy"], 10);
        assert_eq!(json["unforgeable"], false);
    }

    #[test]
    fn growth_task_round_trips_through_json() {
        let task = GrowthTask {
            task_code: "first_buddy".into(),
            name: "领取一只 Buddy".into(),
            description: String::new(),
            jump_url: "workbuddy://buddy".into(),
            status: "not_accepted".into(),
            task_type: "chat".into(),
            current: 0,
            target: 1,
            reward_credit: 300,
            reward_energy: 0,
            unforgeable: false,
            reason: String::new(),
        };
        assert_eq!(GrowthTask::from_json(&task.to_json()), task);
    }

    #[test]
    fn growth_task_missing_fields_fall_back_to_python_defaults() {
        // 上游只给了 task_code：其余 11 个键全部按契约 2 缺省补齐。
        let task = GrowthTask::from_json(&json!({"task_code": "chat_5"}));
        assert_eq!(task.task_code, "chat_5");
        assert_eq!(task.name, "");
        assert_eq!(task.description, "");
        assert_eq!(task.jump_url, "");
        assert_eq!(task.status, "not_accepted");
        assert_eq!(task.task_type, "");
        assert_eq!(task.current, 0);
        assert_eq!(task.target, 1);
        assert_eq!(task.reward_credit, 0);
        assert_eq!(task.reward_energy, 0);
        assert!(!task.unforgeable);
        assert_eq!(task.reason, "");
        // 补齐后依旧是 12 键
        assert_eq!(task.to_json().as_object().unwrap().len(), 12);
    }

    #[test]
    fn growth_task_rejects_wrong_types_by_defaulting() {
        // 上游把数字写成字符串时不 panic，回落缺省值。
        let task = GrowthTask::from_json(&json!({
            "task_code": "chat_5",
            "current": "3",
            "target": null,
            "unforgeable": "yes",
        }));
        assert_eq!(task.current, 0);
        assert_eq!(task.target, 1);
        assert!(!task.unforgeable);
    }

    // ---- 常量表 ----

    #[test]
    fn task_specs_match_python_table() {
        assert_eq!(TASK_SPECS.len(), 19);
        let canvas = task_spec("create_canvas").unwrap();
        assert_eq!(canvas.kind, "canvas");
        assert_eq!(canvas.target, 1);
        assert_eq!(canvas.reward, 300);
        assert_eq!(canvas.name, "创建设计任务");

        let team = task_spec("Expert_team_use_3").unwrap();
        assert_eq!(team.kind, "team");
        assert_eq!(team.target, 3);
        assert_eq!(team.reward, 150);

        assert!(task_spec("does_not_exist").is_none());
    }

    #[test]
    fn unforgeable_specs_carry_reasons() {
        let oa = task_spec("wb_wechat_oa_subscribe_task").unwrap();
        assert!(oa.unforgeable);
        assert_eq!(oa.reward, 100);
        assert_eq!(oa.reason, "需在微信关注「腾讯 WorkBuddy」公众号满 24 小时");

        let charity = task_spec("Expert_Philanthropy").unwrap();
        assert!(charity.unforgeable);
        assert_eq!(charity.reward, 0);
        assert_eq!(charity.reason, "真实捐款动作");

        // Python 这两条**没有** `target` 键，`spec.get("target", 1)` → 1。
        // 这是可观测行为，不能照抄 reward 值（100/0）当 target。
        assert_eq!(oa.target, 1);
        assert_eq!(charity.target, 1);

        // 只有这两条是 unforgeable
        let count = TASK_SPECS.iter().filter(|(_, s)| s.unforgeable).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn desktop_only_tasks_cover_exactly_four_codes() {
        assert_eq!(DESKTOP_ONLY_TASKS.len(), 4);
        for code in ["RichMeow_Chat", "Library_read", "Buddy_App", "Buddy_App_QQ"] {
            assert!(
                DESKTOP_ONLY_TASKS.iter().any(|(c, _)| *c == code),
                "缺少桌面端任务 `{code}`"
            );
        }
    }

    #[test]
    fn night_window_covers_23_to_07_only() {
        for hour in 0..8 {
            assert!(in_night_window(hour), "{hour} 点应命中");
        }
        assert!(in_night_window(23));
        for hour in 8..23 {
            assert!(!in_night_window(hour), "{hour} 点不应命中");
        }
        assert_eq!(NIGHT_TASK_CODES, ["black_cat"]);
    }

    // ---- AcceptOutcome ----

    #[test]
    fn accept_outcome_empty_is_ok() {
        let out = AcceptOutcome::empty();
        assert!(out.ok);
        assert_eq!(
            out.to_json(),
            json!({"ok": true, "accepted": [], "failed": [], "msg": "", "reasons": []})
        );
    }

    #[test]
    fn accept_outcome_records_partial_failure() {
        let out = AcceptOutcome {
            ok: false,
            accepted: vec!["chat_5".into()],
            failed: vec!["black_cat".into()],
            msg: "task not completed".into(),
            reasons: vec!["black_cat: rejected".into()],
        };
        let json = out.to_json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["accepted"], json!(["chat_5"]));
        assert_eq!(json["failed"], json!(["black_cat"]));
        assert_eq!(json["reasons"], json!(["black_cat: rejected"]));
    }

    // ---- ClaimOutcome ----

    #[test]
    fn claim_success_omits_msg_key_like_python() {
        let json = ClaimOutcome::success(300, 10).to_json();
        assert_eq!(json, json!({"ok": true, "credit": 300, "energy": 10}));
        assert!(json.get("msg").is_none(), "成功时不应有 msg 键");
    }

    #[test]
    fn claim_failure_carries_msg() {
        let json = ClaimOutcome::failure("task not completed").to_json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["credit"], 0);
        assert_eq!(json["energy"], 0);
        assert_eq!(json["msg"], "task not completed");
    }

    // ---- TravelStatus ----

    #[test]
    fn travel_defaults_to_unknown_state() {
        assert_eq!(
            TravelStatus::default().to_json(),
            json!({"state": "unknown"})
        );
        assert_eq!(
            TravelStatus::from_upstream_data(None).to_json(),
            json!({"state": "unknown"})
        );
        assert_eq!(
            TravelStatus::from_upstream_data(Some(&json!("oops"))).to_json(),
            json!({"state": "unknown"})
        );
    }

    #[test]
    fn travel_passes_upstream_object_through() {
        let data = json!({"state": "traveling", "name": "猫猫", "away": 2});
        assert_eq!(
            TravelStatus::from_upstream_data(Some(&data)).to_json(),
            data
        );
    }

    // ---- GrowthSummary ----

    #[test]
    fn growth_summary_default_only_has_three_required_keys() {
        let json = GrowthSummary::default().to_json();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["energy", "streak_days", "travel"]);
        assert_eq!(json["energy"], 0);
        assert_eq!(json["streak_days"], 0);
        assert_eq!(json["travel"], json!({"state": "unknown"}));
    }

    #[test]
    fn growth_summary_full_sample_emits_all_keys() {
        let summary = GrowthSummary {
            energy: 42,
            streak_days: 5,
            travel: TravelStatus(json!({"state": "idle"})),
            makeup_cards: Some(1),
            tiers: Some(vec![7, 14, 28]),
            tier_statuses: [
                ("7d", "unlocked".to_string()),
                ("14d", "locked".to_string()),
                ("28d", "locked".to_string()),
            ],
            lottery_chances: Some(2),
        };
        let json = summary.to_json();
        assert_eq!(json["energy"], 42);
        assert_eq!(json["streak_days"], 5);
        assert_eq!(json["makeup_cards"], 1);
        assert_eq!(json["tiers"], json!([7, 14, 28]));
        assert_eq!(json["tier_statuses"]["7d"], "unlocked");
        assert_eq!(json["tier_statuses"]["14d"], "locked");
        assert_eq!(json["lottery_chances"], 2);
    }

    #[test]
    fn tier_statuses_without_tiers_are_not_emitted() {
        // `tiers` 为 None 表示 streak 环节失败：此时不应单独吐 tier_statuses。
        let summary = GrowthSummary {
            makeup_cards: None,
            tiers: None,
            ..Default::default()
        };
        assert!(summary.to_json().get("tier_statuses").is_none());
    }

    // ---- 契约 16 ----

    #[test]
    fn streak_none_is_distinct_from_zero() {
        assert_eq!(streak_days_or_none(None), None);
        assert_eq!(streak_days_or_none(Some(0)), Some(0));
        assert_ne!(streak_days_or_none(None), Some(0));
    }
}
