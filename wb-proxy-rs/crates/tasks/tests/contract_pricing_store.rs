//! 关卡 4.21（线 A）产出物：契约 —— 定价策略库的**读取端** + 成本估算。
//!
//! 权威源：Python `wb_pricing.py`（`load_pricing` :793 / `compute_row` :918 /
//! `sum_rows` :954）与 `wb_proxy.py`（`recent_usage` :5607-5637 /
//! `_fold_cost` :3089-3103）。
//!
//! 断言清单：
//! 1. **读最后一行** ⇒ 得到 `Pricing`（Rust 侧每轮追加整份快照一行 ⇒ 末行最新）。
//! 2. 文件缺失 / 空 / 全是坏行 ⇒ `None`；**不 panic**。
//! 3. 末行坏掉 ⇒ **安全降级**到上一份好快照。
//! 4. `compute_row` / `sum_rows` 算出的成本与**手算值**一致。
//! 5. **定价表缺失 ⇒ 聚合结果不含成本键**（不是 0、不是空表）。
//! 6. 有定价表 ⇒ 快照带 `cost_cny` / `cost_missing`，最近行带 `cost_*` 键组。
//! 7. 未定价的行 ⇒ **整组 `cost_*` 键省略**（不是 0 / null）。
//!
//! ⚠️ 策略库的行是**整份快照**（Rust 侧格式），不是 Python 的「一个价格一行」。
//! 全程只读本地临时目录，**不发任何真实上游请求**。

use serde_json::{json, Value};

use wb_tasks::price::{compute_row, sum_rows, Pricing};
use wb_tasks::pricing_store::{load_pricing, load_pricing_from_rows, policies_path};
use wb_tasks::usage::{
    build_usage_recent_priced, build_usage_snapshot_priced, build_usage_snapshot_with_priced,
    SnapshotDecor,
};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/// 一份**整份快照**（Rust 侧 `SnapshotBuild.doc` 的形状）。
fn snapshot_doc(output_per_million: f64) -> Value {
    json!({
        "meta": {
            "generated_at": "2026-10-22 08:00:00",
            "base_currency": "CNY",
            "usd_cny": 7.1,
            "sources": {"openrouter": "https://openrouter.ai/api/v1/models"},
        },
        "models": {
            "demo-model": {
                "display": "demo-model",
                "source": "openrouter",
                "currency": "USD",
                "unit": 1000000,
                "or_id": "vendor/demo-model",
                "flat": {
                    "input_cache_hit": 0.01,
                    "input_cache_miss": 0.03,
                    "output": output_per_million
                }
            }
        }
    })
}

/// 往策略库里追加一行（Rust 侧的写法：每轮一整份快照一行）。
fn append_line(dir: &std::path::Path, line: &str) {
    use std::io::Write;
    let path = policies_path(dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
}

/// 一条用量行。
fn usage_row(model: &str) -> Value {
    json!({
        "at": 1_800_000_000.0,
        "iso": "2026-10-22 10:00:00",
        "model": model,
        "outcome": "completed",
        "prompt_tokens": 1_000_000,
        "completion_tokens": 1_000_000,
        "reasoning_tokens": 0,
        "cached_tokens": 0,
        "total_tokens": 2_000_000,
        "account": "a1",
        "realm": "cn",
    })
}

// ---------------------------------------------------------------------------
// 1. 读取端
// ---------------------------------------------------------------------------

#[test]
fn reads_the_last_snapshot_line() {
    let dir = tempfile::TempDir::new().unwrap();
    append_line(dir.path(), &snapshot_doc(0.5).to_string());
    append_line(dir.path(), &snapshot_doc(0.9).to_string());

    let pricing = load_pricing(dir.path()).expect("策略库应当读得出定价表");
    let entry = pricing
        .models
        .get("demo-model")
        .expect("demo-model 应当有价");
    // 末行（0.9）胜出。
    assert_eq!(entry.flat.output, Some(0.9));
    assert_eq!(entry.flat.input_cache_miss, Some(0.03));
    assert_eq!(entry.or_id, "vendor/demo-model");
    assert_eq!(entry.unit, 1_000_000.0);
    assert_eq!(entry.source, "openrouter");
    assert_eq!(pricing.base_currency, "CNY");
    assert!((pricing.usd_cny() - 7.1).abs() < 1e-9);
}

#[test]
fn a_missing_file_is_none_not_an_error() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(load_pricing(dir.path()).is_none());
}

#[test]
fn an_empty_file_is_none() {
    let dir = tempfile::TempDir::new().unwrap();
    append_line(dir.path(), "");
    assert!(load_pricing(dir.path()).is_none());
}

#[test]
fn a_file_of_broken_lines_is_none() {
    let dir = tempfile::TempDir::new().unwrap();
    append_line(dir.path(), "{not json");
    append_line(dir.path(), "[1, 2, 3]");
    append_line(dir.path(), "{\"models\": 42}");
    assert!(load_pricing(dir.path()).is_none());
}

#[test]
fn a_broken_last_line_falls_back_to_the_previous_snapshot() {
    let dir = tempfile::TempDir::new().unwrap();
    append_line(dir.path(), &snapshot_doc(0.5).to_string());
    append_line(dir.path(), "{truncated");
    let pricing = load_pricing(dir.path()).expect("应当退到上一份好快照");
    assert_eq!(
        pricing.models.get("demo-model").unwrap().flat.output,
        Some(0.5)
    );
}

#[test]
fn the_pure_helper_takes_the_last_usable_row() {
    let rows = vec![snapshot_doc(0.5), json!({"nope": 1}), snapshot_doc(0.9)];
    let pricing = load_pricing_from_rows(&rows).expect("应当取到最后一份好快照");
    assert_eq!(
        pricing.models.get("demo-model").unwrap().flat.output,
        Some(0.9)
    );
    assert!(load_pricing_from_rows(&[]).is_none());
}

// ---------------------------------------------------------------------------
// 2. 计价与手算值一致
// ---------------------------------------------------------------------------

#[test]
fn compute_row_matches_the_hand_calculation() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let row = usage_row("demo-model");
    let cost = compute_row(&row, &pricing);
    assert!(cost.known);
    assert_eq!(cost.band, None);
    // (1e6 * 0.03 + 1e6 * 0.5) / 1e6 * 7.1 = 0.53 * 7.1 = 3.763
    assert!((cost.cny - 3.763).abs() < 1e-9, "cny was {}", cost.cny);
}

#[test]
fn cached_tokens_are_billed_at_the_hit_rate() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let mut row = usage_row("demo-model");
    row["cached_tokens"] = json!(1_000_000);
    row["completion_tokens"] = json!(0);
    let cost = compute_row(&row, &pricing);
    // (0 * 0.03 + 1e6 * 0.01) / 1e6 * 7.1 = 0.071
    assert!((cost.cny - 0.071).abs() < 1e-9, "cny was {}", cost.cny);
}

#[test]
fn sum_rows_counts_priced_and_missing() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let rows = vec![
        usage_row("demo-model"),
        usage_row("who-knows"),
        usage_row("who-knows"),
    ];
    let totals = sum_rows(&rows, &pricing);
    assert_eq!(totals.priced, 1);
    assert_eq!(totals.missing.get("who-knows"), Some(&2));
    assert!((totals.cny - 3.763).abs() < 1e-9, "cny was {}", totals.cny);
}

// ---------------------------------------------------------------------------
// 3. 聚合：没有定价表 ⇒ 没有任何成本键
// ---------------------------------------------------------------------------

#[test]
fn without_a_pricing_table_the_snapshot_has_no_cost_keys() {
    let rows = vec![usage_row("demo-model")];
    let snap = build_usage_snapshot_priced(&rows, None);
    assert!(
        snap.get("cost_cny").is_none(),
        "没有定价表就不该写 cost_cny（不是 0）"
    );
    assert!(
        snap.get("cost_missing").is_none(),
        "没有定价表就不该写 cost_missing（不是空表）"
    );
    assert!(
        snap["by_model"]["demo-model"].get("cost_cny").is_none(),
        "没有定价表就不该写模型桶的 cost_cny"
    );
    // 其它键一点没变。
    assert_eq!(snap["requests"], 1);
    assert_eq!(snap["total_tokens"], 2_000_000);
}

#[test]
fn without_a_pricing_table_the_recent_rows_have_no_cost_keys() {
    let rows = vec![usage_row("demo-model")];
    let out = build_usage_recent_priced(&rows, "cn", 1, 10, None);
    assert!(out.get("usd_cny").is_none());
    let row = &out["rows"][0];
    for key in [
        "cost_cny",
        "cost_band",
        "cost_source",
        "cost_rates",
        "cost_unit",
        "cost_currency",
        "cost_usd_cny",
        "cost_or_id",
        "cost_via",
        "cost_inherited_from",
        "cost_override_from",
        "cost_band_note",
        "cost_via_derived",
        "cost_backfilled",
    ] {
        assert!(row.get(key).is_none(), "不该有 {key}");
    }
}

#[test]
fn an_unpriced_model_omits_the_whole_cost_group() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let rows = vec![usage_row("who-knows")];
    let out = build_usage_recent_priced(&rows, "cn", 1, 10, Some(&pricing));
    let row = &out["rows"][0];
    assert!(row.get("cost_cny").is_none(), "未定价就不该写 cost_cny");
    assert!(row.get("cost_source").is_none());
    assert!(row.get("cost_rates").is_none());
    // 未定价的行本身原样保留。
    assert_eq!(row["model"], "who-knows");
    assert_eq!(row["total_tokens"], 2_000_000);
}

// ---------------------------------------------------------------------------
// 4. 聚合：有定价表 ⇒ 成本键齐全
// ---------------------------------------------------------------------------

#[test]
fn with_a_pricing_table_the_snapshot_carries_the_cost_keys() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let rows = vec![usage_row("demo-model"), usage_row("who-knows")];
    let snap = build_usage_snapshot_priced(&rows, Some(&pricing));

    // 1 行可定价 + 1 行未定价。
    assert!((snap["cost_cny"].as_f64().unwrap() - 3.763).abs() < 1e-9);
    assert_eq!(snap["cost_missing"]["who-knows"], 1);
    assert!(snap["cost_missing"].get("demo-model").is_none());
    assert!((snap["by_model"]["demo-model"]["cost_cny"].as_f64().unwrap() - 3.763).abs() < 1e-9);
    // 未定价的模型桶的 cost_cny 是 0（Python 同款：桶里恒有这个键）。
    assert_eq!(
        snap["by_model"]["who-knows"]["cost_cny"].as_f64().unwrap(),
        0.0
    );
}

#[test]
fn the_decorated_snapshot_carries_usd_cny() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let decor = SnapshotDecor {
        realm: "cn".to_string(),
        log_file: "/tmp/usage/usage.jsonl".to_string(),
        accounts_map: None,
        account: None,
    };
    let rows = vec![usage_row("demo-model")];
    let out = build_usage_snapshot_with_priced(&rows, &decor, Some(&pricing));
    assert!((out["usd_cny"].as_f64().unwrap() - 7.1).abs() < 1e-9);
    // 键序：装饰组在基底之后，`usd_cny` 夹在 `realm` 与 `accounts_map` 之间
    // （Python `_usage_snapshot_uncached` 的顺序）。
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    let realm_at = keys.iter().position(|k| *k == "realm").unwrap();
    assert_eq!(keys[realm_at + 1], "usd_cny");

    // 没有定价表 ⇒ 不写 usd_cny。
    let out = build_usage_snapshot_with_priced(&rows, &decor, None);
    assert!(out.get("usd_cny").is_none());
}

#[test]
fn the_recent_row_carries_the_cost_detail_keys() {
    let pricing = Pricing::from_json(&snapshot_doc(0.5));
    let rows = vec![usage_row("demo-model")];
    let out = build_usage_recent_priced(&rows, "cn", 1, 10, Some(&pricing));
    assert!((out["usd_cny"].as_f64().unwrap() - 7.1).abs() < 1e-9);

    let row = &out["rows"][0];
    assert!((row["cost_cny"].as_f64().unwrap() - 3.763).abs() < 1e-6);
    assert_eq!(row["cost_source"], "openrouter");
    assert_eq!(row["cost_currency"], "USD");
    assert_eq!(row["cost_unit"], 1_000_000.0);
    assert_eq!(row["cost_or_id"], "vendor/demo-model");
    assert_eq!(row["cost_via"], "direct");
    assert_eq!(row["cost_backfilled"], false);
    assert_eq!(row["cost_via_derived"], false);
    assert!((row["cost_usd_cny"].as_f64().unwrap() - 7.1).abs() < 1e-9);
    assert_eq!(row["cost_rates"]["output"], 0.5);
    assert_eq!(row["cost_rates"]["input_cache_miss"], 0.03);
    assert_eq!(row["cost_rates"]["input_cache_hit"], 0.01);
    // 无档位 ⇒ 不写 cost_band / cost_band_note。
    assert!(row.get("cost_band").is_none());
    assert!(row.get("cost_band_note").is_none());
    // 策略层的两个键 Rust 侧算不出来 ⇒ 省略（不写 null）。
    assert!(row.get("cost_source_at").is_none());
    assert!(row.get("cost_policy").is_none());
}
