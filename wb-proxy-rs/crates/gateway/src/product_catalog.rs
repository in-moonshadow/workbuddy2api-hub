//! 关卡 4.21（线 C）：**远端产品目录**（HTTP 那一半）。
//!
//! 权威源：Python `wb_proxy.py`
//!
//! - `fetch_remote_product_config()`（`:10431-10467`）：GET
//!   `<chat_upstream>/v3/config`，**最多两次尝试**（桌面 chat UA，再计费 UA），
//!   单次超时 **10s**；任何一步失败 ⇒ 返回 `None`（调用方回落）。
//! - `parse_remote_catalog()`（`:10332-10373`）：把应答解析成 `(ids, meta)`——
//!   id 列表来自 `agents[].models`（**裸字符串数组**），每个模型的元信息来自
//!   文档里任意 `models`（**对象数组**）。空目录 ⇒ `None`（凭据不可用时上游
//!   会回 HTTP 200 + 空列表）。
//! - `remote_config_headers()`（`:10278-10293`）：`Accept` / `User-Agent` /
//!   `Origin` / `Referer` / `Authorization: Bearer <token>` / `X-User-Id`。
//!
//! ## 边界（与 `wb_gateway::pricing` 一致）
//!
//! 本模块只负责**把远端目录取回来并解析成 (ids, credits)**；**筛选规则**
//! （`is_chat_model` / 变体后缀 / 免费兄弟）是取价语义，放在
//! `wb_tasks::live_catalog`——那一边才有静态目录的 credits 表。
//!
//! ## 端点可注入
//!
//! [`fetch_product_catalog`] 的 `endpoint` 是参数：生产走
//! [`product_config_url`]，契约测试走 wiremock 假上游。
//!
//! ⛔ 契约测试一律走 wiremock mock 上游，**绝不向真实上游发请求**。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::endpoints::{chat_base_url, realm_config, PRODUCT_DESKTOP};
use crate::outbound::{describe_error, UpstreamClient};

/// 桌面端自己的产品配置端点。Python `wb_proxy.py:10269` 的 `REMOTE_CONFIG_PATH`。
pub const REMOTE_CONFIG_PATH: &str = "/v3/config";

/// 单次请求超时（秒）。Python `wb_proxy.py:10457` 的 `urlopen(req, timeout=10)`。
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 10.0;

/// 错误文案里最多保留多少字符的响应体（上游可能回很大的错误页）。
const MAX_ERROR_BODY_CHARS: usize = 200;

/// 远端产品目录的解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductCatalog {
    /// 模型 id 列表（去重保序，取自 `agents[].models`）。
    pub ids: Vec<String>,
    /// id → credits（小写），取自文档里的 `models` 对象数组。
    ///
    /// 取价侧用它回答「这个模型是不是免费的」——远端 meta 里的 credits
    /// **覆盖**静态目录的 credits（对齐 Python `curate_remote_catalog`
    /// 的 `credits[mid] = ...` 赋值语义）。
    pub credits: BTreeMap<String, String>,
}

/// 调用 `/v3/config` 所需的凭据与身份。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogAuth {
    /// 账号的 access_token（进 `Authorization: Bearer`）。
    pub access_token: String,
    /// 账号 uid（进 `X-User-Id`）。
    pub user_id: String,
}

/// 该 realm 是否支持拉远端产品目录。
///
/// Python `fetch_remote_product_config()` 的 `if realm not in ("intl", "cn"):
/// return None`——远端目录是**按区域**的，不存在「两个区域合并」的抓取。
pub fn is_supported_realm(realm: &str) -> bool {
    realm == "intl" || realm == "cn"
}

/// 产品配置端点地址。Python `cfg["chat_upstream"] + REMOTE_CONFIG_PATH`。
pub fn product_config_url(realm: &str) -> String {
    format!(
        "{}{}",
        chat_base_url(realm, PRODUCT_DESKTOP),
        REMOTE_CONFIG_PATH
    )
}

/// 依次尝试的 User-Agent。Python `wb_proxy.py:10449-10451`：
/// 先桌面 chat UA；若计费 UA 与之不同，再试一次计费 UA。
///
/// 「UA 决定回哪份目录」——未知的 UA 是硬 400（code 12403），所以只用
/// realm 的固定桌面身分，不跟随账号当前身份。
pub fn catalog_user_agents(realm: &str) -> Vec<&'static str> {
    let cfg = realm_config(realm);
    let mut uas = vec![cfg.chat_ua];
    if cfg.billing_ua != cfg.chat_ua {
        uas.push(cfg.billing_ua);
    }
    uas
}

/// 请求头。Python `remote_config_headers()`（`:10278-10293`）。
pub fn product_config_headers(
    realm: &str,
    ua: &str,
    auth: &CatalogAuth,
) -> HashMap<String, String> {
    let cfg = realm_config(realm);
    let mut headers = HashMap::new();
    headers.insert(
        "Accept".to_string(),
        "application/json, text/plain, */*".to_string(),
    );
    headers.insert("User-Agent".to_string(), ua.to_string());
    headers.insert("Origin".to_string(), cfg.origin.to_string());
    headers.insert("Referer".to_string(), format!("{}/", cfg.origin));
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {}", auth.access_token),
    );
    headers.insert("X-User-Id".to_string(), auth.user_id.clone());
    headers
}

/// 抓一份远端产品目录（**只要 id 列表**）。
///
/// `endpoint` 是参数 ⇒ 契约测试可指向 wiremock 假上游；生产用
/// [`product_config_url`]。`timeout_secs` 是这次请求的总时限。
///
/// 失败返回 `Err`（文案可读、可分类）；调用方按 Python 的口径**回落**，
/// 而不是把失败当成「这个区域没有模型」。
pub async fn fetch_product_catalog(
    client: &UpstreamClient,
    endpoint: &str,
    realm: &str,
    auth: &CatalogAuth,
    timeout_secs: f64,
) -> Result<Vec<String>, String> {
    let catalog =
        fetch_product_catalog_with_credits(client, endpoint, realm, auth, timeout_secs).await?;
    Ok(catalog.ids)
}

/// 抓一份远端产品目录（**id + credits**）。
///
/// 语义与 [`fetch_product_catalog`] 相同，只是多带一份 credits 表（取价侧的
/// 「免费兄弟」规则要用）。
pub async fn fetch_product_catalog_with_credits(
    client: &UpstreamClient,
    endpoint: &str,
    realm: &str,
    auth: &CatalogAuth,
    timeout_secs: f64,
) -> Result<ProductCatalog, String> {
    if !is_supported_realm(realm) {
        return Err(format!("realm {realm} 不支持拉远端产品目录"));
    }
    let timeout = Duration::from_secs_f64(sane_timeout(timeout_secs));
    let mut last = String::new();
    for ua in catalog_user_agents(realm) {
        let headers = product_config_headers(realm, ua, auth);
        let response = match client.get_with_timeout(endpoint, &headers, timeout).await {
            Ok(response) => response,
            Err(err) => {
                // 网络失败（WAF 丢连接是常态）⇒ 换下一个 UA 再试。
                last = describe_error(&err);
                continue;
            }
        };
        let status = response.status();
        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                last = format!("读取响应失败: {err}");
                continue;
            }
        };
        if !status.is_success() {
            // Python 的 `urlopen` 在非 2xx 时抛 HTTPError ⇒ 同样换 UA 重试。
            last = format!(
                "产品配置端点返回 {}: {}",
                status.as_u16(),
                truncate(&body, MAX_ERROR_BODY_CHARS)
            );
            continue;
        }
        let payload: Value = match serde_json::from_str(&body) {
            Ok(payload) => payload,
            Err(err) => {
                last = format!("响应不是合法 JSON: {err}");
                continue;
            }
        };
        match parse_remote_catalog(&payload) {
            Some(catalog) => return Ok(catalog),
            // 空目录（凭据不可用时上游回 200 + 空列表）⇒ 换 UA 再试。
            None => {
                last = "远端目录为空".to_string();
                continue;
            }
        }
    }
    Err(last)
}

/// 解析 `/v3/config` 的应答。Python `parse_remote_catalog()`（`:10332-10373`）。
///
/// 返回 `None` 表示「这份应答没有可用目录」——空目录也走这条路（调用方据此
/// 回落，而不是发布「这个区域没有模型」）。
pub fn parse_remote_catalog(payload: &Value) -> Option<ProductCatalog> {
    let (cli, other) = agent_model_lists(payload);
    let pool = if !cli.is_empty() { cli } else { other };
    // Python `max(pool, key=len)`：**并列时取第一个**（`max` 返回首个最大值），
    // 所以这里用「严格大于」比较，而不是 `Iterator::max_by_key`（后者取最后一个）。
    let mut best: Option<&Vec<String>> = None;
    for list in pool.iter() {
        match best {
            Some(current) if current.len() >= list.len() => {}
            _ => best = Some(list),
        }
    }
    let best = best?;
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for mid in best.iter() {
        if seen.insert(mid.clone()) {
            ids.push(mid.clone());
        }
    }
    if ids.is_empty() {
        return None;
    }

    let mut meta: BTreeMap<String, Value> = BTreeMap::new();
    walk_meta(payload, &mut meta);
    let mut credits = BTreeMap::new();
    for (mid, item) in meta.iter() {
        credits.insert(mid.clone(), python_credits(item));
    }
    Some(ProductCatalog { ids, credits })
}

/// 每个 agent 的裸 id 列表。Python `_agent_model_lists()`（`:10296-10329`）。
///
/// 返回 `(cli 命名 agent 的列表, 其它 agent 的列表)`——CLI 那份优先。
fn agent_model_lists(payload: &Value) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    // 端点的应答把目录包在 `data` 里，桌面缓存文件是同一份文档但没有这层信封。
    let mut roots: Vec<&Value> = vec![payload];
    if let Some(data @ Value::Object(_)) = payload.get("data") {
        roots.push(data);
    }

    let mut cli: Vec<Vec<String>> = Vec::new();
    let mut other: Vec<Vec<String>> = Vec::new();
    for root in roots {
        let Some(agents) = root.get("agents") else {
            continue;
        };
        let entries: Vec<(String, &Value)> = match agents {
            Value::Object(map) => map
                .iter()
                .map(|(name, entry)| (name.clone(), entry))
                .collect(),
            Value::Array(list) => list
                .iter()
                .map(|entry| {
                    let name = entry
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string();
                    (name, entry)
                })
                .collect(),
            _ => continue,
        };
        for (name, entry) in entries {
            let Some(Value::Array(models)) = entry.get("models") else {
                continue;
            };
            // Python 要求 `isinstance(models[0], str)`——**首个元素是字符串**
            // 整段才作数（元信息数组是对象，会被这里排除）。
            if !matches!(models.first(), Some(Value::String(_))) {
                continue;
            }
            let ids: Vec<String> = models
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect();
            if ids.is_empty() {
                continue;
            }
            if name.trim().to_lowercase() == "cli" {
                cli.push(ids);
            } else {
                other.push(ids);
            }
        }
    }
    (cli, other)
}

/// 递归找出文档里所有 `models`（对象数组）里的模型元信息。
///
/// Python `parse_remote_catalog()` 里的 `walk()`（`:10357-10370`）：遇到
/// `models` 数组且**首元素是带 id 的对象**时，整段按 `meta.setdefault` 收
/// （**先到先得**），然后继续递归。
fn walk_meta(node: &Value, meta: &mut BTreeMap<String, Value>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Array(models)) = map.get("models") {
                if let Some(first) = models.first() {
                    if first.is_object() && model_id(first).is_some() {
                        for item in models {
                            if !item.is_object() {
                                continue;
                            }
                            let Some(mid) = model_id(item) else {
                                continue;
                            };
                            meta.entry(mid).or_insert_with(|| item.clone());
                        }
                    }
                }
            }
            for value in map.values() {
                walk_meta(value, meta);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_meta(item, meta);
            }
        }
        _ => {}
    }
}

/// 模型条目的 id：Python `str(item.get("id") or "").strip()`（非空才返回）。
fn model_id(item: &Value) -> Option<String> {
    let value = item.get("id")?;
    let raw = scalar_text(value)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 把一个 JSON 标量转成 Python 的 `str(x or "")` 口径（假值 ⇒ 空串）。
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => {
            // Python 的 `0 or ""` ⇒ 空串。
            if number.as_f64() == Some(0.0) {
                Some(String::new())
            } else {
                Some(number.to_string())
            }
        }
        Value::Bool(flag) => Some(if *flag { "True" } else { "" }.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

/// `str(item.get("credits") or "").strip().lower()`。
fn python_credits(item: &Value) -> String {
    let Some(object) = item.as_object() else {
        return String::new();
    };
    let raw = match object.get("credits") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => {
            if number.as_f64() == Some(0.0) {
                String::new()
            } else {
                number.to_string()
            }
        }
        _ => String::new(),
    };
    raw.trim().to_lowercase()
}

/// 把超时夹成一个 `Duration::from_secs_f64` 能接受的值。
fn sane_timeout(timeout_secs: f64) -> f64 {
    if timeout_secs.is_finite() && timeout_secs > 0.0 {
        timeout_secs
    } else {
        DEFAULT_TIMEOUT_SECONDS
    }
}

/// 截断一段文本（按字符，不切碎多字节字符）。
fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max_chars).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn auth() -> CatalogAuth {
        CatalogAuth {
            access_token: "tok-123".to_string(),
            user_id: "uid-9".to_string(),
        }
    }

    /// 一份最小的 `/v3/config` 应答：CLI agent 的裸 id 列表 + 一份元信息数组。
    fn config_payload() -> Value {
        json!({
            "data": {
                "agents": {
                    "cli": {"models": ["alpha", "beta"]},
                    "other": {"models": ["gamma"]}
                }
            },
            "models": [
                {"id": "alpha", "credits": "x1.00"},
                {"id": "beta", "credits": "x0.00"}
            ]
        })
    }

    #[test]
    fn parses_ids_from_the_cli_agent_and_credits_from_the_models_array() {
        let catalog = parse_remote_catalog(&config_payload()).expect("必须解析出目录");
        assert_eq!(catalog.ids, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(
            catalog.credits.get("alpha").map(String::as_str),
            Some("x1.00")
        );
        assert_eq!(
            catalog.credits.get("beta").map(String::as_str),
            Some("x0.00")
        );
    }

    #[test]
    fn cli_agent_wins_over_other_agents() {
        let payload = json!({
            "agents": {
                "cli": {"models": ["a"]},
                "other": {"models": ["b", "c", "d"]}
            }
        });
        let catalog = parse_remote_catalog(&payload).expect("必须解析出目录");
        assert_eq!(catalog.ids, vec!["a".to_string()]);
    }

    #[test]
    fn the_longest_agent_list_wins_when_no_cli_agent_exists() {
        let payload = json!({
            "agents": {
                "short": {"models": ["a"]},
                "long": {"models": ["b", "c", "d"]}
            }
        });
        let catalog = parse_remote_catalog(&payload).expect("必须解析出目录");
        assert_eq!(
            catalog.ids,
            vec!["b".to_string(), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn an_empty_catalogue_is_reported_as_none() {
        // 凭据不可用时上游回 HTTP 200 + 空列表 ⇒ 回落，不是「这个区域没模型」。
        let payload = json!({"data": {"agents": {"cli": {"models": []}}}});
        assert!(parse_remote_catalog(&payload).is_none());
        assert!(parse_remote_catalog(&json!({})).is_none());
        assert!(parse_remote_catalog(&json!("not an object")).is_none());
    }

    #[test]
    fn urls_and_headers_follow_the_python_realm_config() {
        assert_eq!(
            product_config_url("intl"),
            "https://www.workbuddy.ai/v3/config"
        );
        assert_eq!(
            product_config_url("cn"),
            "https://copilot.tencent.com/v3/config"
        );

        let headers = product_config_headers("cn", "UA/1.0", &auth());
        assert_eq!(headers["Accept"], "application/json, text/plain, */*");
        assert_eq!(headers["User-Agent"], "UA/1.0");
        assert_eq!(headers["Origin"], "https://www.codebuddy.cn");
        assert_eq!(headers["Referer"], "https://www.codebuddy.cn/");
        assert_eq!(headers["Authorization"], "Bearer tok-123");
        assert_eq!(headers["X-User-Id"], "uid-9");
    }

    #[test]
    fn user_agents_are_the_desktop_one_then_the_billing_one() {
        let uas = catalog_user_agents("intl");
        assert_eq!(uas.len(), 2);
        assert_eq!(uas[0], "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2");
        assert_eq!(uas[1], "WorkBuddy/5.5.2");
    }

    #[tokio::test]
    async fn fetch_reads_the_ids_from_a_mock_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/config"))
            .and(header("authorization", "Bearer tok-123"))
            .and(header("x-user-id", "uid-9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(config_payload()))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let ids = fetch_product_catalog(
            &client,
            &format!("{}/v3/config", server.uri()),
            "intl",
            &auth(),
            5.0,
        )
        .await
        .expect("mock 上游必须成功");
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn fetch_reports_upstream_failure_instead_of_panicking() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = UpstreamClient::new();
        let err = fetch_product_catalog(
            &client,
            &format!("{}/v3/config", server.uri()),
            "intl",
            &auth(),
            5.0,
        )
        .await
        .expect_err("500 必须报错");
        assert!(err.contains("500"), "错误文案要带状态码: {err}");
    }

    #[tokio::test]
    async fn an_unsupported_realm_is_rejected() {
        let client = UpstreamClient::new();
        let err = fetch_product_catalog(
            &client,
            "https://example.invalid/v3/config",
            "all",
            &auth(),
            5.0,
        )
        .await
        .expect_err("未知 realm 必须报错");
        assert!(err.contains("all"), "错误文案要带上 realm: {err}");
    }

    #[test]
    fn illegal_timeout_falls_back_to_the_default() {
        assert_eq!(sane_timeout(0.0), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(sane_timeout(-1.0), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(sane_timeout(f64::NAN), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(sane_timeout(12.5), 12.5);
    }
}
