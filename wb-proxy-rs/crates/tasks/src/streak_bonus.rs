//! 关卡 4.17：连登管家（streak_bonus）——Python `wb_tasks.run_streak_bonus` 的
//! **四步**移植。
//!
//! 权威参照 `wb_tasks.py:771-866`（`run_streak_bonus`）。
//!
//! ## 旧实现是臆造端点
//!
//! 移植前的 Rust 实现（`api_routes.rs` 的一步式 `POST {chat_base}/activity/
//! growth/streak/claim`）**在 Python 全仓 0 命中**——Python 的连登管家是下面
//! 这四步，从不调 `streak/claim`。也就是说该功能在 Rust 侧从来没真正工作过：
//! 上游没有这个端点，调用必然 404/400。
//!
//! ## 四步
//!
//! 0. **补签保连登**：`GET /activity/growth/heatmap` → 找昨天（本地日期减一天）
//!    的格子，`score == 0`（漏签）时再查 `GET /activity/growth/streak` 的
//!    `data.makeup_cards.balance`，有卡才 `POST /activity/growth/makeup-cards/use`
//!    （body `{"target_date": "YYYY-MM-DD"}`）。
//! 1. **新手礼包 / 活动补偿**：`POST {bill_base}/billing/meter/claim-gift` 与
//!    `claim-compensation`，body 是空对象 `{}`；`code == 0` 才记一条日志。
//! 2. **连登档位兑换**：`GET /activity/growth/streak` 的
//!    `data.redemption_status.tiers` 逐档尝试 `POST /activity/growth/redeem`
//!    （body `{"tier": ..., "client_token": ...}`）；档位状态是 `locked` 或
//!    `claimed` 的跳过。
//! 3. **自动抽奖闭环**：`GET /activity/growth/lottery/summary` 的
//!    `data.chances` 有几次就抽几次 `POST /activity/growth/lottery/draw`
//!    （body `{"client_token": ...}`），每次抽完 `sleep(1.0)`。
//!
//! ## 幂等与门禁
//!
//! - `realm != "cn"` → `{"ok": false, "msg": "连登管家仅限国内版账号"}`
//! - `enterprise_id` 非空 → `{"ok": true, "msg": "企业版账号无成长体系，跳过"}`
//! - 上面两个门禁的返回体**没有 `logs` 键**（Python 的 dict 字面量就没有），
//!   故 [`StreakBonusOutcome::logs`] 是 `Option`。
//! - 未解锁 / 已领 / 无次数 / 已领过都**安全静默跳过**。
//!
//! ## 失败可见（R8）
//!
//! Python 把各步的网络异常写进 `_log`（应用日志），返回体里看不到。本模块把
//! 这些异常收进 [`StreakBonusOutcome::warnings`]，**不进 `to_json()`**（返回体
//! 形状与 Python 逐字一致），由调用方写进面板日志。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Map, Value};

use wb_gateway::outbound::{describe_error, UpstreamClient};

use crate::growth_runner::{Clock, Sleeper};

// ---------------------------------------------------------------------------
// 常量（逐字对齐 wb_tasks.py）
// ---------------------------------------------------------------------------

/// Python `wb_tasks.py:17` 的 `CHAT_BASE`。
pub const CHAT_BASE: &str = "https://copilot.tencent.com";
/// Python `wb_tasks.py:18` 的 `BILL_BASE`。
pub const BILL_BASE: &str = "https://www.codebuddy.cn";

/// 成长体系路径（拼在 `chat_base` 之后）。
pub const HEATMAP_PATH: &str = "/activity/growth/heatmap";
pub const STREAK_PATH: &str = "/activity/growth/streak";
pub const MAKEUP_USE_PATH: &str = "/activity/growth/makeup-cards/use";
pub const REDEEM_PATH: &str = "/activity/growth/redeem";
pub const LOTTERY_SUMMARY_PATH: &str = "/activity/growth/lottery/summary";
pub const LOTTERY_DRAW_PATH: &str = "/activity/growth/lottery/draw";
/// 计费路径（拼在 `bill_base` 之后）。
pub const BILL_GIFT_PATH: &str = "/billing/meter/claim-gift";
pub const BILL_COMPENSATION_PATH: &str = "/billing/meter/claim-compensation";

/// 出站超时（秒）。Python 这几步全是 `timeout=10`。
pub const STREAK_BONUS_TIMEOUT: Duration = Duration::from_secs(10);
/// 两次抽奖之间的间隔（秒）。Python 是 `time.sleep(1.0)`。
pub const STREAK_BONUS_DRAW_INTERVAL: f64 = 1.0;

/// 非国内版时的文案（Python :778）。
pub const MSG_CN_ONLY: &str = "连登管家仅限国内版账号";
/// 企业版跳过时的文案（Python :780）。
pub const MSG_ENTERPRISE_SKIP: &str = "企业版账号无成长体系，跳过";
/// 什么都没做时的文案（Python :864）。
pub const MSG_IDLE: &str = "连登状态正常（无需兑换/无抽奖次数）";

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 连登管家需要的账号身份与出站头。
///
/// 出站头由调用方组装（`wb_gateway::headers::chat_request_headers` /
/// `billing_request_headers`），与本 crate 其余模块的
/// [`crate::growth_api::TaskAccount`] 同一口径。
pub struct StreakBonusAccount<'a> {
    /// `"cn"` 之外一律拒绝（Python :778）。
    pub realm: &'a str,
    /// 企业空间 id；非空即企业账号 → 跳过（Python :780）。
    pub enterprise_id: &'a str,
    /// chat 用途出站头（成长体系那几个端点用）。
    pub chat_headers: &'a HashMap<String, String>,
    /// billing 用途出站头（礼包/补偿那两个端点用）。
    pub billing_headers: &'a HashMap<String, String>,
}

/// 运行配置。端点可注入：契约测试必须指向 wiremock mock 上游
/// （M4 硬约束「不得对真实上游发任何请求」）。
pub struct StreakBonusConfig<'a> {
    /// 成长体系 base（`chat_base`）。
    pub chat_base: &'a str,
    /// 计费 base（`bill_base`）。
    pub bill_base: &'a str,
    /// 出站超时。
    pub timeout: Duration,
    /// 两次抽奖之间的间隔（秒）。
    pub draw_interval: f64,
    /// `client_token` 生成器（Python 是 `str(uuid.uuid4())`）。
    ///
    /// 做成注入点有两个原因：① 契约测试要能断言请求体；② 本 crate 没有
    /// `uuid` 依赖，**不为此新增生产依赖**（全局约束），用默认实现即可。
    ///
    /// `Sync` 是必需的：路由层把本结构跨 `.await` 借用（`&mut
    /// StreakBonusConfig` 要活过 await 点），future 才会是 `Send`。
    pub client_token: Box<dyn FnMut() -> String + Send + Sync>,
}

impl StreakBonusConfig<'static> {
    /// 生产口径：Python 的两个常量域名。
    pub fn production() -> Self {
        Self {
            chat_base: CHAT_BASE,
            bill_base: BILL_BASE,
            timeout: STREAK_BONUS_TIMEOUT,
            draw_interval: STREAK_BONUS_DRAW_INTERVAL,
            client_token: Box::new(default_client_token),
        }
    }
}

/// 本地时间源与节流器（复用成长任务那套 trait）。
pub struct StreakBonusCtx<'a> {
    /// 睡（Python 的 `time.sleep`）。
    pub sleeper: &'a dyn Sleeper,
    /// 本地时间源（Python 的 `time.localtime`）。
    pub clock: &'a dyn Clock,
}

// ---------------------------------------------------------------------------
// 返回值
// ---------------------------------------------------------------------------

/// 连登管家的结果。
///
/// 键集与顺序逐字对齐 Python `run_streak_bonus` 的返回 dict：
/// 正常路径是 `{"ok", "logs", "msg"}`；两个门禁早退时**没有 `logs` 键**
/// （Python 的 dict 字面量就没有），故 [`Self::logs`] 是 `Option`。
#[derive(Debug, Clone, PartialEq)]
pub struct StreakBonusOutcome {
    /// 只有「非国内版」这一种情况是 `false`；其余一律 `true`
    /// （各步失败都只是跳过，Python 同）。
    pub ok: bool,
    /// Python 的 `logs`（`None` = 该返回体里没有这个键）。
    pub logs: Option<Vec<String>>,
    /// Python 的 `msg`。
    pub msg: String,
    /// Rust 侧独有：各步的异常原因。Python 写进 `_log`，返回体里看不到。
    /// **不进 `to_json()`**，由调用方写进面板日志。
    pub warnings: Vec<String>,
}

impl StreakBonusOutcome {
    /// 门禁早退：没有 `logs` 键。
    fn gate(ok: bool, msg: &str) -> Self {
        Self {
            ok,
            logs: None,
            msg: msg.to_string(),
            warnings: Vec::new(),
        }
    }

    /// 正常路径的返回体（键序：`ok` → `logs` → `msg`）。
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("ok".to_string(), json!(self.ok));
        if let Some(logs) = &self.logs {
            out.insert("logs".to_string(), json!(logs));
        }
        out.insert("msg".to_string(), json!(self.msg));
        Value::Object(out)
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 连登管家：补签保连登 → 礼包/补偿 → 档位兑换 → 自动抽奖闭环。
///
/// 逐字对齐 Python `wb_tasks.run_streak_bonus`（:771-866）：返回体的键集、
/// 日志文案、跳过条件都照抄；Python 把异常写进 `_log`，本实现收进
/// [`StreakBonusOutcome::warnings`]（不进返回体）。
pub async fn run_streak_bonus(
    client: &UpstreamClient,
    acc: &StreakBonusAccount<'_>,
    cfg: &mut StreakBonusConfig<'_>,
    ctx: &StreakBonusCtx<'_>,
) -> StreakBonusOutcome {
    if acc.realm != "cn" {
        return StreakBonusOutcome::gate(false, MSG_CN_ONLY);
    }
    if !acc.enterprise_id.is_empty() {
        return StreakBonusOutcome::gate(true, MSG_ENTERPRISE_SKIP);
    }

    let mut logs: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 0. 补签保连登
    if let Some(log) = makeup_yesterday(client, acc, cfg, ctx, &mut warnings).await {
        logs.push(log);
    }
    // 1. 新手礼包 + 活动补偿
    claim_billing_gifts(client, acc, cfg, &mut logs, &mut warnings).await;
    // 2. 连登档位兑换
    redeem_tiers(client, acc, cfg, &mut logs, &mut warnings).await;
    // 3. 自动抽奖闭环
    draw_lottery(client, acc, cfg, ctx, &mut logs, &mut warnings).await;

    let msg = if logs.is_empty() {
        MSG_IDLE.to_string()
    } else {
        logs.join("; ")
    };
    StreakBonusOutcome {
        ok: true,
        logs: Some(logs),
        msg,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// 第 0 步：补签保连登
// ---------------------------------------------------------------------------

/// 昨天漏签且有补签卡时消耗一张补签卡；返回要记进 `logs` 的那行文案。
///
/// Python :790-815。日志文案在 **HTTP 成功**（2xx）时写，与上游返回的
/// `code` 无关——这是 Python 的原样行为，照抄。
async fn makeup_yesterday(
    client: &UpstreamClient,
    acc: &StreakBonusAccount<'_>,
    cfg: &StreakBonusConfig<'_>,
    ctx: &StreakBonusCtx<'_>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let yesterday = yesterday_date(ctx.clock);

    let heatmap = match get_json(
        client,
        &format!("{}{}", cfg.chat_base, HEATMAP_PATH),
        acc.chat_headers,
        cfg.timeout,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("makeup yesterday failed: {e}"));
            return None;
        }
    };
    let cells = heatmap
        .get("data")
        .and_then(|d| d.get("cells"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let y_cell = cells
        .iter()
        .find(|c| c.get("date").and_then(Value::as_str) == Some(yesterday.as_str()));
    // Python：`y_cell and y_cell.get("score", 0) == 0`
    let missed = match y_cell {
        Some(cell) => py_eq_zero(cell.get("score")),
        None => false,
    };
    if !missed {
        return None;
    }

    // 查补签卡余额
    let streak = match get_json(
        client,
        &format!("{}{}", cfg.chat_base, STREAK_PATH),
        acc.chat_headers,
        cfg.timeout,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("makeup yesterday failed: {e}"));
            return None;
        }
    };
    let cards = streak
        .get("data")
        .and_then(|d| d.get("makeup_cards"))
        .and_then(|m| m.get("balance"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if cards <= 0 {
        return None;
    }

    // 用掉一张补签卡
    let body = json!({"target_date": yesterday});
    match client
        .post_with_timeout(
            &format!("{}{}", cfg.chat_base, MAKEUP_USE_PATH),
            acc.chat_headers,
            &body,
            cfg.timeout,
        )
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            Some(format!("已消耗 1 张补签卡补签昨日 ({yesterday})"))
        }
        Ok(resp) => {
            warnings.push(format!(
                "makeup yesterday failed: HTTP {}",
                resp.status().as_u16()
            ));
            None
        }
        Err(e) => {
            warnings.push(format!("makeup yesterday failed: {}", describe_error(&e)));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// 第 1 步：新手礼包 + 活动补偿
// ---------------------------------------------------------------------------

/// 领新手礼包与活动补偿（每号一次，没有就静默）。
///
/// Python :817-830：两个端点各 POST 一个空对象 `{}`；`code == 0` 才记
/// `领取{name} +{cr}积分`。
async fn claim_billing_gifts(
    client: &UpstreamClient,
    acc: &StreakBonusAccount<'_>,
    cfg: &StreakBonusConfig<'_>,
    logs: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    for (path, name) in [
        (BILL_GIFT_PATH, "新手礼包"),
        (BILL_COMPENSATION_PATH, "活动补偿"),
    ] {
        let url = format!("{}{}", cfg.bill_base, path);
        match client
            .post_with_timeout(&url, acc.billing_headers, &json!({}), cfg.timeout)
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if py_code_is_zero(payload.get("code")) {
                    let credit = payload
                        .get("data")
                        .and_then(|d| d.get("credit"))
                        .map(py_str_value)
                        .unwrap_or_else(|| "0".to_string());
                    logs.push(format!("领取{name} +{credit}积分"));
                }
            }
            Ok(resp) => warnings.push(format!(
                "claim {name} failed: HTTP {}",
                resp.status().as_u16()
            )),
            Err(e) => warnings.push(format!("claim {name} failed: {}", describe_error(&e))),
        }
    }
}

// ---------------------------------------------------------------------------
// 第 2 步：连登档位兑换（7d / 14d / 28d）
// ---------------------------------------------------------------------------

/// 逐档尝试兑换；`locked` / `claimed` 的档位跳过。
///
/// Python :832-858。
async fn redeem_tiers(
    client: &UpstreamClient,
    acc: &StreakBonusAccount<'_>,
    cfg: &mut StreakBonusConfig<'_>,
    logs: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let streak = match get_json(
        client,
        &format!("{}{}", cfg.chat_base, STREAK_PATH),
        acc.chat_headers,
        cfg.timeout,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("streak redeem check failed: {e}"));
            return;
        }
    };
    let redemption = streak
        .get("data")
        .and_then(|d| d.get("redemption_status"))
        .cloned()
        .unwrap_or(Value::Null);
    let tiers = redemption
        .get("tiers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for tier in tiers {
        // Python 是 `t.get("tier")`：档位条目不是 dict 时抛 AttributeError，
        // 被外层 try 捕获 → **整步中止**。这里同样中止（不当成空档位继续）。
        let Some(obj) = tier.as_object() else {
            warnings.push("streak redeem check failed: tier entry is not an object".to_string());
            break;
        };
        let tier_value = obj.get("tier").cloned().unwrap_or(Value::Null);
        let status = tier_value
            .as_str()
            .and_then(|name| tier_status(&redemption, name));
        let status_str = status.and_then(Value::as_str).unwrap_or("");
        if status_str == "locked" || status_str == "claimed" {
            continue;
        }
        let token = (cfg.client_token)();
        let body = json!({"tier": tier_value, "client_token": token});
        match client
            .post_with_timeout(
                &format!("{}{}", cfg.chat_base, REDEEM_PATH),
                acc.chat_headers,
                &body,
                cfg.timeout,
            )
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if py_code_is_zero(payload.get("code")) {
                    logs.push(format!("成功兑换连登 {} 档位", py_str_value(&tier_value)));
                }
            }
            Ok(resp) => warnings.push(format!(
                "redeem {} failed: HTTP {}",
                py_str_value(&tier_value),
                resp.status().as_u16()
            )),
            Err(e) => warnings.push(format!(
                "redeem {} failed: {}",
                py_str_value(&tier_value),
                describe_error(&e)
            )),
        }
    }
}

/// 档位状态（Python 的 `statuses` 字典：7d / 14d / 28d）。
fn tier_status<'a>(redemption: &'a Value, tier_name: &str) -> Option<&'a Value> {
    match tier_name {
        "7d" => redemption.get("tier_7d_status"),
        "14d" => redemption.get("tier_14d_status"),
        "28d" => redemption.get("tier_28d_status"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 第 3 步：自动抽奖闭环
// ---------------------------------------------------------------------------

/// 有几次机会就抽几次；抽完 `sleep(1.0)`。
///
/// Python :860-880。Python 在**每次**迭代末尾都 sleep（含最后一次），
/// 抽失败（网络异常或非 2xx）时 `break`——**不**再 sleep，照抄。
async fn draw_lottery(
    client: &UpstreamClient,
    acc: &StreakBonusAccount<'_>,
    cfg: &mut StreakBonusConfig<'_>,
    ctx: &StreakBonusCtx<'_>,
    logs: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let summary = match get_json(
        client,
        &format!("{}{}", cfg.chat_base, LOTTERY_SUMMARY_PATH),
        acc.chat_headers,
        cfg.timeout,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("lottery summary failed: {e}"));
            return;
        }
    };
    // Python：`range(chances)` —— chances 不是整数会抛 TypeError，被外层
    // try 捕获 → 整步中止。缺省 0（不抽）才是正常路径。
    let chances = match summary.get("data").and_then(|d| d.get("chances")) {
        None => 0,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(v) => v.max(0),
            None => {
                warnings.push(format!(
                    "lottery summary failed: chances is not an integer ({n})"
                ));
                return;
            }
        },
        Some(other) => {
            warnings.push(format!(
                "lottery summary failed: chances is not an integer ({other})"
            ));
            return;
        }
    };

    let mut drawn = 0i64;
    for _ in 0..chances {
        let token = (cfg.client_token)();
        let body = json!({"client_token": token});
        match client
            .post_with_timeout(
                &format!("{}{}", cfg.chat_base, LOTTERY_DRAW_PATH),
                acc.chat_headers,
                &body,
                cfg.timeout,
            )
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if py_code_is_zero(payload.get("code")) {
                    drawn += 1;
                }
            }
            Ok(resp) => {
                warnings.push(format!(
                    "lottery draw failed: HTTP {}",
                    resp.status().as_u16()
                ));
                break;
            }
            Err(e) => {
                warnings.push(format!("lottery draw failed: {}", describe_error(&e)));
                break;
            }
        }
        // Python 每次迭代末尾都睡 1 秒（含最后一次）。
        ctx.sleeper.sleep(cfg.draw_interval).await;
    }
    if drawn > 0 {
        logs.push(format!("完成成长抽奖 {drawn} 次"));
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// GET 一个 JSON；非 2xx / 解析失败都算 `Err`（Python 里是抛异常）。
async fn get_json(
    client: &UpstreamClient,
    url: &str,
    headers: &HashMap<String, String>,
    timeout: Duration,
) -> Result<Value, String> {
    let resp = client
        .get_with_timeout(url, headers, timeout)
        .await
        .map_err(|e| describe_error(&e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Python `time.strftime("%Y-%m-%d", time.localtime(time.time() - 86400))`。
///
/// 用注入的本地时钟取**今天的本地日期**再减一天。跨夏令时的那一天与 Python
/// 的「减 86400 秒再格式化」可能差一天（Python 那种算法在夏令时切换日会
/// 给出当天或前天的日期），本实现给的是稳定的「昨天」。
pub fn yesterday_date(clock: &dyn Clock) -> String {
    let now = clock.local_now();
    let today = now.get(..10).unwrap_or("").to_string();
    chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d")
        .map(|d| {
            (d - chrono::Duration::days(1))
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or(today)
}

/// Python 的 `x == 0`：`None` 不等于 0，但**键缺失时**的缺省值就是 0。
///
/// 用于 `cell.get("score", 0) == 0`：键缺失 → 0 → 命中；值是字符串 `"0"` →
/// Python 里不等于 0 → 不命中。
fn py_eq_zero(value: Option<&Value>) -> bool {
    match value {
        None => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(b)) => !*b,
        _ => false,
    }
}

/// Python 的 `payload.get("code") == 0`。
fn py_code_is_zero(code: Option<&Value>) -> bool {
    match code {
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(b)) => !*b,
        _ => false,
    }
}

/// Python 的 `%s` 格式化一个 JSON 值（`None` → `"None"`、`True` → `"True"`）。
fn py_str_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// `client_token` 的默认实现：形状对齐 uuid4（36 字符、含连字符、版本位 4）。
///
/// 本 crate 没有 `uuid` 依赖，**不为此新增生产依赖**；用「系统时钟 + 进程内
/// 计数器 + 哈希随机器播种」拼一个唯一性足够的一次性令牌。
/// 默认的 `client_token` 生成器（uuid4 形状）。
///
/// `pub` 是因为路由层要把它装进 [`StreakBonusConfig::client_token`]；本 crate
/// 无 `uuid` 依赖，**不为此新增生产依赖**（全局约束）。
pub fn default_client_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn pseudo_random() -> u64 {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        RandomState::new().build_hasher().finish()
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = pseudo_random() ^ nanos.rotate_left(17);
    let b = pseudo_random() ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (nanos >> 32) as u32,
        (a & 0xFFFF) as u16,
        (a >> 16) & 0xFFF,
        ((b >> 48) as u16 & 0x3FFF) | 0x8000,
        b & 0xFFFF_FFFF_FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedClock(&'static str);

    impl Clock for FixedClock {
        fn local_hour(&self) -> u32 {
            12
        }
        fn local_now(&self) -> String {
            self.0.to_string()
        }
    }

    #[test]
    fn yesterday_date_is_today_minus_one_day() {
        let clock = FixedClock("2026-10-20 08:30:00");
        assert_eq!(yesterday_date(&clock), "2026-10-19");
        // 跨月
        let clock = FixedClock("2026-11-01 00:00:01");
        assert_eq!(yesterday_date(&clock), "2026-10-31");
        // 跨闰年 2 月
        let clock = FixedClock("2024-03-01 12:00:00");
        assert_eq!(yesterday_date(&clock), "2024-02-29");
    }

    #[test]
    fn yesterday_date_falls_back_to_today_when_unparsable() {
        let clock = FixedClock("not-a-date");
        assert_eq!(yesterday_date(&clock), "not-a-date");
    }

    #[test]
    fn py_eq_zero_follows_python_semantics() {
        // 键缺失 → 缺省 0 → 命中
        assert!(py_eq_zero(None));
        // 数值 0 → 命中
        assert!(py_eq_zero(Some(&json!(0))));
        assert!(py_eq_zero(Some(&json!(0.0))));
        // False == 0（Python）→ 命中
        assert!(py_eq_zero(Some(&json!(false))));
        // 字符串 "0" != 0 → 不命中
        assert!(!py_eq_zero(Some(&json!("0"))));
        // null != 0 → 不命中
        assert!(!py_eq_zero(Some(&Value::Null)));
        // 非零 → 不命中
        assert!(!py_eq_zero(Some(&json!(1))));
    }

    #[test]
    fn py_code_is_zero_only_accepts_numeric_zero() {
        assert!(py_code_is_zero(Some(&json!(0))));
        assert!(!py_code_is_zero(Some(&json!(1))));
        assert!(!py_code_is_zero(None));
        assert!(!py_code_is_zero(Some(&json!("0"))));
    }

    #[test]
    fn py_str_value_matches_python_percent_s() {
        assert_eq!(py_str_value(&Value::Null), "None");
        assert_eq!(py_str_value(&json!(true)), "True");
        assert_eq!(py_str_value(&json!(false)), "False");
        assert_eq!(py_str_value(&json!(7)), "7");
        assert_eq!(py_str_value(&json!("x")), "x");
    }

    #[test]
    fn outcome_json_omits_logs_on_gate_exit() {
        let out = StreakBonusOutcome::gate(false, MSG_CN_ONLY);
        let json = out.to_json();
        assert_eq!(json, json!({"ok": false, "msg": MSG_CN_ONLY}));
        assert!(json.get("logs").is_none(), "门禁早退时没有 logs 键");
    }

    #[test]
    fn outcome_json_carries_logs_in_normal_path() {
        let out = StreakBonusOutcome {
            ok: true,
            logs: Some(vec!["领取新手礼包 +100积分".to_string()]),
            msg: "领取新手礼包 +100积分".to_string(),
            warnings: vec!["boom".to_string()],
        };
        let json = out.to_json();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["ok", "logs", "msg"]);
        assert_eq!(json["logs"], json!(["领取新手礼包 +100积分"]));
        assert!(
            json.get("warnings").is_none(),
            "warnings 是 Rust 侧独有的，不进返回体"
        );
    }

    #[test]
    fn default_client_token_looks_like_uuid4_and_is_unique() {
        let a = default_client_token();
        let b = default_client_token();
        assert_eq!(a.len(), 36, "uuid4 形状：{a}");
        assert_ne!(a, b, "令牌必须互不相同");
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
    }
}
