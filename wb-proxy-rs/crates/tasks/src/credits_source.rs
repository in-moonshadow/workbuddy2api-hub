//! 关卡 4.18：积分刷新的**生产数据源**（[`CreditsSource`] 的生产实现）。
//!
//! 把两半分身接起来：
//! - HTTP 那一半在 `wb_gateway::credits`（只取回上游原文，不塑形）；
//! - 塑形那一半在 `crate::credits`（包解析 + 两条聚合口径）；
//! - 落盘在 `wb_storage::write_account_credits`（原子写回账号文件）。
//!
//! 权威源是 Python `wb_accounts.py` 的 `Account.fetch_credits()`（`:2138-2150`）
//! 与 `Account.save()`（`:1971-1972`）：抓取成功后**立刻**把快照写回账号文件，
//! 后台刷新器下次判断「快照有多旧」时读的就是这个文件。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::api_routes::TaskRuntime;
use crate::credits::{aggregate_detailed, aggregate_enterprise, packages_from_accounts};
use crate::credits_refresher::{CreditsSource, RefresherAccount};

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 生产数据源：把 [`TaskRuntime`] 的账号池 / 出站头 / 账号目录接进刷新器。
pub struct ProductionCreditsSource {
    task: Weak<TaskRuntime>,
}

impl ProductionCreditsSource {
    pub fn new(task: &Arc<TaskRuntime>) -> Self {
        Self {
            task: Arc::downgrade(task),
        }
    }
}

/// 当前 epoch 秒（等价 Python `time.time()`）。
fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl CreditsSource for ProductionCreditsSource {
    /// 账号池快照（Python 的 `self.pool.accounts`）。
    ///
    /// 「快照多旧」从**账号文件**里读（`credits.updated_at`）——那才是 Python
    /// 的落点（`account.credits` 存在账号 JSON 里），不是内存态。
    fn accounts(&self) -> Vec<RefresherAccount> {
        let Some(task) = self.task.upgrade() else {
            return Vec::new();
        };
        task.pool
            .iter()
            .map(|st| {
                let credits =
                    wb_storage::read_account_credits(&task.accounts_dir, &st.credential_file)
                        .ok()
                        .flatten();
                RefresherAccount {
                    uid: st.uid.clone(),
                    file_name: st.credential_file.clone(),
                    enabled: st.enabled,
                    has_token: st.has_token,
                    credits_updated_at: credits
                        .as_ref()
                        .and_then(|c| c.get("updated_at"))
                        .and_then(Value::as_f64),
                }
            })
            .collect()
    }

    fn now(&self) -> f64 {
        now_epoch()
    }

    /// 抓某个账号的积分快照（Python `Account.fetch_credits()` 的国内版详细口径）。
    ///
    /// 出站基址按**账号 realm** 取：计费基址取 `realm_config(realm).billing_upstream`，
    /// 签到状态端点挂在 **chat** 主机上（`chat_base_url(realm, "workbuddy")`），
    /// 与 Python `wb_accounts.py` 的 `_fetch_credits_raw` 一致。
    fn fetch_credits<'a>(&'a self, acc: &'a RefresherAccount) -> BoxFut<'a, Result<Value, String>> {
        Box::pin(async move {
            let task = self.task.upgrade().ok_or("runtime 已释放")?;
            let ra = task
                .ready
                .iter()
                .find(|r| r.state.uid == acc.uid)
                .ok_or("account not found")?;
            let realm_cfg = wb_gateway::endpoints::realm_config(&ra.state.realm);
            let chat_upstream = wb_gateway::endpoints::chat_base_url(&ra.state.realm, "workbuddy");
            let headers = ra.creds.billing_headers(ra.device_token.as_deref());
            let now = now_epoch();
            if !ra.state.enterprise_id.is_empty() {
                // 企业账号积分路径（Python `_fetch_credits_enterprise`）
                let ent = wb_gateway::credits::fetch_credits_enterprise(
                    &task.client,
                    realm_cfg.billing_upstream,
                    &headers,
                )
                .await?;
                let credits = aggregate_enterprise(
                    ent.used,
                    ent.size,
                    &ent.cycle_start,
                    &ent.cycle_end,
                    now,
                );
                return Ok(credits.to_json());
            }
            let detailed = wb_gateway::credits::fetch_credits_detailed(
                &task.client,
                realm_cfg.billing_upstream,
                &chat_upstream,
                &headers,
            )
            .await?;
            let packages = packages_from_accounts(&detailed.accounts, now);
            let credits = aggregate_detailed(
                packages,
                &detailed.summary_packages,
                detailed.is_paid_user,
                detailed.checkin,
                now,
            );
            Ok(credits.to_json())
        })
    }

    /// 把快照写回账号文件（Python 的 `self.save(...)`）。
    fn save_credits(&self, acc: &RefresherAccount, credits: &Value) -> Result<(), String> {
        let task = self
            .task
            .upgrade()
            .ok_or_else(|| "runtime 已释放".to_string())?;
        wb_storage::write_account_credits(&task.accounts_dir, &acc.file_name, credits)
            .map_err(|err| err.to_string())
    }
}
