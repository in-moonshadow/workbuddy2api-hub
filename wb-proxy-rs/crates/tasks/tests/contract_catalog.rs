//! 关卡 4.20（线 A）：hub 静态模型目录的**跨语言 parity** 与**匹配冒烟**。
//!
//! 权威源：Python `wb_catalog.py`（`STATIC_INTL_MODELS` / `STATIC_CN_MODELS`）
//! 与 `wb_pricing.py`（`hub_model_ids()` / `runtime_overrides()` /
//! `variant_inherit_enabled()`）。
//!
//! 本文件里有两类测试：
//!
//! 1. **跨语言 parity**（最重要）：直接读仓库里的 `wb_catalog.py`，把两段内嵌
//!    JSON 解析出来、取出 id 列表、去重保序，与 Rust 的 `HUB_MODEL_IDS`
//!    **逐字逐序**比对（含条数）。目录漂移 ⇒ CI 直接红。
//! 2. **匹配冒烟**：造一份 OpenRouter 模型表，走 `wb_tasks::price::build_snapshot`
//!    验证静态目录里的真实 hub id 能被匹配上、别名落进 `unpriced`、覆盖表生效。
//!
//! ⛔ 本文件**不发任何真实上游请求**。

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};
use tempfile::TempDir;

use wb_tasks::catalog::{hub_model_ids, HUB_MODEL_IDS};
use wb_tasks::price::build_snapshot;
use wb_tasks::price_refresher::PriceSource;
use wb_tasks::price_source::SettingsPriceSource;

/// 仓库里的 Python 权威源。
const CATALOG_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../wb_catalog.py");

// ---------------------------------------------------------------------------
// 1. 跨语言 parity：Rust 的 HUB_MODEL_IDS 必须与 Python 源逐字逐序一致
// ---------------------------------------------------------------------------

/// 从 `wb_catalog.py` 里抠出 `NAME = r'''...'''` 的那段原始文本。
///
/// 选**行首锚点**（前置换行）而不是裸变量名：`_JSON` 是 `_CN_JSON` 的子串，
/// 裸搜会命中后者的变体；加换行锚点后两个标记各自唯一。
///
/// 段内是**合法 JSON**（Python 侧刻意用 `r'''` 包 JSON，`true/false/null` 与
/// JSON 一致），所以直接交给 `serde_json` 解析即可——不需要退化成正则抽
/// `"id": "..."`（正则会漏掉嵌套结构里的 id、也会被描述文本里的同形串干扰）。
fn python_raw_string(src: &str, marker: &str) -> String {
    let start = src
        .find(marker)
        .unwrap_or_else(|| panic!("wb_catalog.py 里找不到 {marker:?}"));
    let body_start = start + marker.len();
    let rel_end = src[body_start..]
        .find("'''")
        .unwrap_or_else(|| panic!("{marker:?} 之后找不到结束的 '''"));
    src[body_start..body_start + rel_end].to_string()
}

/// 解析一段内嵌 JSON，取出全部模型条目。
fn parse_models(src: &str, marker: &str) -> Vec<Value> {
    let text = python_raw_string(src, marker);
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{marker:?} 里的内嵌 JSON 解析失败：{err}"))
}

/// 按 Python `hub_model_ids()` 的口径取 id：**去重保序、先到先得**。
fn python_ids(models: &[Value]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in models {
        let mid = item
            .get("id")
            .map(|v| match v {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        if !mid.is_empty() && !seen.contains(&mid) {
            seen.push(mid.clone());
            ids.push(mid);
        }
    }
    ids
}

#[test]
fn rust_catalog_matches_the_python_source_exactly() {
    let src = std::fs::read_to_string(CATALOG_PY).expect("读不到仓库里的 wb_catalog.py");

    let intl = parse_models(&src, "\n_JSON = r'''");
    let cn = parse_models(&src, "\n_CN_JSON = r'''");

    // 两段的**原始条目数**——Python 侧 `STATIC_INTL_MODELS` 24 条、
    // `STATIC_CN_MODELS` 39 条。
    assert_eq!(intl.len(), 24, "国际版静态目录条目数变了");
    assert_eq!(cn.len(), 39, "国内版静态目录条目数变了");

    // Python `hub_model_ids()`：国际版在前、国内版在后，去重保序。
    let mut expected = python_ids(&intl);
    for id in python_ids(&cn) {
        if !expected.contains(&id) {
            expected.push(id);
        }
    }

    assert_eq!(
        expected.len(),
        54,
        "去重后的 hub id 条数变了（24 + 39 = 63，其中 9 个 id 两段都有）"
    );
    assert_eq!(
        HUB_MODEL_IDS.len(),
        expected.len(),
        "Rust 的 HUB_MODEL_IDS 条数与 Python 不一致"
    );

    // 逐字逐序比对：任一条 id 的**大小写 / 下划线 / 连字符**不同都会红。
    for (idx, (want, got)) in expected.iter().zip(HUB_MODEL_IDS.iter()).enumerate() {
        assert_eq!(
            want, got,
            "第 {idx} 条 hub id 与 Python 不一致（Python={want:?}, Rust={got:?}）"
        );
    }

    // 顺序敏感的集合相等（防「条数相同但整体重排」）。
    let mut want_sorted = expected.clone();
    let mut got_sorted: Vec<String> = HUB_MODEL_IDS.iter().map(|s| s.to_string()).collect();
    want_sorted.sort();
    got_sorted.sort();
    assert_eq!(want_sorted, got_sorted, "hub id 集合与 Python 不一致");
}

#[test]
fn hub_model_ids_accessor_returns_the_same_order() {
    assert_eq!(hub_model_ids(), &HUB_MODEL_IDS[..]);
    // 国际版在前：前 24 条应当与 Python 的国际版段一致（去重后）。
    let src = std::fs::read_to_string(CATALOG_PY).expect("读不到 wb_catalog.py");
    let intl = parse_models(&src, "\n_JSON = r'''");
    let intl_ids = python_ids(&intl);
    assert_eq!(
        &HUB_MODEL_IDS[..intl_ids.len()],
        &intl_ids[..],
        "国际版必须排在前面"
    );
}

// ---------------------------------------------------------------------------
// 2. 匹配冒烟：静态目录里的真实 hub id 能被 OpenRouter 表匹配上
// ---------------------------------------------------------------------------

/// 造一条 OpenRouter 模型记录（`pricing` 里的键名与 OpenRouter 实际返回一致：
/// `prompt` / `completion` / `input_cache_read`）。
fn or_model(id: &str, prompt: f64, completion: f64) -> Value {
    json!({
        "id": id,
        "pricing": {
            "prompt": prompt,
            "completion": completion,
            "input_cache_read": 0.0,
        }
    })
}

/// 一份小而有代表性的 OpenRouter 模型表。
///
/// 归一化后（`price::normalize`：小写 + 去掉非字母数字）互不撞名，否则
/// `auto_match` 的「唯一命中」判定会失效。
fn openrouter_models() -> HashMap<String, Value> {
    let mut table = HashMap::new();
    for m in [
        or_model("tencent/hy4-preview", 1.0e-6, 4.0e-6),
        or_model("moonshotai/kimi-k3", 6.0e-7, 2.5e-6),
        or_model("deepseek/deepseek-chat-v3.1", 2.8e-7, 4.2e-7),
        or_model("deepseek/deepseek-r1-0528", 4.0e-7, 1.6e-6),
        or_model("tencent/hunyuan-a13b-instruct", 1.0e-7, 3.0e-7),
    ] {
        let id = m["id"].as_str().unwrap().to_string();
        table.insert(id, m);
    }
    table
}

fn hub_ids() -> Vec<String> {
    HUB_MODEL_IDS.iter().map(|s| s.to_string()).collect()
}

#[test]
fn static_catalog_ids_match_against_the_openrouter_table() {
    let table = openrouter_models();
    let overrides: HashMap<String, String> = HashMap::new();
    let build = build_snapshot(
        Some(&table),
        &hub_ids(),
        None,
        &overrides,
        true,
        "2026-10-21T00:00:00Z",
    );
    let models = &build.doc["models"];

    // 真实 id 命中（归一化后全等唯一）。
    assert_eq!(
        models["hy4-preview"]["or_id"], "tencent/hy4-preview",
        "hy4-preview 应当按名字自动匹配上"
    );
    assert_eq!(models["kimi-k3"]["or_id"], "moonshotai/kimi-k3");
    assert_eq!(
        models["deepseek-r1-0528"]["or_id"],
        "deepseek/deepseek-r1-0528"
    );

    // 源码覆盖表命中（`price::OVERRIDES`）。
    assert_eq!(
        models["hy4-preview-f"]["or_id"], "tencent/hy4-preview",
        "hy4-preview-f 应当走源码覆盖表"
    );
    assert_eq!(models["hy4-preview-f"]["via"], "override");
    assert_eq!(
        models["hunyuan-chat"]["or_id"],
        "tencent/hunyuan-a13b-instruct"
    );

    // 剥后缀继承（`-lkeap` 可剥离）。
    assert_eq!(
        models["deepseek-v3-1-lkeap"]["or_id"], "deepseek/deepseek-chat-v3.1",
        "deepseek-v3-1-lkeap 应当继承 deepseek-v3-1 的价"
    );
    assert_eq!(models["deepseek-v3-1-lkeap"]["via"], "variant");
    assert_eq!(
        models["deepseek-v3-1-lkeap"]["inherited_from"],
        "deepseek-v3-1"
    );

    // 价格确实落进了快照（不是空壳）。
    assert!(models["hy4-preview"]["flat"]["input_cache_miss"].is_number());
}

#[test]
fn alias_ids_land_in_unpriced() {
    let table = openrouter_models();
    let overrides: HashMap<String, String> = HashMap::new();
    let build = build_snapshot(
        Some(&table),
        &hub_ids(),
        None,
        &overrides,
        true,
        "2026-10-21T00:00:00Z",
    );

    // 档位别名在 OpenRouter 上匹配不到任何条目 ⇒ 落进未定价清单，
    // 但**不算缺口**（Python 侧同样保留它们）。
    for alias in [
        "default-model",
        "fast-model",
        "balanced-model",
        "primary-model",
        "deep-model",
    ] {
        assert!(
            build.unpriced.contains(&alias.to_string()),
            "别名 {alias} 应当落进 unpriced，实际 unpriced={:?}",
            build.unpriced
        );
        assert!(
            build.doc["models"].get(alias).is_none(),
            "别名 {alias} 不该有价格条目"
        );
    }
}

#[test]
fn runtime_overrides_take_priority_over_the_static_table() {
    let table = openrouter_models();
    let mut overrides: HashMap<String, String> = HashMap::new();
    overrides.insert(
        "default-model".to_string(),
        "tencent/hy4-preview".to_string(),
    );

    let build = build_snapshot(
        Some(&table),
        &hub_ids(),
        None,
        &overrides,
        true,
        "2026-10-21T00:00:00Z",
    );
    assert_eq!(
        build.doc["models"]["default-model"]["or_id"], "tencent/hy4-preview",
        "手填覆盖表应当优先于静态匹配"
    );
    assert!(
        !build.unpriced.contains(&"default-model".to_string()),
        "被覆盖的别名不该再落进 unpriced"
    );
}

// ---------------------------------------------------------------------------
// 3. runtime_overrides 的读取口径（生产数据源）
// ---------------------------------------------------------------------------

fn make_source(usage_dir: &Path, accounts_dir: &Path) -> SettingsPriceSource {
    SettingsPriceSource::new(accounts_dir, usage_dir)
}

#[test]
fn catalog_reports_the_static_ids_and_reads_overrides() {
    let dir = TempDir::new().unwrap();
    let usage_dir = dir.path().join("usage");
    let accounts_dir = dir.path().join("accounts");
    std::fs::create_dir_all(&usage_dir).unwrap();
    std::fs::create_dir_all(&accounts_dir).unwrap();

    // 覆盖表存在 ⇒ 被读出来。
    std::fs::write(
        usage_dir.join("pricing-overrides.json"),
        r#"{"default-model": "tencent/hy4-preview", "hy3": "tencent/hy3"}"#,
    )
    .unwrap();

    let source = make_source(&usage_dir, &accounts_dir);
    let catalog = source.catalog();

    assert_eq!(
        catalog.hub_ids,
        hub_ids(),
        "catalog() 必须返回完整的静态目录"
    );
    assert_eq!(catalog.hub_ids.len(), 54);
    assert_eq!(
        catalog.runtime_overrides.get("default-model"),
        Some(&"tencent/hy4-preview".to_string())
    );
    assert_eq!(
        catalog.runtime_overrides.get("hy3"),
        Some(&"tencent/hy3".to_string())
    );
    assert!(catalog.variants, "变体继承缺键 = 开");
}

#[test]
fn missing_or_corrupt_overrides_fall_back_to_an_empty_map() {
    let dir = TempDir::new().unwrap();
    let usage_dir = dir.path().join("usage");
    let accounts_dir = dir.path().join("accounts");
    std::fs::create_dir_all(&usage_dir).unwrap();
    std::fs::create_dir_all(&accounts_dir).unwrap();

    let source = make_source(&usage_dir, &accounts_dir);

    // 文件不存在 ⇒ 空 map，不 panic。
    let catalog = source.catalog();
    assert!(
        catalog.runtime_overrides.is_empty(),
        "覆盖表缺失时应回落空 map"
    );
    assert_eq!(catalog.hub_ids.len(), 54);

    // 文件损坏 ⇒ 空 map，不 panic。
    std::fs::write(
        usage_dir.join("pricing-overrides.json"),
        "{ not json at all",
    )
    .unwrap();
    let catalog = source.catalog();
    assert!(
        catalog.runtime_overrides.is_empty(),
        "覆盖表损坏时应回落空 map"
    );

    // 顶层不是对象 ⇒ 空 map。
    std::fs::write(usage_dir.join("pricing-overrides.json"), "[1, 2, 3]").unwrap();
    let catalog = source.catalog();
    assert!(catalog.runtime_overrides.is_empty());

    // 空串的键值对被跳过（Python 侧同样跳过）。
    std::fs::write(
        usage_dir.join("pricing-overrides.json"),
        r#"{"  ": "tencent/hy4-preview", "hy3": "   "}"#,
    )
    .unwrap();
    let catalog = source.catalog();
    assert!(
        catalog.runtime_overrides.is_empty(),
        "两端有空白的条目应当被跳过，实际={:?}",
        catalog.runtime_overrides
    );
}

#[test]
fn variant_inherit_switch_follows_the_python_settings_key() {
    let dir = TempDir::new().unwrap();
    let usage_dir = dir.path().join("usage");
    let accounts_dir = dir.path().join("accounts");
    std::fs::create_dir_all(&usage_dir).unwrap();
    std::fs::create_dir_all(&accounts_dir).unwrap();
    let source = make_source(&usage_dir, &accounts_dir);

    // 缺键 ⇒ 开。
    assert!(source.catalog().variants);

    // 布尔 false ⇒ 关。
    std::fs::write(
        accounts_dir.join("settings.json"),
        r#"{"pricing_variant_inherit": false}"#,
    )
    .unwrap();
    assert!(!source.catalog().variants);

    // 布尔 true ⇒ 开。
    std::fs::write(
        accounts_dir.join("settings.json"),
        r#"{"pricing_variant_inherit": true}"#,
    )
    .unwrap();
    assert!(source.catalog().variants);

    // 严格判定：字符串 "true" 不算开（Python 的 `value is True`）。
    std::fs::write(
        accounts_dir.join("settings.json"),
        r#"{"pricing_variant_inherit": "true"}"#,
    )
    .unwrap();
    assert!(!source.catalog().variants);
}
