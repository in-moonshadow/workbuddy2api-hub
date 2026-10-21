//! 关卡 4.19（主会话）：`PriceRefresher` 的**生产数据源**。
//!
//! 线 A（`crates/tasks/src/price_refresher.rs`）交付了线程本体与可注入的
//! [`PriceSource`] trait，但没有生产实现——本文件补上：
//!
//! - 抓取走 `wb_gateway::pricing::fetch_openrouter_models`（HTTP 那一半）；
//! - 总开关走 `crate::settings::pricing_enabled`（**缺键 = 开**，
//!   Python `wb_settings.py:980-989`）；
//! - 数据目录走 `TaskRuntime.usage_dir`（`usage/`，与 `accounts/` 同级）。
//!
//! ⚠️ **取价清单仍是空的**（`catalog()` 用默认的空清单）：Rust 侧的模型目录
//! （Python `wb_catalog.STATIC_*_MODELS` ∪ live 目录）尚未落地 ⇒ 抓回来的
//! 模型列表匹配不到任何 hub 模型 ⇒ 走到「一个都没匹配上」分支、不写盘。
//! 这是**已知缺口**（与账本 D43 同一件事），不是等价实现。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use wb_gateway::outbound::UpstreamClient;

use crate::price_refresher::PriceSource;

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 生产数据源：settings 读总开关、`usage/` 写盘、OpenRouter 取价。
pub struct SettingsPriceSource {
    accounts_dir: PathBuf,
    usage_dir: PathBuf,
    client: UpstreamClient,
}

impl SettingsPriceSource {
    pub fn new(accounts_dir: &Path, usage_dir: &Path) -> Self {
        Self {
            accounts_dir: accounts_dir.to_path_buf(),
            usage_dir: usage_dir.to_path_buf(),
            client: UpstreamClient::new(),
        }
    }

    /// 当前 epoch 秒（等价 Python `time.time()`）。
    fn now_epoch() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}

impl PriceSource for SettingsPriceSource {
    fn fetch_models(&self) -> BoxFut<'_, Result<Value, String>> {
        Box::pin(async move {
            wb_gateway::pricing::fetch_openrouter_models(
                &self.client,
                wb_gateway::pricing::DEFAULT_TIMEOUT_SECONDS,
            )
            .await
        })
    }

    fn now(&self) -> f64 {
        Self::now_epoch()
    }

    fn usage_dir(&self) -> &Path {
        &self.usage_dir
    }

    /// 定价总开关：**缺键 = 开**（Python `wb_settings.py:980-989`）。
    ///
    /// 每次调用都**现读** settings（Python 的 `run_once()` 在锁内重读开关，
    /// `wb_pricing.py:2391-2393`）⇒ 面板上改开关能立刻生效，不用重启。
    fn master_enabled(&self) -> bool {
        let data =
            wb_storage::read_settings(&self.accounts_dir).unwrap_or_else(|_| serde_json::json!({}));
        crate::settings::pricing_enabled(&data)
    }
}

/// 造一个装好生产数据源的刷新器（接线方用）。
pub fn production_price_refresher(
    accounts_dir: &Path,
    usage_dir: &Path,
    interval_seconds: f64,
) -> Arc<crate::price_refresher::PriceRefresher> {
    Arc::new(crate::price_refresher::PriceRefresher::new(
        Arc::new(SettingsPriceSource::new(accounts_dir, usage_dir)),
        interval_seconds,
    ))
}
