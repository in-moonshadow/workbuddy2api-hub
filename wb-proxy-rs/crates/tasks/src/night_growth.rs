//! 关卡 4.17：夜猫子任务（`run_night_growth`）—— Python `wb_tasks.py:712-770` 的移植。
//!
//! 权威源：`wb_tasks.run_night_growth(account)`。
//!
//! ## 语义（夜猫子 = `black_cat`）
//! 每日 23:00-08:00 上报 1 次 GLM-5.2 夜间对话事件，每天计 1 次、累计 3 天可领奖。
//! 白天调用**诚实跳过**（不硬凑）。
//!
//! ## 分支表（逐条对齐 Python，返回键序也逐字一致）
//!
//! | # | 条件 | `ok` | `msg` 键 | `earned_credit` | `logs` |
//! |---|---|---|---|---|---|
//! | 1 | `realm != "cn"` | `false` | **有** | 0 | `[]` |
//! | 2 | 企业版 | `true` | **有** | 0 | `[]` |
//! | 3 | 不在 23:00-08:00 窗口 | `true` | 无 | 0 | 1 行 |
//! | 4 | 拉不到 `black_cat` 任务 | `false` | 无 | 0 | 1 行 |
//! | 5 | 任务已 `claimed` | `true` | 无 | 0 | 1 行 |
//! | 6 | 接取失败 | `false` | 无 | 0 | 1 行 |
//! | 7 | `cur >= tgt`（已达标） | 领奖结果 | 无 | 领到的积分 | ≥2 行 |
//! | 8 | 上报 + 轮询 | `true` | 无 | 由日志里的 `+N 积分` 反推 | ≥1 行 |
//!
//! ⚠️ **只有 1、2 两档带 `msg` 键**——Python 的字面量如此，本模块照抄
//! （`to_json` 因此有两套键序）。
//!
//! ## 与 Python 的已知差异
//! - **出站时限**：Python 的 `report_events` 带 `timeout=15`，本模块复用的
//!   `events::report_events` **没有**时限（与本仓库其它出站调用同批待办，
//!   账本 D24）。领奖/接取/拉清单沿用 `growth_api` 的既有实现（同样无时限）。
//! - **积分刷新**：Python 调 `account.fetch_credits()`；本模块注入闭包
//!   （`wb-tasks → wb-api` 禁止依赖）。返回体不含 `credits`，故对调用方
//!   无影响，只是「余额被刷新」这一副作用由调用方承担。

use serde_json::{json, Value};

use crate::events::{build_event, report_events};
use crate::growth_api::{accept_tasks, claim_task, fetch_growth_tasks, TaskEndpoints};
use crate::growth_runner::{now_ms, RunnerAccount, RunnerCtx};
use crate::in_night_window;
use wb_gateway::outbound::UpstreamClient;

/// 夜猫子任务 code（Python `NIGHT_TASK_CODES = {"black_cat"}`）。
pub const NIGHT_TASK_CODE: &str = "black_cat";

/// `accept_tasks` 的分块大小（Python 默认 `chunk=20`）。
const ACCEPT_CHUNK: usize = 20;

/// 达标后的轮询轮数（Python `for _ in range(4)`）。
const POLL_ROUNDS: usize = 4;

/// 轮询间隔秒（Python `time.sleep(4)`）。
const POLL_INTERVAL_SECS: f64 = 4.0;

/// `run_night_growth` 的返回值。
///
/// 键集合与键序按 Python 的八个分支逐字对齐（见模块文档的分支表）。
#[derive(Debug, Clone, PartialEq)]
pub struct NightGrowthOutcome {
    /// 整体是否视为成功。
    pub ok: bool,
    /// **仅**「国际版 / 企业版」两档出现（Python 只有这两个分支带 `msg`）。
    pub msg: Option<String>,
    /// 全过程日志（顺序即执行顺序）。
    pub logs: Vec<String>,
    /// 本次到账积分。
    pub earned_credit: i64,
}

impl NightGrowthOutcome {
    /// 序列化为与 Python 字面量键序一致的 JSON 对象。
    ///
    /// ⚠️ 两套键序：带 `msg` 的档是 `ok, msg, logs, earned_credit`，
    /// 不带的是 `ok, earned_credit, logs`（`serde_json` 开了 `preserve_order`）。
    pub fn to_json(&self) -> Value {
        match &self.msg {
            Some(m) => json!({
                "ok": self.ok,
                "msg": m,
                "logs": self.logs,
                "earned_credit": self.earned_credit,
            }),
            None => json!({
                "ok": self.ok,
                "earned_credit": self.earned_credit,
                "logs": self.logs,
            }),
        }
    }
}

/// `run_night_growth`（Python `wb_tasks.py:712-770`）。
///
/// - `credits_refresh`：Python `account.fetch_credits()` 的注入形态；
///   返回 `None` 表示刷新失败（Python 侧也不抛）。
pub async fn run_night_growth(
    client: &UpstreamClient,
    acc: &RunnerAccount<'_>,
    ep: &TaskEndpoints<'_>,
    ctx: &RunnerCtx<'_>,
    credits_refresh: &mut (dyn FnMut() -> Option<Value> + Send),
) -> NightGrowthOutcome {
    // ① 国际版：国内成长任务中心不适用。
    if acc.realm != "cn" {
        return NightGrowthOutcome {
            ok: false,
            msg: Some("国际版不适用国内成长任务中心".to_string()),
            logs: Vec::new(),
            earned_credit: 0,
        };
    }
    // ② 企业版：无成长体系。
    if acc.is_enterprise {
        return NightGrowthOutcome {
            ok: true,
            msg: Some("企业版账号无成长体系，跳过".to_string()),
            logs: Vec::new(),
            earned_credit: 0,
        };
    }

    let mut logs: Vec<String> = Vec::new();
    // Python `name = account.nickname or account.uid[:8]`。
    let name = acc.display_name();

    // ③ 不在夜间窗口：诚实跳过。
    if !in_night_window(ctx.clock.local_hour()) {
        logs.push(format!(
            "[{name}] 当前不在 23:00-08:00 夜间窗口, 夜猫子任务跳过 (每日 01:00 自动执行)"
        ));
        return NightGrowthOutcome {
            ok: true,
            msg: None,
            logs,
            earned_credit: 0,
        };
    }

    // ④ 拉不到任务清单。
    let tasks = fetch_growth_tasks(client, &acc.api, ep).await;
    let Some(task) = tasks.iter().find(|t| t.task_code == NIGHT_TASK_CODE) else {
        logs.push(format!("[{name}] 未获取到夜猫子任务清单"));
        return NightGrowthOutcome {
            ok: false,
            msg: None,
            logs,
            earned_credit: 0,
        };
    };

    // ⑤ 已领奖。
    if task.status == "claimed" {
        logs.push(format!("[{name}] 夜猫子任务已领奖"));
        return NightGrowthOutcome {
            ok: true,
            msg: None,
            logs,
            earned_credit: 0,
        };
    }

    // ⑥ 未接取：先接取。上游只对已接取的任务累计进度。
    if task.status == "not_accepted" {
        let codes = vec![NIGHT_TASK_CODE.to_string()];
        let acc_res = accept_tasks(client, &acc.api, &codes, ACCEPT_CHUNK, ep).await;
        if acc_res.accepted.is_empty() {
            // Python：`acc_res.get("msg") or ("; ".join(acc_res.get("reasons") or [])[:120])`
            let why = if acc_res.msg.is_empty() {
                let joined: String = acc_res.reasons.join("; ");
                joined.chars().take(120).collect()
            } else {
                acc_res.msg.clone()
            };
            let suffix = if why.is_empty() {
                String::new()
            } else {
                format!(": {why}")
            };
            logs.push(format!(
                "[{name}] 夜猫子任务未接取, 今晚跳过 (先解决接取失败{suffix})"
            ));
            return NightGrowthOutcome {
                ok: false,
                msg: None,
                logs,
                earned_credit: 0,
            };
        }
        logs.push(format!("[{name}] 夜猫子任务已接取"));
    }

    // ⑦ 已达标：直接领奖。
    let cur = task.current;
    let tgt = task.target;
    if cur >= tgt {
        let res = claim_task(client, &acc.api, NIGHT_TASK_CODE, ep).await;
        let cr = if res.ok { res.credit } else { 0 };
        let _ = credits_refresh();
        logs.push(format!(
            "[{name}] 夜猫子任务达标, 领奖 {}",
            claim_log_suffix(res.ok, res.credit, res.msg.as_deref())
        ));
        return NightGrowthOutcome {
            ok: res.ok,
            msg: None,
            logs,
            earned_credit: cr,
        };
    }

    // ⑧ 未达标：上报一次夜间对话事件，然后轮询进度。
    let ev = build_event(acc.api.uid, "cat", 0, now_ms(), None);
    let reported = report_events(client, ep.chat_base, acc.api.chat_headers, &[ev]).await;
    logs.push(format!(
        "[{name}] 上报夜间 GLM-5.2 对话事件: {} (进度 {cur}/{tgt})",
        if reported { "成功" } else { "失败" }
    ));

    // 轮询进度：最多 4 轮，每轮 `sleep(4)` 后重拉清单。
    // `fresh` 存「当前进度」而不是任务引用——Python 的 `fresh` 是任务 dict，
    // 但只用它的 `current`；这里存 `Option<i64>` 以便跨轮次存活
    // （任务列表每次都是新分配的 `Vec`，不能借用）。
    let mut fresh: Option<i64> = None;
    for _ in 0..POLL_ROUNDS {
        ctx.sleeper.sleep(POLL_INTERVAL_SECS).await;
        let latest = fetch_growth_tasks(client, &acc.api, ep).await;
        fresh = latest
            .iter()
            .find(|t| t.task_code == NIGHT_TASK_CODE)
            .map(|t| t.current);
        if fresh.is_some_and(|c| c > cur) {
            break;
        }
    }
    let new_cur = fresh.unwrap_or(cur);

    if new_cur >= tgt {
        let res = claim_task(client, &acc.api, NIGHT_TASK_CODE, ep).await;
        logs.push(format!(
            "[{name}] 夜猫子任务完成 {new_cur}/{tgt}, 领奖 {}",
            claim_log_suffix(res.ok, res.credit, res.msg.as_deref())
        ));
    } else if new_cur > cur {
        logs.push(format!(
            "[{name}] 今晚 +1 ({new_cur}/{tgt}), 明晚继续, 累计 3 天可领奖"
        ));
    } else {
        logs.push(format!(
            "[{name}] 已上报但进度暂未变化 ({new_cur}/{tgt}), 明晚调度器会继续累计"
        ));
    }

    let _ = credits_refresh();
    let earned = earned_from_logs(&logs);
    NightGrowthOutcome {
        ok: true,
        msg: None,
        logs,
        earned_credit: earned,
    }
}

/// 领奖日志的后缀：`✓ +N 积分` / `! 失败: <msg>`（Python `:747`、`:763`）。
fn claim_log_suffix(ok: bool, credit: i64, msg: Option<&str>) -> String {
    if ok {
        format!("✓ +{credit} 积分")
    } else {
        format!("! 失败: {}", msg.unwrap_or(""))
    }
}

/// 从日志里反推本次到账积分。
///
/// Python：`re.search(r"\+(\d+) 积分", line)` 逐行找，**后一行覆盖前一行**
/// ⇒ 取「最后一条含 `+N 积分` 的日志」的 N。
fn earned_from_logs(logs: &[String]) -> i64 {
    let mut earned = 0i64;
    for line in logs {
        if let Some(v) = first_plus_credit(line) {
            earned = v;
        }
    }
    earned
}

/// 一行里**第一处** `+<数字> 积分` 的数字（无则 `None`）。
fn first_plus_credit(line: &str) -> Option<i64> {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            let mut j = i + 1;
            let mut digits = String::new();
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                digits.push(bytes[j] as char);
                j += 1;
            }
            if !digits.is_empty() && line[j..].starts_with(" 积分") {
                return digits.parse::<i64>().ok();
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_credit_is_extracted_from_the_claim_log() {
        assert_eq!(first_plus_credit("[a] 领奖 ✓ +30 积分"), Some(30));
        assert_eq!(first_plus_credit("no credit here"), None);
        assert_eq!(first_plus_credit("[a] 今晚 +1 (2/3), 明晚继续"), None);
        assert_eq!(first_plus_credit("[a] 领奖 ! 失败: x"), None);
        // 数字后面必须紧跟「 积分」。
        assert_eq!(first_plus_credit("[a] +30积分"), None);
    }

    #[test]
    fn earned_takes_the_last_matching_line() {
        let logs = vec![
            "[a] 夜猫子任务达标, 领奖 ✓ +30 积分".to_string(),
            "[a] 今晚 +1 (2/3), 明晚继续, 累计 3 天可领奖".to_string(),
        ];
        assert_eq!(earned_from_logs(&logs), 30);
    }

    #[test]
    fn outcome_key_order_follows_python_branches() {
        let with_msg = NightGrowthOutcome {
            ok: false,
            msg: Some("国际版不适用国内成长任务中心".into()),
            logs: vec![],
            earned_credit: 0,
        };
        assert_eq!(
            with_msg
                .to_json()
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["ok", "msg", "logs", "earned_credit"]
        );
        let without_msg = NightGrowthOutcome {
            ok: true,
            msg: None,
            logs: vec!["x".into()],
            earned_credit: 0,
        };
        assert_eq!(
            without_msg
                .to_json()
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["ok", "earned_credit", "logs"]
        );
    }
}
