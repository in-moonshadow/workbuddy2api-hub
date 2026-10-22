//! 关卡 4.21（线 C）：**live 模型目录**（取价清单的 live 那一半）。
//!
//! 权威源：Python `wb_pricing.PriceRefresher.live_ids()`（`:2353-2387`）——
//! 经 `gateway.curated_live_sources(realm)` 取**两个 realm**（`intl` 然后
//! `cn`）的 live 目录，只收**不在静态目录里**的 id；任何异常 ⇒ 回落**空
//! 列表**（Python 注释：live 出问题只会少覆盖，不会缩小覆盖）。
//!
//! ## 分层
//!
//! - **HTTP 那一半**在 `wb_gateway::product_catalog`：拉 `<chat_upstream>/v3/config`
//!   并解析成 `(ids, credits)`（Python `wb_proxy.fetch_remote_product_config`
//!   + `parse_remote_catalog`）。
//! - **筛选/合并这一半**在本模块：`is_chat_model` / 变体后缀 / 免费兄弟
//!   （Python `wb_proxy.curate_remote_catalog`）+ 去重保序合并
//!   （Python `wb_pricing.priced_model_ids`）。
//!
//! ## 可注入接缝
//!
//! [`LiveCatalogSource`] 是唯一的取数入口：生产实现包 HTTP 那一半，测试实现
//! 返回写死的目录 ⇒ **契约测试永不触网**。
//!
//! ## 缓存与刷新节奏（Python 侧的事实）
//!
//! - 网关的 `/v1/models` 有 **300 秒**的 `_models_cache`；那是**网关目录**的
//!   缓存，不是本模块的。
//! - 取价线程默认每 **5 分钟**（`wb_pricing.DEFAULT_REFRESH_MINUTES`）跑一轮，
//!   每轮**重新**调一次 `live_ids()` ⇒ 本模块**没有自己的 TTL**，每轮现取。
//! - 远端抓取本身最多两次尝试（两个 UA）、单次超时 10 秒。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

use wb_gateway::product_catalog::ProductCatalog;

/// 会问远端产品目录的 realm。Python `wb_pricing.LIVE_CATALOG_REALMS`（`:995`）。
///
/// 顺序有意义：`intl` 先、`cn` 后——live id 按这个顺序进清单。
pub const LIVE_CATALOG_REALMS: [&str; 2] = ["intl", "cn"];

/// 变体后缀（区域版 `-sg` / 实验版 `-x`）。
///
/// 权威源：Python `wb_proxy.VARIANT_SUFFIXES`（`:10275`）= `("-sg", "-x")`。
///
/// ⚠️ **不要**用 `wb_pricing.py:1039` 的同名常量——那是**价格继承**用的
/// `(后缀, 可否剥离)` 表，与目录筛选不是一回事。
pub const VARIANT_SUFFIXES: [&str; 2] = ["-sg", "-x"];

/// 非 chat 模型（虚拟别名 + `lite`）。Python `wb_proxy.NON_CHAT_MODELS`（`:9937`）。
pub const NON_CHAT_MODELS: [&str; 7] = [
    "lite",
    "default-model",
    "fast-model",
    "balanced-model",
    "primary-model",
    "deep-model",
    "auto",
];

/// 非 chat 模型的前缀。Python `wb_proxy.NON_CHAT_PREFIXES`（`:9938`）。
pub const NON_CHAT_PREFIXES: [&str; 2] = ["codewise-", "completion-"];

/// 非 chat 模型的后缀。Python `wb_proxy.NON_CHAT_SUFFIXES`（`:9939`）。
pub const NON_CHAT_SUFFIXES: [&str; 3] = ["-image-alpha", "-image-alpha-edit", "-taco-completion"];

/// 国内版的选择器顺序。Python `wb_proxy.CN_UI_ORDER`（`:9950-9965`）。
pub const CN_UI_ORDER: [&str; 14] = [
    "hy4-preview-f",
    "hy3",
    "deepseek-v4.1-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5v-turbo",
    "minimax-m3",
    "kimi-k3-1",
    "kimi-k2.8-preview",
    "kimi-k2.7",
    "kimi-k2.6",
    "deepseek-v4-pro",
];

/// 国际版的选择器顺序。Python `wb_proxy.INTL_UI_ORDER`（`:9966-9984`）。
pub const INTL_UI_ORDER: [&str; 17] = [
    "hy4-preview-f",
    "hy3",
    "deepseek-v4.1-flash",
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "grok-4.7",
    "gemini-3.5-flash",
    "glm-5.3-flash",
    "glm-5.3",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.6",
    "kimi-k2.8-preview",
];

/// 注入式接缝的异步返回类型。
///
/// 与 `price_refresher::PriceSource` 同一个形状（不用 `async-trait`，避免引
/// 第三方依赖）。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// live 目录的取数接缝。
///
/// 实现方负责「取某个 realm 的**原始**远端目录」；**筛选规则由本模块统一
/// 执行**（Python 侧 `live_ids()` 刻意复用网关那一份筛选，不复制第二份）。
pub trait LiveCatalogSource: Send + Sync {
    /// 取某个 realm 的原始远端目录（id 列表 + credits）。
    ///
    /// 失败返回 `Err`——调用方把它当作「这个 realm 本轮没有 live 目录」，
    /// 继续问下一个 realm（对齐 Python `live_ids()` 的 per-realm `except`）。
    fn fetch(&self, realm: &str) -> BoxFut<'_, Result<ProductCatalog, String>>;
}

/// 这个 id 是不是可对话的模型。Python `wb_proxy.is_chat_model()`（`:9940-9949`）。
///
/// 剔除：虚拟别名（`default-model` … `auto`）、`lite`、`codewise-*`、
/// `completion-*`、`*-image-alpha` / `*-image-alpha-edit` / `*-taco-completion`。
pub fn is_chat_model(mid: &str) -> bool {
    if mid.is_empty() {
        return false;
    }
    if NON_CHAT_MODELS.contains(&mid) {
        return false;
    }
    if NON_CHAT_PREFIXES
        .iter()
        .any(|prefix| mid.starts_with(prefix))
    {
        return false;
    }
    if NON_CHAT_SUFFIXES.iter().any(|suffix| mid.ends_with(suffix)) {
        return false;
    }
    true
}

/// 把远端目录整理成「选择器应当展示」的模型列表。
///
/// Python `wb_proxy.curate_remote_catalog()`（`:10393-10428`）：
///
/// 1. 丢掉非 chat 模型（别名 / lite / `codewise-` / `completion-` / 图片模型…）；
/// 2. 丢掉变体后缀（`-sg` 区域版 / `-x` 实验版——实测**原名才是免费的**，
///    变体是付费的那个）；
/// 3. 有**免费兄弟**时丢掉付费的那个（免费兄弟才是选择器展示的那个）；
/// 4. 其余保持上游顺序。
///
/// `remote_credits` 是远端 meta 里的 credits（小写）；它**覆盖**静态目录的
/// credits（Python 的 `credits[mid] = ...` 是赋值不是 `setdefault`）。
pub fn curate_remote_catalog(
    realm: &str,
    ids: &[String],
    remote_credits: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut credits: HashMap<&str, &str> = crate::catalog::hub_credits_map();
    for (mid, value) in remote_credits.iter() {
        credits.insert(mid.as_str(), value.as_str());
    }

    let order: &[&str] = if realm == "cn" {
        &CN_UI_ORDER
    } else {
        &INTL_UI_ORDER
    };
    // Python `known = set(ids) | set(credits) | set(order)`。
    let mut known: HashSet<&str> = HashSet::new();
    for id in ids.iter() {
        known.insert(id.as_str());
    }
    for id in credits.keys() {
        known.insert(*id);
    }
    for id in order.iter() {
        known.insert(id);
    }

    let free = |mid: &str| -> bool {
        matches!(
            credits.get(mid).copied(),
            Some("x0.00") | Some("x0") | Some("0") | Some("0.00")
        )
    };

    let mut out: Vec<String> = Vec::new();
    for raw in ids.iter() {
        let mid = raw.trim();
        if mid.is_empty() {
            continue;
        }
        if !is_chat_model(mid) {
            continue;
        }
        if VARIANT_SUFFIXES.iter().any(|suffix| mid.ends_with(suffix)) {
            continue;
        }
        if let Some(base) = mid.strip_suffix("-f") {
            // `xxx-f` 的兄弟是 `xxx`：兄弟免费、自己不免费 ⇒ 丢掉自己。
            if known.contains(base) && free(base) && !free(mid) {
                continue;
            }
        } else {
            // `xxx` 的兄弟是 `xxx-f`：兄弟免费、自己不免费 ⇒ 丢掉自己。
            let sibling = format!("{mid}-f");
            if known.contains(sibling.as_str()) && free(&sibling) && !free(mid) {
                continue;
            }
        }
        out.push(mid.to_string());
    }
    out
}

/// 把静态目录与 live 目录合并成**取价输入清单**（去重保序，**静态在前**）。
///
/// Python `wb_pricing.priced_model_ids()`（`:1157-1175`）：静态目录先整体
/// 进去，live 的新增 id 追加在后；两段各自去重。
///
/// 静态段是泛型的（`S: AsRef<str>`）：调用方既有 `&[&str]`（静态目录常量
/// [`crate::catalog::hub_model_ids`]），也有 `&[String]`（取价线程手头的
/// 取价清单）。
pub fn merge_catalog<S: AsRef<str>>(static_ids: &[S], live_ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(static_ids.len() + live_ids.len());
    let mut seen: HashSet<String> = HashSet::new();
    for id in static_ids.iter() {
        let id = id.as_ref().trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        out.push(id.to_string());
    }
    for id in live_ids.iter() {
        let id = id.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        out.push(id.to_string());
    }
    out
}

/// live 目录取数的结果：id 列表 + 每个 realm 的失败原因。
///
/// 失败原因**不丢弃**：Python `PriceRefresher.live_ids()`（`wb_pricing.py:2373`）
/// 在 per-realm 失败时写一行日志（"live 目录取不到（%s），该区域本轮只按内置
/// 目录：%s"）再继续问下一个 realm。Rust 侧把这句话的**素材**带回来，由调用
/// 方（取价线程）写进自己的日志环。
#[derive(Debug, Clone, Default)]
pub struct LiveCollect {
    /// 静态目录之外的新增 id（跨 realm 去重保序）。
    pub ids: Vec<String>,
    /// 每个 realm 的失败原因（`(realm, 原因)`），**realm 顺序与请求顺序一致**。
    pub errors: Vec<(String, String)>,
}

/// 取两个 realm 的 live 目录，整理成「静态目录之外的新增 id」。
///
/// Python `PriceRefresher.live_ids()`（`:2368-2387`）：
///
/// - 两个 realm 都问（`intl` 先、`cn` 后）；
/// - **单个 realm 出错只跳过它**（另一个 realm 照常）；
/// - 已经在静态目录里的 id 不要（Python 的 `known = set(hub_model_ids())`）；
/// - 跨 realm 去重保序。
///
/// 全部失败 ⇒ 返回**空列表**（不是抛错）：静态目录必须照样取价，live 出问题
/// 只能留下缺口，不能缩小覆盖。
pub async fn collect_live_ids<S>(source: &S, static_ids: &[&str]) -> Vec<String>
where
    S: LiveCatalogSource + ?Sized,
{
    collect_live_ids_detailed(source, static_ids).await.ids
}

/// 同 [`collect_live_ids`]，但**额外带回每个 realm 的失败原因**。
///
/// Python 侧这两者本来就是一件事：`live_ids()` 在 per-realm 失败时写一行
/// 日志再继续。Rust 侧把「日志素材」单独带回，由取价线程写进自己的日志环
/// ——这样既不丢信息，又不让本模块依赖取价线程的类型。
///
/// 失败**不抛错**：live 出问题只能留下缺口，不能缩小覆盖（静态目录必须照
/// 样取价）。
pub async fn collect_live_ids_detailed<S>(source: &S, static_ids: &[&str]) -> LiveCollect
where
    S: LiveCatalogSource + ?Sized,
{
    let known: HashSet<&str> = static_ids.iter().copied().collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    for realm in LIVE_CATALOG_REALMS.iter() {
        let raw = match source.fetch(realm).await {
            Ok(catalog) => catalog,
            // Python 的 per-realm `except` ⇒ 记一笔、跳过这个 realm，继续下一个。
            Err(err) => {
                errors.push(((*realm).to_string(), err));
                continue;
            }
        };
        for mid in curate_remote_catalog(realm, &raw.ids, &raw.credits) {
            let mid = mid.trim();
            if mid.is_empty() || known.contains(mid) {
                continue;
            }
            if !seen.insert(mid.to_string()) {
                continue;
            }
            out.push(mid.to_string());
        }
    }
    LiveCollect { ids: out, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn chat_model_filter_drops_aliases_and_non_chat_families() {
        // 虚拟别名不是模型。
        for alias in [
            "default-model",
            "fast-model",
            "balanced-model",
            "primary-model",
            "deep-model",
            "auto",
            "lite",
        ] {
            assert!(!is_chat_model(alias), "{alias} 不该是 chat 模型");
        }
        assert!(!is_chat_model("codewise-foo"));
        assert!(!is_chat_model("completion-bar"));
        assert!(!is_chat_model("gemini-3.5-flash-image-alpha"));
        assert!(!is_chat_model("gemini-3.5-flash-image-alpha-edit"));
        assert!(!is_chat_model("gpt-5.3-taco-completion"));
        assert!(!is_chat_model(""));

        // 正常模型保留。
        assert!(is_chat_model("hy4-preview-f"));
        assert!(is_chat_model("deepseek-v4.1-flash"));
        assert!(is_chat_model("gpt-6-astra"));
    }

    #[test]
    fn curation_drops_the_variant_suffixes() {
        let curated =
            curate_remote_catalog("intl", &ids(&["hy3-x", "hy4-preview-sg"]), &BTreeMap::new());
        assert!(curated.is_empty(), "变体后缀要被丢掉：{curated:?}");
    }

    #[test]
    fn curation_keeps_the_free_sibling_and_drops_the_paid_one() {
        // 静态目录里 hy4-preview-f 是 x0.00（免费），hy4-preview 是 x0.29。
        // 远端同时列出两个时，只留免费的那个。
        let curated = curate_remote_catalog(
            "cn",
            &ids(&["hy4-preview", "hy4-preview-f"]),
            &BTreeMap::new(),
        );
        assert_eq!(curated, ids(&["hy4-preview-f"]));

        // 反过来：远端只列了付费的那个，但免费兄弟在静态目录里 ⇒ 付费的也被丢掉。
        let curated = curate_remote_catalog("cn", &ids(&["hy4-preview"]), &BTreeMap::new());
        assert!(curated.is_empty(), "付费的应当被免费兄弟挤掉：{curated:?}");
    }

    #[test]
    fn remote_credits_override_the_static_ones() {
        // 远端说 hy4-preview 现在免费了 ⇒ 它不再是「付费的那个」。
        let mut credits = BTreeMap::new();
        credits.insert("hy4-preview".to_string(), "x0.00".to_string());
        let curated = curate_remote_catalog("cn", &ids(&["hy4-preview"]), &credits);
        assert_eq!(curated, ids(&["hy4-preview"]));
    }

    #[test]
    fn curation_keeps_the_upstream_order() {
        let curated =
            curate_remote_catalog("intl", &ids(&["zeta", "alpha", "mid"]), &BTreeMap::new());
        assert_eq!(curated, ids(&["zeta", "alpha", "mid"]));
    }

    #[test]
    fn merge_keeps_static_first_and_dedups() {
        let merged = merge_catalog(&["a", "b"], &ids(&["c", "a", "c"]));
        assert_eq!(merged, ids(&["a", "b", "c"]));
    }
}
