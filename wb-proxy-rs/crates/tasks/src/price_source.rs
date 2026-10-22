//! 关卡 4.19（主会话）：`PriceRefresher` 的**生产数据源**。
//!
//! 线 A（`crates/tasks/src/price_refresher.rs`）交付了线程本体与可注入的
//! [`PriceSource`] trait，但没有生产实现——本文件补上：
//!
//! - 抓取走 `wb_gateway::pricing::fetch_openrouter_models`（HTTP 那一半）；
//! - 总开关走 `crate::settings::pricing_enabled`（**缺键 = 开**，
//!   Python `wb_settings.py:980-989`）；
//! - 数据目录走 `TaskRuntime.usage_dir`（`usage/`，与 `accounts/` 同级）；
//! - 取价清单走 [`crate::catalog`]（Python `wb_catalog.STATIC_*_MODELS` 的
//!   静态目录移植）+ 面板手填的 `usage/pricing-overrides.json`。
//!
//! ## 已知缺口：live 目录
//!
//! Python 的取价清单是「静态目录 ∪ **网关 live 目录新增**」
//! （`wb_pricing.priced_model_ids()`，`:1157-1192`）。Rust 侧只落地了
//! **静态目录**那一半（[`crate::catalog::HUB_MODEL_IDS`]）；live 目录需要
//! 「远端产品目录抓取 + `is_chat_model` 过滤」，Rust 侧完全缺这条链路。
//! ⇒ Python 能取到价、而 Rust 取不到的模型会落进 `unpriced`。这是**已知
//! 缺口**，不是等价实现。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use wb_gateway::outbound::UpstreamClient;

use crate::price_refresher::{PriceCatalog, PriceSource};

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 面板手填的 hub → OpenRouter id 映射文件名。
///
/// Python `wb_pricing._data_paths()`（`:1459-1466`）的第四个返回值，
/// 落在**数据目录**（= `usage/`）里，不随镜像升级被覆盖。
pub const OVERRIDES_FILENAME: &str = "pricing-overrides.json";

/// settings 里的变体后缀继承开关键名。
///
/// Python `wb_settings.PRICING_VARIANT_INHERIT_KEY`（`wb_settings.py:45`）。
/// ⚠️ `crates/tasks/src/settings.rs` 里**没有**这个键（那边只有
/// `pricing_enabled` / `pricing_refresh_minutes` 等），所以本文件就地定义，
/// 键名与 Python 逐字一致。
pub use crate::settings::PRICING_VARIANT_INHERIT_KEY;

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

    /// 变体后缀继承是否打开。**缺键 = 开**。
    ///
    /// Python `wb_pricing.variant_inherit_enabled()`（`:1254-1259`）→
    /// `wb_settings.pricing_variant_inherit()`（`wb_settings.py:955-968`）：
    /// 读 `accounts/settings.json` 的 `pricing_variant_inherit` 键，
    /// **缺键 = 开**；有键时只有布尔 `true` 算开（`value is True` 的严格
    /// 判定：数字 `1`、字符串 `"true"` 都不算）。
    fn variant_inherit_enabled(&self) -> bool {
        let data =
            wb_storage::read_settings(&self.accounts_dir).unwrap_or_else(|_| serde_json::json!({}));
        // 判定口径统一走 `crate::settings`（**唯一一份**定义：键名 + 语义）。
        // 本模块不再自带一份 `match`，免得两边漂移。
        crate::settings::pricing_variant_inherit(&data)
    }
}

/// Python `str(x or "")` 的等价写法——把 JSON 值转成「非空字符串」。
///
/// `None` / `False` / `0` / `""` 都是 Python 的 falsy ⇒ `x or ""` 落成空串；
/// 其余按 Python 的 `str()` 渲染（`True` → `"True"`，数字 → 十进制文本）。
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => String::new(),
        Value::Number(n) => {
            // `0` 也是 falsy ⇒ Python 的 `0 or ""` 落成空串。
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => String::new(),
    }
}

/// 读面板手填的 hub → OpenRouter id 映射（`usage/pricing-overrides.json`）。
///
/// Python `wb_pricing.runtime_overrides()`（`:1511-1525`）→
/// `_read_runtime_overrides()`（`:1495-1508`）：
///
/// - 文件缺失 / 读不出来 / JSON 损坏 / 顶层不是对象 ⇒ **空 dict**（不报错）；
/// - 键值两端都 `strip()`，**任一端为空就跳过**这条。
///
/// Python 按 `(path, mtime, size)` 缓存；这里**每次调用现读**——刷新周期是
/// 分钟级，读一次文件的代价可以忽略，而「面板改完立刻生效」更值得保。
fn read_runtime_overrides(usage_dir: &Path) -> HashMap<String, String> {
    let path = usage_dir.join(OVERRIDES_FILENAME);
    let Ok(text) = std::fs::read_to_string(&path) else {
        // 文件不存在 / 读不了 ⇒ 空 dict（Python 的 OSError 分支同样返回 {}）。
        return HashMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        // JSON 损坏 / 顶层不是对象 ⇒ 空 dict。
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (key, value) in map {
        let hub_id = key.trim().to_string();
        let reference = python_str(&value).trim().to_string();
        if !hub_id.is_empty() && !reference.is_empty() {
            out.insert(hub_id, reference);
        }
    }
    out
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

    /// 取价输入清单。
    ///
    /// Python `wb_pricing.priced_model_ids()`（`:1157-1192`）= 静态目录 ∪ live
    /// 目录新增。Rust 侧只有**静态目录**那一半（[`crate::catalog`]，移植自
    /// `wb_catalog.STATIC_*_MODELS`）——live 目录那条链路尚未落地（见模块
    /// 文档的「已知缺口」）。
    ///
    /// 另外两项：
    ///
    /// - `runtime_overrides`：面板手填的 `usage/pricing-overrides.json`，
    ///   **每次调用现读**；文件缺失 / 损坏 ⇒ 空表（不报错）。
    /// - `variants`：变体后缀继承开关，读 `accounts/settings.json` 的
    ///   `pricing_variant_inherit` 键，**缺键 = 开**（Python
    ///   `variant_inherit_enabled()`，`:1254-1259`）。
    fn catalog(&self) -> PriceCatalog {
        PriceCatalog {
            hub_ids: crate::catalog::hub_model_ids()
                .iter()
                .map(|id| id.to_string())
                .collect(),
            runtime_overrides: read_runtime_overrides(&self.usage_dir),
            variants: self.variant_inherit_enabled(),
        }
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
