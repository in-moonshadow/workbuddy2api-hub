//! 关卡 4.21（线 C）：live 模型目录的**契约测试**。
//!
//! 权威源：Python `wb_proxy.py`（`curated_live_sources` / `curate_remote_catalog`
//! / `is_chat_model` / `snapshot_credits`）与 `wb_pricing.py`
//! （`PriceRefresher.live_ids()` / `priced_model_ids()`）。
//!
//! 本文件覆盖四件事：
//!
//! 1. **跨语言 parity**：直接读仓库里的 `wb_catalog.py`，把两段内嵌 JSON 的
//!    credits 解析出来（国际版优先），与 Rust 的 `HUB_MODEL_CREDITS` 逐条比对。
//! 2. **合并语义**：静态在前、去重保序（对齐 `priced_model_ids`）。
//! 3. **容错**：live 取不到 ⇒ 回落**静态目录**（不是空清单、不是 panic）。
//! 4. **HTTP 那一半**：wiremock 假上游能吐出 id；上游 500 ⇒ 报错 ⇒ 回落静态。
//!
//! ⛔ 本文件**不发任何真实上游请求**：唯一的网络出口是 wiremock 假上游，
//! 端点由 [`wb_gateway::product_catalog`] 的 `endpoint` 参数注入。

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wb_gateway::outbound::UpstreamClient;
use wb_gateway::product_catalog::{
    fetch_product_catalog, fetch_product_catalog_with_credits, CatalogAuth, ProductCatalog,
};
use wb_tasks::catalog::{hub_model_ids, hub_model_ids_with_live, HUB_MODEL_CREDITS, HUB_MODEL_IDS};
use wb_tasks::live_catalog::{
    collect_live_ids, curate_remote_catalog, is_chat_model, merge_catalog, BoxFut,
    LiveCatalogSource,
};

/// 仓库里的 Python 权威源。
const CATALOG_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../wb_catalog.py");

// ---------------------------------------------------------------------------
// 测试替身：注入式 live 目录来源（永不触网）
// ---------------------------------------------------------------------------

/// 写死的 live 目录来源：按 realm 返回固定目录；指定 realm 可以「失败」。
struct StubSource {
    catalogs: HashMap<String, ProductCatalog>,
    failing: HashSet<String>,
}

impl StubSource {
    fn new() -> Self {
        Self {
            catalogs: HashMap::new(),
            failing: HashSet::new(),
        }
    }

    fn with(mut self, realm: &str, ids: &[&str]) -> Self {
        self.catalogs.insert(
            realm.to_string(),
            ProductCatalog {
                ids: ids.iter().map(|value| value.to_string()).collect(),
                credits: BTreeMap::new(),
            },
        );
        self
    }

    fn failing(mut self, realm: &str) -> Self {
        self.failing.insert(realm.to_string());
        self
    }
}

impl LiveCatalogSource for StubSource {
    fn fetch(&self, realm: &str) -> BoxFut<'_, Result<ProductCatalog, String>> {
        if self.failing.contains(realm) {
            let message = format!("{realm} 的 live 目录取不到");
            return Box::pin(async move { Err(message) });
        }
        let catalog = self.catalogs.get(realm).cloned().unwrap_or_default();
        Box::pin(async move { Ok(catalog) })
    }
}

/// 走 HTTP 那一半的来源：端点指向 wiremock 假上游。
struct HttpSource {
    client: UpstreamClient,
    base: String,
}

impl LiveCatalogSource for HttpSource {
    fn fetch(&self, realm: &str) -> BoxFut<'_, Result<ProductCatalog, String>> {
        let client = self.client.clone();
        let url = format!("{}/v3/config", self.base);
        let realm = realm.to_string();
        let auth = CatalogAuth {
            access_token: "tok-123".to_string(),
            user_id: "uid-9".to_string(),
        };
        Box::pin(async move {
            fetch_product_catalog_with_credits(&client, &url, &realm, &auth, 5.0).await
        })
    }
}

fn stub_ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

// ---------------------------------------------------------------------------
// 1. 跨语言 parity：静态 credits 表
// ---------------------------------------------------------------------------

/// 从 `wb_catalog.py` 里抠出 `NAME = r'''...'''` 的那段原始文本。
///
/// 选**行首锚点**（前置换行）而不是裸变量名：`_JSON` 是 `_CN_JSON` 的子串。
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

fn parse_models(src: &str, marker: &str) -> Vec<Value> {
    let text = python_raw_string(src, marker);
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{marker:?} 里的内嵌 JSON 解析失败：{err}"))
}

/// Python 的 `str(x or "")` 口径。
fn py_str_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => {
            if number.as_f64() == Some(0.0) {
                String::new()
            } else {
                number.to_string()
            }
        }
        Some(Value::Bool(flag)) => {
            if *flag {
                "True".to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Python `wb_proxy.snapshot_credits()`（`:10376-10390`）的等价：国际版在前、
/// `setdefault` 去重、credits 小写。
fn python_credits_table(src: &str) -> Vec<(String, String)> {
    let intl = parse_models(src, "\n_JSON = r'''");
    let cn = parse_models(src, "\n_CN_JSON = r'''");
    let mut out: Vec<(String, String)> = Vec::new();
    for models in [intl, cn] {
        for item in models {
            let mid = py_str_or_empty(item.get("id"));
            let mid = mid.trim().to_string();
            if mid.is_empty() {
                continue;
            }
            let credits = py_str_or_empty(item.get("credits")).trim().to_lowercase();
            if !out.iter().any(|(existing, _)| *existing == mid) {
                out.push((mid, credits));
            }
        }
    }
    out
}

#[test]
fn static_credits_table_matches_the_python_source_exactly() {
    let src = std::fs::read_to_string(CATALOG_PY).expect("读不到仓库里的 wb_catalog.py");
    let expected = python_credits_table(&src);

    assert_eq!(
        expected.len(),
        HUB_MODEL_CREDITS.len(),
        "credits 表条数与 Python 不一致"
    );
    for (idx, (want, got)) in expected.iter().zip(HUB_MODEL_CREDITS.iter()).enumerate() {
        assert_eq!(
            want.0.as_str(),
            got.0,
            "第 {idx} 条 credits 的 id 与 Python 不一致"
        );
        assert_eq!(
            want.1.as_str(),
            got.1,
            "第 {idx} 条 credits 的值与 Python 不一致"
        );
    }
}

#[test]
fn the_static_credits_table_keeps_the_intl_value_on_conflicts() {
    // deepseek-v4.1-flash：国际版 x0.00（免费）、国内版 x0.11 —— 取国际版。
    let entry = HUB_MODEL_CREDITS
        .iter()
        .find(|(mid, _)| *mid == "deepseek-v4.1-flash")
        .expect("表里必须有 deepseek-v4.1-flash");
    assert_eq!(entry.1, "x0.00");
}

// ---------------------------------------------------------------------------
// 2. 合并语义：静态在前、去重保序
// ---------------------------------------------------------------------------

#[test]
fn merge_keeps_static_first_and_preserves_order() {
    let merged = merge_catalog(&["a", "b", "c"], &stub_ids(&["d", "e"]));
    assert_eq!(merged, stub_ids(&["a", "b", "c", "d", "e"]));
}

#[test]
fn duplicate_live_ids_do_not_produce_duplicates() {
    let merged = merge_catalog(&["a"], &stub_ids(&["x", "x", "x"]));
    assert_eq!(merged, stub_ids(&["a", "x"]));
}

#[test]
fn live_ids_that_are_already_static_are_deduped() {
    let merged = merge_catalog(&["a", "b"], &stub_ids(&["b", "c", "a"]));
    assert_eq!(merged, stub_ids(&["a", "b", "c"]));
}

#[test]
fn the_merged_list_is_the_price_input_list() {
    let live = stub_ids(&["brand-new"]);
    let merged = hub_model_ids_with_live(&live);
    assert_eq!(merged.len(), HUB_MODEL_IDS.len() + 1);
    assert_eq!(&merged[..HUB_MODEL_IDS.len()], &HUB_MODEL_IDS[..]);
    assert_eq!(merged[HUB_MODEL_IDS.len()], "brand-new");
}

// ---------------------------------------------------------------------------
// 3. 容错：live 取不到 ⇒ 回落静态目录
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_fetch_failure_falls_back_to_the_static_catalog() {
    // 两个 realm 都失败。
    let source = StubSource::new().failing("intl").failing("cn");
    let live = collect_live_ids(&source, hub_model_ids()).await;
    assert!(
        live.is_empty(),
        "live 全失败时应当回落空列表，实际={live:?}"
    );

    // 关键：**取价清单仍是完整的静态目录**（不是空、不是抛错）。
    let merged = hub_model_ids_with_live(&live);
    assert_eq!(merged.len(), HUB_MODEL_IDS.len());
    assert_eq!(&merged[..], &HUB_MODEL_IDS[..]);
}

#[tokio::test]
async fn one_realm_failing_does_not_stop_the_other() {
    let source = StubSource::new()
        .failing("intl")
        .with("cn", &["cn-only-model"]);
    let live = collect_live_ids(&source, hub_model_ids()).await;
    assert_eq!(live, stub_ids(&["cn-only-model"]));
}

#[tokio::test]
async fn live_ids_already_in_the_static_catalog_are_dropped() {
    let source = StubSource::new()
        .with("intl", &["hy4-preview-f", "brand-new"])
        .with("cn", &["brand-new"]);
    let live = collect_live_ids(&source, hub_model_ids()).await;
    // hy4-preview-f 在静态目录里 ⇒ 不要；brand-new 两个 realm 都报 ⇒ 只留一份。
    assert_eq!(live, stub_ids(&["brand-new"]));
}

// ---------------------------------------------------------------------------
// 4. 筛选规则（`is_chat_model` / 变体后缀 / 免费兄弟）
// ---------------------------------------------------------------------------

#[test]
fn aliases_and_non_chat_models_are_not_live_models() {
    for alias in ["default-model", "fast-model", "auto", "lite"] {
        assert!(!is_chat_model(alias), "{alias} 不该是 chat 模型");
    }
    assert!(!is_chat_model("codewise-x"));
    assert!(!is_chat_model("gpt-5.3-taco-completion"));
    assert!(is_chat_model("brand-new"));
}

#[test]
fn the_variant_suffixes_are_dropped_by_the_curation() {
    let curated = curate_remote_catalog(
        "intl",
        &stub_ids(&["hy3-x", "deepseek-v4.1-flash-sg", "keep-me"]),
        &BTreeMap::new(),
    );
    assert_eq!(curated, stub_ids(&["keep-me"]));
}

#[test]
fn the_paid_sibling_loses_to_the_free_one() {
    // 静态目录里 hy4-preview-f 免费（x0.00）、hy4-preview 付费（x0.29）。
    let curated = curate_remote_catalog(
        "cn",
        &stub_ids(&["hy4-preview", "hy4-preview-f"]),
        &BTreeMap::new(),
    );
    assert_eq!(curated, stub_ids(&["hy4-preview-f"]));
}

// ---------------------------------------------------------------------------
// 5. HTTP 那一半（wiremock 假上游）
// ---------------------------------------------------------------------------

/// 一份最小的 `/v3/config` 应答。
fn config_payload() -> Value {
    serde_json::json!({
        "data": {
            "agents": {
                "cli": {"models": ["live-model-a", "live-model-b"]},
                "other": {"models": ["ignored"]}
            }
        },
        "models": [
            {"id": "live-model-a", "credits": "x1.00"},
            {"id": "live-model-b", "credits": "x2.00"}
        ]
    })
}

#[tokio::test]
async fn the_http_half_reads_ids_from_a_fake_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/config"))
        .and(header("authorization", "Bearer tok-123"))
        .and(header("x-user-id", "uid-9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(config_payload()))
        .mount(&server)
        .await;

    let client = UpstreamClient::new();
    let auth = CatalogAuth {
        access_token: "tok-123".to_string(),
        user_id: "uid-9".to_string(),
    };
    let ids = fetch_product_catalog(
        &client,
        &format!("{}/v3/config", server.uri()),
        "intl",
        &auth,
        5.0,
    )
    .await
    .expect("假上游必须成功");
    assert_eq!(ids, stub_ids(&["live-model-a", "live-model-b"]));
}

#[tokio::test]
async fn the_http_half_surfaces_upstream_500_as_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let client = UpstreamClient::new();
    let auth = CatalogAuth {
        access_token: "tok-123".to_string(),
        user_id: "uid-9".to_string(),
    };
    let err = fetch_product_catalog(
        &client,
        &format!("{}/v3/config", server.uri()),
        "intl",
        &auth,
        5.0,
    )
    .await
    .expect_err("上游 500 必须报错（而不是 panic 或返回空目录）");
    assert!(err.contains("500"), "错误文案要带状态码: {err}");
}

#[tokio::test]
async fn upstream_failure_leaves_the_price_input_list_intact() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&server)
        .await;

    let source = HttpSource {
        client: UpstreamClient::new(),
        base: server.uri(),
    };
    let live = collect_live_ids(&source, hub_model_ids()).await;
    assert!(live.is_empty(), "上游挂了 ⇒ live 为空，实际={live:?}");

    // 取价清单不受影响：静态目录一条不少。
    let merged = hub_model_ids_with_live(&live);
    assert_eq!(merged.len(), HUB_MODEL_IDS.len());
    assert_eq!(&merged[..], &HUB_MODEL_IDS[..]);
}

#[tokio::test]
async fn the_http_half_feeds_the_live_catalog_through_the_seam() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(config_payload()))
        .mount(&server)
        .await;

    let source = HttpSource {
        client: UpstreamClient::new(),
        base: server.uri(),
    };
    let live = collect_live_ids(&source, hub_model_ids()).await;
    // 两个 realm 都拿到同一份目录 ⇒ 去重后只剩两条。
    assert_eq!(live, stub_ids(&["live-model-a", "live-model-b"]));

    let merged = hub_model_ids_with_live(&live);
    assert_eq!(merged.len(), HUB_MODEL_IDS.len() + 2);
    assert_eq!(&merged[..HUB_MODEL_IDS.len()], &HUB_MODEL_IDS[..]);
}

/// 目录与价格链路无关：静态目录 id 的**相对顺序**不能被 live 目录打乱。
#[test]
fn the_static_order_is_never_disturbed_by_live_ids() {
    let live = stub_ids(&["aaa-first-in-live", "zzz-last-in-live"]);
    let merged = hub_model_ids_with_live(&live);
    let static_part = &merged[..HUB_MODEL_IDS.len()];
    assert_eq!(static_part, &HUB_MODEL_IDS[..]);
    assert_eq!(&merged[HUB_MODEL_IDS.len()..], &live[..]);
}
