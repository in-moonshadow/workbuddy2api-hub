//! 关卡 4.21（主会话）：live 模型目录的**生产来源**。
//!
//! 线 C 交付了接缝（[`crate::live_catalog::LiveCatalogSource`]）与 HTTP 那一半
//! （[`wb_gateway::product_catalog`]），但没把它接进取价线程。本模块补上生产
//! 实现：**用账号池里的凭据**拉两个 realm 的远端产品目录。
//!
//! 权威源：Python `wb_proxy.fetch_remote_product_config()`（`:10431-10467`）
//!
//! - 只支持 `intl` / `cn`（其它 realm 直接跳过）；
//! - 凭据取该 realm 的**代表账号**（Python `POOL.representative(realm=...)`，
//!   `wb_accounts.py:3327-3332`：先挑有 access_token 的，没有就挑第一个候选）；
//! - 请求 `<chat_upstream>/v3/config`，**最多两次尝试**（桌面 chat UA，再计费
//!   UA），单次超时 **10s**；
//! - 任何一步失败 ⇒ 该 realm 本轮**没有** live 目录（回落静态目录）。
//!
//! ## 与 Python 的已知差异（登记在案）
//!
//! Python 的 `curated_live_sources()`（`wb_proxy.py:10498-10523`）有**三级
//! 回落**：①远端产品目录 → ②桌面端缓存文件
//! （`~/.workbuddy-ai/cache/acc-product-config-v3.json`）→ ③旧的窄端点
//! （`read_product_config_models`）。**Rust 侧只移植了第 ① 级**——第 ② ③ 级
//! 依赖用户桌面端在本地写下的缓存文件，Rust 侧没有对应来源。
//! ⇒ 远端挂掉时 Rust 侧只剩静态目录（Python 还能用桌面缓存兜底）。这是
//! **少覆盖**，不是错误覆盖。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use wb_gateway::pool::AccountState;
use wb_gateway::product_catalog::{
    fetch_product_catalog_with_credits, product_config_url, CatalogAuth, ProductCatalog,
    DEFAULT_TIMEOUT_SECONDS,
};

use crate::api_routes::TaskRuntime;
use crate::live_catalog::LiveCatalogSource;

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 生产 live 目录来源：从 [`TaskRuntime`] 的账号池取凭据。
///
/// 持 `Weak`（不是 `Arc`）——`TaskRuntime` 本身装着取价刷新器，持有强引用会
/// 成环。参照 [`crate::credits_source::ProductionCreditsSource`] 的做法。
pub struct PoolLiveCatalogSource {
    task: Weak<TaskRuntime>,
}

impl PoolLiveCatalogSource {
    pub fn new(task: &Arc<TaskRuntime>) -> Self {
        Self {
            task: Arc::downgrade(task),
        }
    }
}

/// 挑一个该 realm 的代表账号（Python `POOL.representative()`）。
///
/// Python：先挑**有 access_token** 的候选，都没有就挑第一个候选（哪怕它没有
/// token——那样后面会因为「没有可用账号」而跳过）。
///
/// Rust 侧按同一口径实现：**有 token 的候选优先**；一个都没有 ⇒ 返回
/// `None`（调用方按「该 realm 没有可用账号」处理，与 Python
/// `if account is None or not account.access_token` 的效果一致）。
///
/// 返回 `(uid, access_token)`。
fn representative(accounts: &[AccountState], realm: &str) -> Option<(String, String)> {
    let mut fallback: Option<(String, String)> = None;
    for account in accounts.iter() {
        if account.realm != realm {
            continue;
        }
        if fallback.is_none() {
            fallback = Some((account.uid.clone(), account.access_token.clone()));
        }
        if !account.access_token.is_empty() {
            return Some((account.uid.clone(), account.access_token.clone()));
        }
    }
    fallback
}

impl LiveCatalogSource for PoolLiveCatalogSource {
    /// 拉某个 realm 的远端产品目录。
    ///
    /// 失败返回 `Err`——调用方把它当作「这个 realm 本轮没有 live 目录」，
    /// 继续问下一个 realm（对齐 Python `live_ids()` 的 per-realm `except`）。
    fn fetch(&self, realm: &str) -> BoxFut<'_, Result<ProductCatalog, String>> {
        // realm 借用不能进 future（future 的生命周期比 `&realm` 短）⇒ 复制一份。
        let realm = realm.to_string();
        Box::pin(async move {
            let task = self
                .task
                .upgrade()
                .ok_or_else(|| "任务运行时已释放".to_string())?;
            let (uid, token) = representative(&task.pool, &realm)
                .ok_or_else(|| format!("没有可用的 {realm} 账号"))?;
            if token.is_empty() {
                return Err(format!("没有可用的 {realm} 账号"));
            }
            let auth = CatalogAuth {
                access_token: token,
                user_id: uid,
            };
            let endpoint = product_config_url(&realm);
            fetch_product_catalog_with_credits(
                &task.client,
                &endpoint,
                &realm,
                &auth,
                DEFAULT_TIMEOUT_SECONDS,
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: &str, realm: &str, token: &str) -> AccountState {
        let mut state = AccountState::new(uid, realm);
        state.access_token = token.to_string();
        state.has_token = !token.is_empty();
        state
    }

    #[test]
    fn the_representative_prefers_an_account_with_a_token() {
        let accounts = vec![
            account("u-cn-empty", "cn", ""),
            account("u-cn-ok", "cn", "token-ok"),
        ];
        let picked = representative(&accounts, "cn").expect("必须有代表账号");
        assert_eq!(picked.0, "u-cn-ok");
        assert_eq!(picked.1, "token-ok");
    }

    #[test]
    fn a_realm_without_a_token_falls_back_to_its_first_candidate() {
        // Python：候选里没有带 token 的 ⇒ 挑第一个（随后因无 token 被跳过）。
        let accounts = vec![account("u-cn-empty", "cn", "")];
        let picked = representative(&accounts, "cn").expect("候选存在就有代表");
        assert_eq!(picked.0, "u-cn-empty");
        assert!(picked.1.is_empty());
    }

    #[test]
    fn a_realm_without_accounts_has_no_representative() {
        let accounts = vec![account("u-cn", "cn", "token")];
        assert!(
            representative(&accounts, "intl").is_none(),
            "没有 intl 账号时不该硬凑一个"
        );
    }
}
