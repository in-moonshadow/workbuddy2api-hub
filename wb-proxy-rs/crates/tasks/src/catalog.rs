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
//! ## live 目录：静态目录之外的另一半
//!
//! Python 的 `priced_model_ids()`（`:1157-1192`）= 本静态目录 ∪ **网关 live
//! 目录新增**的模型。live 目录那一半（远端产品目录抓取 + `is_chat_model`
//! 过滤 + 变体整理 + 免费兄弟规则）在 [`crate::live_catalog`]；本模块提供
//! [`hub_model_ids_with_live`]——把两者合成**取价输入清单**（静态在前、
//! 去重保序）。
//!
//! 取价链路另外需要**静态目录的 credits**（回答「这个模型是不是免费的」
//! ⇒ 免费兄弟规则），所以本模块除 id 外还搬了一份 credits 表
//! （[`HUB_MODEL_CREDITS`]，对齐 Python `wb_proxy.snapshot_credits()`）。
//!
//! ## 跨语言 parity
//!
//! `crates/tasks/tests/contract_catalog.rs` 直接读仓库里的 `wb_catalog.py`，
//! 解析出两段内嵌 JSON 的 id 列表，与 [`HUB_MODEL_IDS`] **逐字逐序**比对
//! （含条数）——目录漂移会在 CI 里直接红。
//! `crates/tasks/tests/contract_live_catalog.rs` 另有一份 parity：把
//! `wb_catalog.py` 里的 credits 与 [`HUB_MODEL_CREDITS`] 逐条比对。

/// hub 静态模型 id 目录（去重保序，国际版在前）。
///
/// 与 Python `wb_pricing.hub_model_ids()`（`:1137-1155`）逐字逐序一致，
/// 共 **54** 条（`wb_catalog.py` 的 `STATIC_INTL_MODELS` 24 条 ∪ 去重后的
/// `STATIC_CN_MODELS` 30 条新 id）。
///
/// ⚠️ 这里只覆盖**静态目录**；Python `priced_model_ids()` 还会并上网关 live
/// 目录新增的模型——那一半在 [`crate::live_catalog`]，用
/// [`hub_model_ids_with_live`] 合成完整清单。
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

/// hub 静态模型 id 目录（**含 live 目录新增**）——取价输入清单。
///
/// 等价于 Python `wb_pricing.priced_model_ids(extra_ids)`（`:1157-1175`）：
/// **静态在前**、live 在后、去重保序。`live` 传空切片 ⇒ 就是静态目录本身。
///
/// ⚠️ `live` 应当是**已经整理过**的 live id（见 [`crate::live_catalog`] 的
/// `collect_live_ids`）：别名、变体后缀、付费的「免费兄弟」在那里已经剔除。
pub fn hub_model_ids_with_live(live: &[String]) -> Vec<String> {
    crate::live_catalog::merge_catalog(hub_model_ids(), live)
}

/// hub 静态目录里每个模型的 credits（**小写**，国际版在前）。
///
/// 权威源：Python `wb_proxy.snapshot_credits()`（`:10376-10390`）——它遍历
/// `STATIC_INTL_MODELS` 再 `STATIC_CN_MODELS`，用 `setdefault` ⇒ **国际版
/// 优先**。
///
/// 这个优先级有实质影响：`deepseek-v4.1-flash` 国际版是 `x0.00`（免费）、
/// 国内版是 `x0.11`；按国际版算，它才是「免费的那个」。
///
/// 取价链路用这份表回答「有没有免费的兄弟」——远端只列了付费变体时，
/// 免费的那个才是选择器应当展示的（Python `curate_remote_catalog`）。
pub const HUB_MODEL_CREDITS: [(&str, &str); 54] = [
    ("default-model", ""),
    ("fast-model", "x0.34 credits"),
    ("balanced-model", "x0.59 credits"),
    ("primary-model", "x3.31 credits"),
    ("deep-model", "x3.33 credits"),
    ("deepseek-v4.1-flash", "x0.00"),
    ("gpt-6-astra", "x6.67"),
    ("hy4-preview-f", "x0.00"),
    ("hy4-preview", "x0.29"),
    ("hy3", "x0.00"),
    ("gpt-5.6-sol", "x3.47"),
    ("gpt-5.6-terra", "x1.39"),
    ("gpt-5.6-luna", "x0.14"),
    ("gpt-5.5", "x3.31"),
    ("gpt-5.4", "x1.65"),
    ("grok-4.7", "x1.90"),
    ("gpt-5.3-codex", "x1.25"),
    ("gemini-3.5-flash", "x0.99"),
    ("glm-5.3", "x0.79"),
    ("glm-5.3-flash", "x0.06"),
    ("glm-5.2", "x0.79"),
    ("kimi-k3", "x1.62"),
    ("kimi-k2.8-preview", "x0.77"),
    ("kimi-k2.6", "x0.52"),
    ("hy3-x", "x0.05"),
    ("hy4-preview-dev", "x0.29"),
    ("hy4-preview-x", "x0.29"),
    ("minimax-m2.5", "x0.18"),
    ("glm-5v-turbo", "x0.71"),
    ("glm-5.1", "x0.79"),
    ("glm-5.0-turbo", "x0.95"),
    ("glm-4.6v", "x0.11"),
    ("kimi-k3-1", "x1.62"),
    ("kimi-k2.7", "x0.57"),
    ("kimi-k2.5", "x0.45"),
    ("kimi-k2-thinking", "x0.54"),
    ("minimax-m3", "x0.25"),
    ("minimax-m2.7", "x0.26"),
    ("glm-4.6", "x0.23"),
    ("deepseek-v4-flash", "x0.17"),
    ("deepseek-v4-pro", "x0.51"),
    ("deepseek-v3-2-volc", "x0.29"),
    ("deepseek-v3-1-volc", "x0.52"),
    ("deepseek-v3-1-lkeap", "x0.52"),
    ("deepseek-v3-1", "x0.52"),
    ("deepseek-v3-0324-lkeap", "x0.52"),
    ("deepseek-r1-0528-lkeap", ""),
    ("kimi-k2-instruct-taiji", ""),
    ("default-1.1", ""),
    ("deepseek-r1-0528", ""),
    ("deepseek-v3-0324", ""),
    ("default-1.2", ""),
    ("hunyuan-2.0-instruct", ""),
    ("hunyuan-chat", ""),
];

/// 查一个模型的静态 credits（小写）。未知 id ⇒ `None`。
pub fn hub_credit(id: &str) -> Option<&'static str> {
    HUB_MODEL_CREDITS
        .iter()
        .find(|(mid, _)| *mid == id)
        .map(|(_, credits)| *credits)
}

/// 静态目录的 credits 表（id → 小写 credits）。
///
/// 与 Python `snapshot_credits()` 逐条一致（含**国际版优先**的取值）。
pub fn hub_credits_map() -> std::collections::HashMap<&'static str, &'static str> {
    HUB_MODEL_CREDITS.iter().copied().collect()
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

    #[test]
    fn credits_table_covers_every_static_id_exactly_once() {
        assert_eq!(HUB_MODEL_CREDITS.len(), HUB_MODEL_IDS.len());
        for id in HUB_MODEL_IDS.iter() {
            assert!(hub_credit(id).is_some(), "credits 表缺少静态 id {id}");
        }
        // credits 已经小写化（Python `snapshot_credits()` 的 `.lower()`）。
        for (id, credits) in HUB_MODEL_CREDITS.iter() {
            assert_eq!(
                *credits,
                credits.to_lowercase(),
                "{id} 的 credits 必须是小写"
            );
        }
    }

    #[test]
    fn intl_credits_win_over_cn_ones() {
        // deepseek-v4.1-flash：国际版 x0.00（免费）、国内版 x0.11 —— 取国际版。
        assert_eq!(hub_credit("deepseek-v4.1-flash"), Some("x0.00"));
        // 另外两个免费模型。
        assert_eq!(hub_credit("hy4-preview-f"), Some("x0.00"));
        assert_eq!(hub_credit("hy3"), Some("x0.00"));
        assert_eq!(hub_credit("does-not-exist"), None);
    }

    #[test]
    fn hub_model_ids_with_live_keeps_static_first() {
        let live = vec!["brand-new".to_string()];
        let merged = hub_model_ids_with_live(&live);
        assert_eq!(merged.len(), HUB_MODEL_IDS.len() + 1);
        assert_eq!(&merged[..HUB_MODEL_IDS.len()], &HUB_MODEL_IDS[..]);
        assert_eq!(merged[HUB_MODEL_IDS.len()], "brand-new");
    }

    #[test]
    fn hub_model_ids_with_live_dedups_and_keeps_order() {
        let live = vec![
            "zeta".to_string(),
            HUB_MODEL_IDS[0].to_string(),
            "zeta".to_string(),
            "alpha".to_string(),
        ];
        let merged = hub_model_ids_with_live(&live);
        assert_eq!(merged.len(), HUB_MODEL_IDS.len() + 2);
        assert_eq!(&merged[..HUB_MODEL_IDS.len()], &HUB_MODEL_IDS[..]);
        assert_eq!(merged[HUB_MODEL_IDS.len()], "zeta");
        assert_eq!(merged[HUB_MODEL_IDS.len() + 1], "alpha");
    }

    #[test]
    fn empty_live_leaves_the_static_catalog_untouched() {
        let merged = hub_model_ids_with_live(&[]);
        assert_eq!(merged.len(), HUB_MODEL_IDS.len());
        for (want, got) in HUB_MODEL_IDS.iter().zip(merged.iter()) {
            assert_eq!(*want, got);
        }
    }
}
