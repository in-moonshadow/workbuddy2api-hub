//! 关卡 4.20（线 A）：hub 侧的**静态模型目录**（Rust 侧移植）。
//!
//! 权威源是 Python `wb_catalog.py`：
//!
//! - `STATIC_MODELS` / `STATIC_INTL_MODELS`（国际版，`:18-718`）：`_JSON` 里
//!   内嵌的那段 JSON，**24 条**。
//! - `STATIC_CN_MODELS`（国内版，`:720-1731`）：`_CN_JSON` 里内嵌的那段 JSON，
//!   **39 条**。
//!
//! ## 为什么只搬 id，不搬整份 JSON
//!
//! Python 的 `wb_pricing.hub_model_ids()`（`:1137-1155`）**只取 `item["id"]`**：
//!
//! ```python
//! for source in (wb_catalog.STATIC_INTL_MODELS, wb_catalog.STATIC_CN_MODELS):
//!     for item in source or []:
//!         mid = str(item.get("id") or "").strip()
//!         if mid and mid not in seen:
//!             seen.add(mid); ids.append(mid)
//! ```
//!
//! 取价链路上真正用到的也只有 id——`name` / `credits` / `maxInputTokens` 这些
//! 字段在 Python 侧**取价时并不读**（价格来自 OpenRouter 的价目表，不是目录
//! 里的 credits）。搬整份 JSON 等于把 1000+ 行 base64 图标搬进 Rust 二进制，
//! 却没有任何一处消费它 ⇒ 只搬 id 列表。
//!
//! ## 顺序与去重
//!
//! 与 Python 逐字一致：**国际版在前、国内版在后**，**去重保序**（先到先得）。
//! 两段合计 24 + 39 = 63 条，其中 **9 个 id 同时出现在两段**
//! （`deepseek-v4.1-flash` / `glm-5.2` / `glm-5.3` / `glm-5.3-flash` / `hy3` /
//! `hy4-preview` / `hy4-preview-f` / `kimi-k2.6` / `kimi-k2.8-preview`），
//! 去重后 **54 条**。
//!
//! ## 别名不剔除
//!
//! `default-model` / `fast-model` / `balanced-model` / `primary-model` /
//! `deep-model`（以及 `default-1.1` / `default-1.2`）这些**档位别名**保留在
//! 目录里：Python 侧也保留（`hub_model_ids()` 不做过滤），它们在 OpenRouter
//! 上匹配不到任何条目 ⇒ 自然落进快照的 `unpriced` 清单。**不剔除**是为了与
//! Python 的清单**逐条同构**——否则两侧的 `unpriced` 长度会不一样，
//! 「未定价」这个指标就不可比了。
//!
//! ## 已知缺口：live 目录
//!
//! Python 的 `priced_model_ids()`（`:1157-1192`）= 本静态目录 ∪ **网关 live
//! 目录新增**的模型。live 目录那一半需要「远端产品目录抓取 + `is_chat_model`
//! 过滤 + 变体整理」，**Rust 侧目前完全没有这条链路** ⇒ 本模块只覆盖静态
//! 目录。后果：Python 能在 live 目录里取到价、而 Rust 取不到的模型，会在
//! Rust 侧落进 `unpriced`。这是**已知缺口**，不是等价实现。
//!
//! ## 跨语言 parity
//!
//! `crates/tasks/tests/contract_catalog.rs` 直接读仓库里的 `wb_catalog.py`，
//! 解析出两段内嵌 JSON 的 id 列表，与 [`HUB_MODEL_IDS`] **逐字逐序**比对
//! （含条数）——目录漂移会在 CI 里直接红。

/// hub 静态模型 id 目录（去重保序，国际版在前）。
///
/// 与 Python `wb_pricing.hub_model_ids()`（`:1137-1155`）逐字逐序一致，
/// 共 **54** 条（`wb_catalog.py` 的 `STATIC_INTL_MODELS` 24 条 ∪ 去重后的
/// `STATIC_CN_MODELS` 30 条新 id）。
///
/// ⚠️ 这里只覆盖**静态目录**；Python `priced_model_ids()` 还会并上网关 live
/// 目录新增的模型，那一半 Rust 侧尚未落地（见模块文档的「已知缺口」）。
pub const HUB_MODEL_IDS: [&str; 54] = [
    // ---- 国际版（Python `wb_catalog.STATIC_INTL_MODELS`，24 条）----
    "default-model",
    "fast-model",
    "balanced-model",
    "primary-model",
    "deep-model",
    "deepseek-v4.1-flash",
    "gpt-6-astra",
    "hy4-preview-f",
    "hy4-preview",
    "hy3",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "grok-4.7",
    "gpt-5.3-codex",
    "gemini-3.5-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.8-preview",
    "kimi-k2.6",
    // ---- 国内版（Python `wb_catalog.STATIC_CN_MODELS`，39 条里新出现 30 条）----
    "hy3-x",
    "hy4-preview-dev",
    "hy4-preview-x",
    "minimax-m2.5",
    "glm-5v-turbo",
    "glm-5.1",
    "glm-5.0-turbo",
    "glm-4.6v",
    "kimi-k3-1",
    "kimi-k2.7",
    "kimi-k2.5",
    "kimi-k2-thinking",
    "minimax-m3",
    "minimax-m2.7",
    "glm-4.6",
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-v3-2-volc",
    "deepseek-v3-1-volc",
    "deepseek-v3-1-lkeap",
    "deepseek-v3-1",
    "deepseek-v3-0324-lkeap",
    "deepseek-r1-0528-lkeap",
    "kimi-k2-instruct-taiji",
    "default-1.1",
    "deepseek-r1-0528",
    "deepseek-v3-0324",
    "default-1.2",
    "hunyuan-2.0-instruct",
    "hunyuan-chat",
];

/// hub 静态模型 id 目录（借用形式）。
///
/// 等价于 Python `wb_pricing.hub_model_ids()`（`:1137-1155`）的返回值：
/// 去重保序、国际版在前。
pub fn hub_model_ids() -> &'static [&'static str] {
    &HUB_MODEL_IDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_keep_python_order() {
        let mut seen = std::collections::HashSet::new();
        for id in HUB_MODEL_IDS.iter() {
            assert!(!id.is_empty(), "id 不得为空串");
            assert!(seen.insert(*id), "id 重复：{id}");
        }
        assert_eq!(HUB_MODEL_IDS.len(), 54);
    }

    #[test]
    fn aliases_are_kept() {
        // 别名不剔除（与 Python 一致）——它们匹配不到 OpenRouter，落进 unpriced。
        for alias in [
            "default-model",
            "fast-model",
            "balanced-model",
            "primary-model",
            "deep-model",
        ] {
            assert!(
                HUB_MODEL_IDS.contains(&alias),
                "别名 {alias} 必须留在目录里"
            );
        }
    }

    #[test]
    fn hub_model_ids_returns_the_same_slice() {
        assert_eq!(hub_model_ids(), &HUB_MODEL_IDS[..]);
    }
}
