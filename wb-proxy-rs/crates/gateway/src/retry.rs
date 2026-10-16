//! 重试循环（T4d），移植自 wb_proxy.py:12506-12820 的 open_upstream 主体。
//!
//! 核心语义：
//! - 预算 `max(2, total) + 1 + (4 若 auto_switch)`，total = max(1, count_ready)
//! - 每轮 pick_for_session 挑账号；瞬时失败退避后 `tried.clear()` 整池重轮
//! - 429/401/402/403/5xx 各走分类冷却（notes.rs 记账 + affinity demote）
//! - 全部失败才交给调用方：429 → rate_limit 信封（retry_after），403 内容审核
//!   透传，其余 HTTPError 透传 `upstream NNN: <body 前 600 字节>`；
//!   兜底 `no usable account` → 503
//!
//! M3 范围裁剪（注释内标 M5/M4）：note_limit_event 用量打卡、_save_account_caps
//! 写凭证档、daily/credit/model_token 三个 limit_blocked 尾部兜底（依赖面板设置
//! 数据）、prompt_cache_key 注入（面板开关）均不在本任务；开关位以参数注入。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::body::{build_upstream_body, BodyOptions};
use crate::egress::{egress_key, EgressState};
use crate::endpoints::{chat_base_url, PRODUCT_CLI, PRODUCT_DESKTOP, PRODUCT_VSCODE};
use crate::headers::{chat_request_headers, ChatHeadersParams};
use crate::notes;
use crate::outbound::UpstreamClient;
use crate::pool::{AccountPool, AccountState};
use crate::prompt::DegradeGate;
use crate::upstream::{
    credential_scope_phrase, openai_error, openai_rate_limited, parse_rate_limit_reset,
    rate_limit_is_account_level, rate_limit_is_cap, rate_limit_is_soft_shape,
    restriction_is_account_level,
};

/// CHAT_PATH（wb_proxy.py:238）。
pub const CHAT_PATH: &str = "/v2/chat/completions";
/// MAX_PRODUCT_SWITCHES（wb_proxy.py:12095）：每轮最多切 4 次身分。
pub const MAX_PRODUCT_SWITCHES: i64 = 4;
/// EGRESS_SLEEP_MAX（wb_proxy.py:12101）：单次请求里出口退避最多睡 5s。
pub const EGRESS_SLEEP_MAX: f64 = 5.0;
/// RESTRICTED_403_COOLDOWN（wb_proxy.py:12320）：风控限制账号冷却 7200s。
pub const RESTRICTED_403_COOLDOWN: f64 = 7200.0;

/// 重试循环的最终结果。
pub enum Outcome {
    /// 上游 2xx：把原始响应与命中账号索引交回调用方（流式读由调用方做）。
    Served(reqwest::Response, usize),
    /// 已分类的错误信封（status + OpenAI 错误 JSON）。
    Rejected { status: u16, body: Value },
}

/// 上游失败分类（循环内记账与兜底信封用）。
enum Failure {
    /// HTTPError（body 已按上限读取）。
    Http { status: u16, detail: String },
    /// 传输层异常（reqwest 错误串；兜底 502 `upstream unreachable: <exc>`）。
    Transport(String),
}

/// open_upstream 入参。生命周期借用方；attempt_body 每轮由 opts 重建。
pub struct RetryOptions<'a> {
    pub realm: &'a str,
    pub model: Option<&'a str>,
    pub session_key: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    /// 下游原始请求体（403 降级时 rebuild 用）。
    pub payload: &'a Value,
    /// body 管线选项（degraded 在降级分支翻转）。
    pub opts: BodyOptions<'a>,
    /// 上游 base URL 覆盖：None → endpoints 表（realm+product）；测试注入
    /// wiremock 地址。M3 契约测试统一走此注入。
    pub base_url: Option<&'a str>,
    /// 面板开关：auto_switch（默认关）→ 预算 +4 并启用身分旋转。
    pub auto_switch: bool,
    /// 面板开关：egress_backoff（默认关）→ 502/503/504 与传输抖动计出口退避。
    pub egress_backoff: bool,
    /// prompt mode（"passthrough"/"append" 才允许 403 降级重试）。
    pub prompt_mode: &'a str,
    pub now: f64,
    /// 本地 04:00 计算用的时区偏移秒（本地时区 - UTC）。
    pub utc_offset: i64,
}

/// 上游瞬时抖动文本标记（wb_proxy.py:12190-12199 的 markers 逐字）。
const TRANSIENT_MARKERS: [&str; 16] = [
    "ssl",
    "unexpected_eof",
    "eof occurred",
    "remote end closed",
    "connection reset",
    "connection aborted",
    "connectionreseterror",
    "connectionabortederror",
    "timed out",
    "timeout",
    "temporarily unavailable",
    "bad gateway",
    "502",
    "503",
    "504",
    "incompleteread",
];

/// is_transient 的文本判定（wb_proxy.py:12190-12207）：`"%s %s" % (type, exc)`
/// 小写后子串匹配 markers。抽成纯函数便于对文本断言；网络错误由调用方先
/// 走 reqwest 分类（is_timeout）再落入文本匹配。
pub fn is_transient_text(text: &str) -> bool {
    let t = text.to_lowercase();
    TRANSIENT_MARKERS.iter().any(|m| t.contains(m))
}

/// reqwest 传输错误 → 是否瞬时（值得重试而非冷却账号）。
fn is_transient_err(e: &reqwest::Error) -> bool {
    if e.is_timeout() {
        return true;
    }
    is_transient_text(&e.to_string())
}

/// 跨请求身分切换计数（wb_proxy.py:12100, 12106-12119）：60s 内的切换算一轮。
/// 模块级（Python `_SWITCH_LOG` 同语义，单进程共享）。
type SwitchLog = LazyLock<Mutex<HashMap<(String, String), (i64, f64)>>>;
static SWITCH_LOG: SwitchLog = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 这一轮已经切过几次（60s 内同 (uid, model) 记同一轮）。
fn switch_count(uid: &str, model: &str, now: f64) -> i64 {
    let log = SWITCH_LOG.lock().unwrap();
    match log.get(&(uid.into(), model.into())) {
        Some((c, last)) if now - *last <= 60.0 => *c,
        _ => 0,
    }
}

/// reset_switch_counter（wb_proxy.py:12154-12157）：成功后归零，下一请求重新
/// 享有 4 次切换额度。
pub fn reset_switch_counter(uid: &str, model: &str) {
    SWITCH_LOG
        .lock()
        .unwrap()
        .remove(&(uid.into(), model.into()));
}

/// _try_switch_product（wb_proxy.py:12122-12152）：429 时换身分重试。
/// 旋转 deskbuddy → vscode → cli；>500 条目清空；set_product 内已做
/// keep_caps 清理。返回 True 表示已切换。
fn try_switch_product(account: &mut AccountState, model: &str, now: f64) -> bool {
    let count = switch_count(&account.uid, model, now);
    if count >= MAX_PRODUCT_SWITCHES {
        return false;
    }
    let target = match account.product.as_str() {
        PRODUCT_DESKTOP => PRODUCT_VSCODE,
        PRODUCT_VSCODE => PRODUCT_CLI,
        _ => PRODUCT_DESKTOP,
    };
    if !account.set_product(target, now) {
        return false;
    }
    let mut log = SWITCH_LOG.lock().unwrap();
    log.insert((account.uid.clone(), model.into()), (count + 1, now));
    if log.len() > 500 {
        log.clear();
    }
    true
}

/// retry_after_seconds（wb_proxy.py:12158-12166）：该 realm 下任一可用账号
/// 能再服务 `model` 的最短等待。池空 / 全无冷却 → 60。
pub fn retry_after_seconds(pool: &AccountPool, realm: &str, model: Option<&str>, now: f64) -> i64 {
    if pool.is_empty() {
        return 60;
    }
    let mut active: Vec<f64> = Vec::new();
    for a in pool.iter_accounts() {
        if a.realm != realm || !a.enabled || !a.has_token {
            continue;
        }
        let w = a.throttle_wait(model, now);
        if w > 0.0 {
            active.push(w);
        }
    }
    match active.into_iter().min_by(|a, b| a.total_cmp(b)) {
        Some(m) => m as i64,
        None => 60,
    }
}

/// realm_model_throttled（wb_proxy.py:12168-12188）：存在可用账号且**全部**
/// 都在冷却该模型 → (True, 最短等待)。只认模型冷却——账号级冷却（常见于
/// 瞬时网络错误）不算「限流」，避免让客户端对一个从未被限流的模型退避。
pub fn realm_model_throttled(
    pool: &AccountPool,
    realm: &str,
    model: Option<&str>,
    now: f64,
) -> (bool, i64) {
    if pool.is_empty() {
        return (false, 0);
    }
    let existing: Vec<&AccountState> = pool
        .iter_accounts()
        .filter(|a| a.realm == realm && a.enabled && a.has_token)
        .collect();
    if existing.is_empty() {
        return (false, 0);
    }
    let waits: Vec<f64> = existing
        .iter()
        .map(|a| {
            model
                .and_then(|m| a.model_cooldowns.get(m).copied())
                .map(|until| (until - now).max(0.0))
                .unwrap_or(0.0)
        })
        .collect();
    if waits.iter().all(|w| *w > 0.0) {
        let m = waits
            .into_iter()
            .min_by(|a, b| a.total_cmp(b))
            .unwrap_or(0.0);
        return (true, m as i64);
    }
    (false, 0)
}

/// pool_unavailable_detail（wb_proxy.py:12790-12820）：逐账号拼「八位 UID + 原因」。
pub fn pool_unavailable_detail(
    pool: &AccountPool,
    realm: &str,
    model: Option<&str>,
    now: f64,
) -> String {
    if pool.is_empty() {
        return "；该区域没有任何账号".into();
    }
    let mut reasons: Vec<String> = Vec::new();
    for a in pool.iter_accounts().filter(|a| a.realm == realm) {
        if let Some(r) = a.unavailable_reason(model, now) {
            let uid8 = a.uid.chars().take(8).collect::<String>();
            reasons.push(format!("{uid8} {r}"));
        }
    }
    if reasons.is_empty() {
        return String::new();
    }
    format!("；各账号：{}", reasons.join("，"))
}

/// anthropic_error_type（wb_proxy.py:13783-13798）：状态 → Anthropic error.type。
pub fn anthropic_error_type(status: u16) -> &'static str {
    match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// anthropic_error_obj（wb_proxy.py:13805-13811）：Anthropic 信封。
pub fn anthropic_error_obj(status: u16, message: &str, error_type: Option<&str>) -> Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type.unwrap_or_else(|| anthropic_error_type(status)),
            "message": message,
        }
    })
}

/// 读错误响应体（有界）：按**字节**截断再 lossy 解码（对齐 Python
/// `exc.read(n).decode("utf-8", "replace")`）。bytes() 消耗 Response 所有权。
async fn read_detail(resp: reqwest::Response, cap: usize) -> String {
    match resp.bytes().await {
        Ok(b) => {
            let b: Vec<u8> = b.iter().take(cap).copied().collect();
            String::from_utf8_lossy(&b).into_owned()
        }
        Err(_) => String::new(),
    }
}

/// 每轮失败 demote 的快捷封装（Python `if session_key and POOL: demote`）。
fn demote_session(pool: &mut AccountPool, session_key: Option<&str>, uid: &str) {
    if let Some(sk) = session_key {
        pool.demote(sk, Some(uid));
    }
}

/// open_upstream 主循环。见模块头契约。
pub async fn open_upstream(
    client: &UpstreamClient,
    pool: &mut AccountPool,
    egress: &EgressState,
    degrade: &mut DegradeGate,
    opts: &RetryOptions<'_>,
) -> Outcome {
    let total = 1usize.max(pool.count_ready(Some(opts.realm), opts.model, opts.now));
    let single_account = total <= 1;
    let max_attempts = total + 1 + (MAX_PRODUCT_SWITCHES as usize * usize::from(opts.auto_switch));
    let mut tried: HashSet<String> = HashSet::new();
    let mut charged: HashSet<String> = HashSet::new();
    let mut egress_wait: HashMap<String, f64> = HashMap::new();
    let mut last_error: Option<Failure> = None;
    let mut last_429: Option<(u16, String)> = None;
    let mut last_403_detail = String::new();
    let mut transient_hits: i64 = 0;
    let mut degraded_retried = false;
    let mut attempt_body = build_upstream_body(opts.payload, &opts.opts);

    'attempts: for attempt in 0..max_attempts {
        let picked = pool.pick_for_session(
            opts.realm,
            opts.session_key,
            Some(&tried),
            opts.model,
            None,
            opts.now,
        );
        let Some(idx) = picked else {
            if transient_hits > 0 && attempt < max_attempts - 1 {
                tried.clear();
                sleep_secs((1.5 * transient_hits as f64).min(3.0)).await;
                continue;
            }
            break;
        };
        let (uid, realm_of, token, product) = {
            let a = pool.account(idx).expect("picked index must exist");
            (
                a.uid.clone(),
                a.realm.clone(),
                a.access_token.clone(),
                a.product.clone(),
            )
        };
        if realm_of != opts.realm {
            demote_session(pool, opts.session_key, &uid);
            continue;
        }
        tried.insert(uid.clone());

        let base = opts
            .base_url
            .map(|b| b.to_string())
            .unwrap_or_else(|| chat_base_url(&realm_of, &product));
        let url = format!("{base}{CHAT_PATH}");
        let headers = chat_request_headers(&ChatHeadersParams {
            uid: &uid,
            token: &token,
            realm: &realm_of,
            product: &product,
            conversation_id: opts.conversation_id,
            device_token: None,
            enterprise_id: "",
            tenant_id: "",
            department: "",
        });

        let result = client.post(&url, &headers, &attempt_body).await;
        match result {
            Ok(resp) if resp.status().is_success() => {
                if let Some(a) = pool.account_mut(idx) {
                    notes::note_success(a, opts.model);
                }
                if opts.egress_backoff {
                    egress.note_success(&egress_key(&url));
                }
                if let Some(m) = opts.model {
                    reset_switch_counter(&uid, m);
                }
                return Outcome::Served(resp, idx);
            }
            Ok(resp) => {
                // ---- HTTPError 分支（wb_proxy.py:12579-12746）----
                let status = resp.status().as_u16();
                let detail = match status {
                    402 => read_detail(resp, 400).await,
                    _ => read_detail(resp, 600).await,
                };
                match status {
                    429 => {
                        let reset_at = parse_rate_limit_reset(&detail);
                        if rate_limit_is_account_level(&detail, reset_at) {
                            let wait = {
                                let a = pool.account_mut(idx).unwrap();
                                notes::note_soft_rate(a, "HTTP 429 (account soft rate)", opts.now)
                            };
                            let streak = pool.account(idx).map(|a| a.soft_streak).unwrap_or(0);
                            eprintln!(
                                "account {} soft-rate limited ({:?}), cooling {:.0}s (streak {})",
                                uid,
                                credential_scope_phrase(&detail),
                                wait,
                                streak
                            );
                            demote_session(pool, opts.session_key, &uid);
                            last_error = Some(Failure::Http {
                                status,
                                detail: detail.clone(),
                            });
                            last_429 = Some((status, detail.clone()));
                            continue;
                        }
                        let mut wait = match reset_at {
                            Some(r) => (r - opts.now).max(1.0),
                            None => 60.0,
                        };
                        if reset_at.is_none() && rate_limit_is_soft_shape(&detail) {
                            let a = pool.account_mut(idx).unwrap();
                            wait = notes::note_unscoped_rate(a);
                        }
                        {
                            let a = pool.account_mut(idx).unwrap();
                            notes::note_error(
                                a,
                                "HTTP 429 (model throttled)",
                                opts.model,
                                reset_at,
                                wait,
                                false,
                                &detail,
                                opts.now,
                            );
                        }
                        if let Some(r) = reset_at {
                            // note_limit_event（M4 用量打卡桩，best-effort 语义保留）
                            if rate_limit_is_cap(&detail) {
                                let a = pool.account_mut(idx).unwrap();
                                notes::note_model_cap(
                                    a,
                                    opts.model.unwrap_or(""),
                                    Some(r),
                                    opts.now,
                                );
                                // _save_account_caps 写凭证档属 M5
                            }
                        }
                        if opts.auto_switch {
                            let switched = {
                                let a = pool.account_mut(idx).unwrap();
                                try_switch_product(a, opts.model.unwrap_or(""), opts.now)
                            };
                            if switched {
                                tried.remove(&uid);
                                let a = pool.account_mut(idx).unwrap();
                                notes::clear_error(a, opts.model, true, opts.now);
                                demote_session(pool, opts.session_key, &uid);
                                continue;
                            }
                        }
                        demote_session(pool, opts.session_key, &uid);
                        last_error = Some(Failure::Http {
                            status,
                            detail: detail.clone(),
                        });
                        last_429 = Some((status, detail.clone()));
                        continue;
                    }
                    403 => {
                        if restriction_is_account_level(&detail) {
                            if let Some(a) = pool.account_mut(idx) {
                                // Python 不传 model：风控是账号级坏，冷却锁全账号
                                // （cooldown_until），而非单模型
                                notes::note_error(
                                    a,
                                    "HTTP 403 (account restricted)",
                                    None,
                                    None,
                                    RESTRICTED_403_COOLDOWN,
                                    single_account,
                                    &detail,
                                    opts.now,
                                );
                            }
                            demote_session(pool, opts.session_key, &uid);
                            last_error = Some(Failure::Http {
                                status,
                                detail: detail.clone(),
                            });
                            last_403_detail = detail;
                            continue;
                        }
                        if !degraded_retried
                            && (opts.prompt_mode == "passthrough" || opts.prompt_mode == "append")
                        {
                            degrade.trigger(opts.now);
                            degraded_retried = true;
                            let mut degraded_opts = opts.opts.clone();
                            degraded_opts.degraded = true;
                            attempt_body = build_upstream_body(opts.payload, &degraded_opts);
                            tried.remove(&uid);
                            continue;
                        }
                        demote_session(pool, opts.session_key, &uid);
                        last_error = Some(Failure::Http {
                            status,
                            detail: detail.clone(),
                        });
                        last_403_detail = detail;
                        break 'attempts;
                    }
                    402 => {
                        let a = pool.account_mut(idx).unwrap();
                        a.last_error = if detail.is_empty() {
                            "HTTP 402 (insufficient credits)".into()
                        } else {
                            detail.clone()
                        };
                        notes::note_balance_cooled(a, opts.now, opts.utc_offset);
                        demote_session(pool, opts.session_key, &uid);
                        last_error = Some(Failure::Http {
                            status,
                            detail: detail.clone(),
                        });
                        continue;
                    }
                    401 => {
                        if let Some(a) = pool.account_mut(idx) {
                            notes::note_error(
                                a,
                                "HTTP 401",
                                opts.model,
                                None,
                                60.0,
                                single_account,
                                "",
                                opts.now,
                            );
                        }
                        demote_session(pool, opts.session_key, &uid);
                        last_error = Some(Failure::Http { status, detail });
                        continue;
                    }
                    code if code >= 500 => {
                        transient_hits += 1;
                        if opts.egress_backoff && (code == 502 || code == 503 || code == 504) {
                            let wait = charge_egress(&mut egress_wait, egress, &url, opts.now);
                            demote_session(pool, opts.session_key, &uid);
                            last_error = Some(Failure::Http { status, detail });
                            sleep_secs(wait.min(EGRESS_SLEEP_MAX)).await;
                            continue;
                        }
                        let msg = format!("HTTP {code}");
                        charge_account(pool, &mut charged, idx, &msg, false, opts.now);
                        demote_session(pool, opts.session_key, &uid);
                        last_error = Some(Failure::Http { status, detail });
                        continue;
                    }
                    _ => {
                        // 未分类 HTTPError（400/404 等）→ Python `raise` 落入
                        // 外层 except Exception → 非瞬时 → 记 unknown 继续
                        let msg = format!("HTTP {status}: {}", short(&detail, 120));
                        charge_account(pool, &mut charged, idx, &msg, true, opts.now);
                        if let Some(a) = pool.account_mut(idx) {
                            notes::note_error(
                                a,
                                &msg,
                                opts.model,
                                None,
                                60.0,
                                single_account,
                                "",
                                opts.now,
                            );
                        }
                        demote_session(pool, opts.session_key, &uid);
                        last_error = Some(Failure::Http { status, detail });
                        continue;
                    }
                }
            }
            Err(e) => {
                // ---- 传输层异常（wb_proxy.py:12710-12743）----
                demote_session(pool, opts.session_key, &uid);
                if is_transient_err(&e) {
                    transient_hits += 1;
                    if opts.egress_backoff {
                        let wait = charge_egress(&mut egress_wait, egress, &url, opts.now);
                        last_error = Some(Failure::Transport(short(&e.to_string(), 200)));
                        sleep_secs(wait.min(EGRESS_SLEEP_MAX)).await;
                        continue;
                    }
                    charge_account(
                        pool,
                        &mut charged,
                        idx,
                        &format!("connection: {}", short(&e.to_string(), 120)),
                        true,
                        opts.now,
                    );
                    last_error = Some(Failure::Transport(e.to_string()));
                    sleep_secs((0.6 * transient_hits as f64).min(2.0)).await;
                    continue;
                }
                let msg = short(&e.to_string(), 120);
                charge_account(pool, &mut charged, idx, &msg, true, opts.now);
                if let Some(a) = pool.account_mut(idx) {
                    notes::note_error(
                        a,
                        &msg,
                        opts.model,
                        None,
                        60.0,
                        single_account,
                        "",
                        opts.now,
                    );
                }
                last_error = Some(Failure::Transport(e.to_string()));
                continue;
            }
        }
    }

    // ---- 循环后兜底（wb_proxy.py:12752-12790）----
    if let Some(err) = last_error {
        if let Some((_, detail)) = last_429 {
            let wait = retry_after_seconds(pool, opts.realm, opts.model, opts.now);
            let text = if detail.is_empty() {
                format!("upstream rate limit reached for this model; retry in {wait}s")
            } else {
                format!(
                    "upstream rate limit reached for this model; retry in {wait}s - {}",
                    short(&detail, 200)
                )
            };
            let (status, body) = openai_rate_limited(&text, wait);
            return Outcome::Rejected { status, body };
        }
        if !last_403_detail.is_empty() {
            let text = format!("upstream 403: {}", short(&last_403_detail, 200));
            let (status, body) = openai_error(403, &text, "invalid_request_error");
            return Outcome::Rejected { status, body };
        }
        return match err {
            Failure::Http { status, detail } => {
                let (s, body) = openai_error(
                    status,
                    &format!("upstream {status}: {detail}"),
                    "server_error",
                );
                Outcome::Rejected { status: s, body }
            }
            Failure::Transport(s) => {
                let (status, body) =
                    openai_error(502, &format!("upstream unreachable: {s}"), "server_error");
                Outcome::Rejected { status, body }
            }
        };
    }

    // 无 last_error：池冷或不可用。
    let (throttled, wait) = realm_model_throttled(pool, opts.realm, opts.model, opts.now);
    if throttled {
        let (status, body) = openai_rate_limited("usage exceeds frequency limit", wait);
        return Outcome::Rejected { status, body };
    }
    // daily/credit/model_token limit_blocked 兜底依赖 M5 设置数据，此处略。
    let detail = pool_unavailable_detail(pool, opts.realm, opts.model, opts.now);
    let message = format!(
        "no usable account for realm '{}': all are disabled, cooling down, expired or parked by the daily token limit{detail}",
        opts.realm
    );
    let (status, body) = openai_error(
        503,
        &format!("{message} - add or enable one at the dashboard (/)"),
        "server_error",
    );
    Outcome::Rejected { status, body }
}

/// 单请求内出口去重退避（Python `charge_egress` + `egress_wait` dict）。
fn charge_egress(
    egress_wait: &mut HashMap<String, f64>,
    egress: &EgressState,
    url: &str,
    now: f64,
) -> f64 {
    let key = egress_key(url);
    let wait = match egress_wait.get(&key) {
        Some(w) => *w,
        None => {
            let w = egress.note_transient(&key, now);
            egress_wait.insert(key, w);
            w
        }
    };
    wait
}

/// 给账号记一笔失败，一次请求最多记一笔（Python `charge_account`，12513-12524）。
fn charge_account(
    pool: &mut AccountPool,
    charged: &mut HashSet<String>,
    idx: usize,
    message: &str,
    unknown: bool,
    now: f64,
) -> bool {
    let uid = {
        let Some(a) = pool.account(idx) else {
            return false;
        };
        a.uid.clone()
    };
    if !charged.insert(uid) {
        return false;
    }
    if let Some(a) = pool.account_mut(idx) {
        if unknown {
            notes::note_unknown_failure(a, message, now);
        } else {
            notes::note_failure(a, message, now);
        }
    }
    true
}

fn short(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

/// 循环退避的单一入口（测试在 start_paused 下自动虚拟时间）。
async fn sleep_secs(secs: f64) {
    tokio::time::sleep(Duration::from_secs_f64(secs.max(0.0))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::PRODUCT_DESKTOP;
    use crate::headers::derive_id;
    use crate::pool::{AccountPool, PoolOptions};
    use crate::prompt::DEGRADED_PROMPT;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW: f64 = 1_792_000_000.0;

    fn account(uid: &str, realm: &str) -> AccountState {
        let mut a = AccountState::new(uid, realm);
        a.access_token = "test-token".into();
        a
    }

    fn pool_of(accounts: Vec<AccountState>) -> AccountPool {
        AccountPool::new(accounts, PoolOptions::default())
    }

    fn opts<'a>(
        realm: &'a str,
        model: Option<&'a str>,
        payload: &'a Value,
        base_url: Option<&'a str>,
    ) -> RetryOptions<'a> {
        RetryOptions {
            realm,
            model,
            session_key: Some("sk"),
            conversation_id: None,
            payload,
            opts: BodyOptions::default(),
            base_url,
            auto_switch: false,
            egress_backoff: false,
            prompt_mode: "passthrough",
            now: NOW,
            utc_offset: 8 * 3600,
        }
    }

    fn body_ok() -> serde_json::Value {
        json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]})
    }

    #[test]
    fn transient_markers_match_python() {
        assert!(is_transient_text("ssl: UNEXPECTED_EOF_WHILE_READING"));
        assert!(is_transient_text("Remote end closed connection"));
        assert!(is_transient_text("<urlopen error timed out>"));
        assert!(is_transient_text("HTTP Error 503: Service Unavailable"));
        assert!(!is_transient_text("HTTP Error 400: Bad Request"));
        assert!(!is_transient_text("connection refused by server"));
        assert!(!is_transient_text(""));
    }

    #[test]
    fn retry_after_picks_shortest_positive_wait() {
        let mut a1 = account("u1", "intl");
        let mut a2 = account("u2", "intl");
        a1.model_cooldowns.insert("gpt-5".into(), NOW + 100.0);
        a2.model_cooldowns.insert("gpt-5".into(), NOW + 50.0);
        let p = pool_of(vec![a1, a2]);
        assert_eq!(retry_after_seconds(&p, "intl", Some("gpt-5"), NOW), 50);
        let p2 = pool_of(vec![account("u1", "intl")]);
        assert_eq!(retry_after_seconds(&p2, "intl", Some("gpt-5"), NOW), 60);
    }

    #[test]
    fn realm_throttled_only_when_all_cool_this_model() {
        let mut a1 = account("u1", "intl");
        let mut a2 = account("u2", "intl");
        a1.model_cooldowns.insert("gpt-5".into(), NOW + 100.0);
        a2.model_cooldowns.insert("gpt-5".into(), NOW + 50.0);
        let p = pool_of(vec![a1, a2]);
        assert_eq!(
            realm_model_throttled(&p, "intl", Some("gpt-5"), NOW),
            (true, 50)
        );
        let mut a3 = account("u3", "intl");
        a3.cooldown_until = NOW + 500.0;
        let p2 = pool_of(vec![account("u1", "intl"), a3]);
        assert_eq!(
            realm_model_throttled(&p2, "intl", Some("gpt-5"), NOW),
            (false, 0)
        );
        let p3 = pool_of(vec![account("u1", "intl")]);
        assert_eq!(realm_model_throttled(&p3, "intl", None, NOW), (false, 0));
    }

    #[test]
    fn anthropic_envelopes_map_statuses() {
        assert_eq!(anthropic_error_type(429), "rate_limit_error");
        assert_eq!(anthropic_error_type(403), "permission_error");
        assert_eq!(anthropic_error_type(500), "api_error");
        let v = anthropic_error_obj(429, "busy", None);
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["message"], "busy");
    }

    #[test]
    fn switch_counter_rotates_products() {
        let mut a = account("u1", "intl");
        a.product = PRODUCT_DESKTOP.into();
        assert!(try_switch_product(&mut a, "gpt-5", NOW));
        assert_eq!(a.product, PRODUCT_VSCODE);
        assert!(try_switch_product(&mut a, "gpt-5", NOW + 1.0));
        assert_eq!(a.product, PRODUCT_CLI);
        assert!(try_switch_product(&mut a, "gpt-5", NOW + 2.0));
        assert_eq!(a.product, PRODUCT_DESKTOP);
        assert!(try_switch_product(&mut a, "gpt-5", NOW + 3.0));
        // 第 4 次后打满 → 拒绝
        assert!(!try_switch_product(&mut a, "gpt-5", NOW + 4.0));
        assert!(!try_switch_product(&mut a, "gpt-5", NOW + 5.0));
        reset_switch_counter(&a.uid, "gpt-5");
        let mut b = account("u2", "intl");
        assert!(try_switch_product(&mut b, "gpt-5", NOW + 100.0));
    }

    #[test]
    fn switch_counter_resets_after_success() {
        let mut a = account("u3", "intl");
        a.product = PRODUCT_DESKTOP.into();
        assert!(try_switch_product(&mut a, "gpt-5", NOW));
        reset_switch_counter(&a.uid, "gpt-5");
        assert!(try_switch_product(&mut a, "gpt-5", NOW + 10.0));
        assert_eq!(a.product, PRODUCT_CLI);
    }

    // ---- wiremock 全链路（start_paused：虚拟时间下 sleep 不真睡）----

    #[tokio::test(start_paused = true)]
    async fn serves_on_first_attempt_and_clears_penalties() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .mount(&server)
            .await;
        let mut a = account("u1", "intl");
        a.last_error = "old error".into();
        a.cooldown_until = NOW - 10.0; // 已过期：不阻塞 ready，但成功要清零
        a.fails = 3;
        a.soft_streak = 2;
        let mut p = pool_of(vec![a]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", Some("gpt-5"), &body_ok(), Some(&server.uri())),
        )
        .await;
        assert!(matches!(out, Outcome::Served(_, 0)));
        // note_success 清惩罚
        let a = p.account(0).unwrap();
        assert_eq!(a.last_error, "");
        assert_eq!(a.cooldown_until, 0.0);
        assert_eq!(a.fails, 0);
        // 未触发过降级 → 窗口不激活
        assert!(!d.active(NOW));
    }

    #[tokio::test(start_paused = true)]
    async fn model_429_then_second_account_serves() {
        let server = MockServer::start().await;
        // u1 回 429（无 reset、无 scope、soft_shape false）；u2 回 200。
        // matcher 用 X-Session-ID（= derive_id(uid, "session")）区分账号。
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u1", "session"),
            ))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"code": 429}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u2", "session"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .mount(&server)
            .await;
        let mut p = pool_of(vec![account("u1", "intl"), account("u2", "intl")]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", Some("gpt-5"), &body_ok(), Some(&server.uri())),
        )
        .await;
        assert!(matches!(out, Outcome::Served(_, 1)));
        // u1 被挂模型冷却 60s（未识别形状、无重置时刻）
        let a = p.account(0).unwrap();
        assert_eq!(a.model_cooldowns.get("gpt-5").copied(), Some(NOW + 60.0));
        assert!(a.last_error.contains("model throttled"));
    }

    #[tokio::test(start_paused = true)]
    async fn account_level_429_cools_soft_ladder() {
        let server = MockServer::start().await;
        // 无 reset 时钟 + 点名账号级短语 → note_soft_rate（600s 起）
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u1", "session"),
            ))
            .respond_with(ResponseTemplate::new(429).set_body_json(
                json!({"error": {"code": 429, "message": "your account was rate limited"}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u2", "session"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .mount(&server)
            .await;
        let mut p = pool_of(vec![account("u1", "intl"), account("u2", "intl")]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", Some("gpt-5"), &body_ok(), Some(&server.uri())),
        )
        .await;
        assert!(matches!(out, Outcome::Served(_, 1)));
        let a = p.account(0).unwrap();
        // 账号级软限：cooldown_until = now + 600s（soft_backoff(1)），last_error 对齐
        // Python `note_soft_rate("HTTP 429 (account soft rate)")`
        assert_eq!(a.cooldown_until, NOW + 600.0);
        assert_eq!(a.soft_streak, 1);
        assert_eq!(a.last_error, "HTTP 429 (account soft rate)");
    }

    #[tokio::test(start_paused = true)]
    async fn restriction_403_parks_account_for_two_hours() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u1", "session"),
            ))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(
                    json!({"error": {"code": 11140, "message": "account restricted"}}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u2", "session"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .mount(&server)
            .await;
        // 双账号：single_account=false → 冷却不被打折到 3s，验证 7200 主路径
        let mut p = pool_of(vec![account("u1", "intl"), account("u2", "intl")]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", Some("gpt-5"), &body_ok(), Some(&server.uri())),
        )
        .await;
        assert!(matches!(out, Outcome::Served(_, 1)));
        // 风控账号冷却 7200s，且是账号级（cooldown_until，不改动模型冷却）
        let a = p.account(0).unwrap();
        assert_eq!(a.cooldown_until, NOW + 7200.0);
        assert!(a.last_error.contains("account restricted"));
        assert!(a.model_cooldowns.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn content_403_degrades_prompt_and_retries() {
        let server = MockServer::start().await;
        // 后 mount 的 200 mock 优先：只匹配降级后 body（messages 整体替换为
        // 单条 system(DEGRADED_PROMPT)，wb_prompt.py degrade 语义）。
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u1", "session"),
            ))
            .and(wiremock::matchers::body_partial_json(json!({
                "messages": [{"role": "system", "content": DEGRADED_PROMPT}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .mount(&server)
            .await;
        // 降级前（messages 仍为原始 user）→ 403 内容审核（非风控：无 11140、
        // 无 markers，不满足 restriction_is_account_level）
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header(
                "X-Session-ID",
                derive_id("u1", "session"),
            ))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"error": {"code": 100, "message": "content policy rejected"}}),
            ))
            .mount(&server)
            .await;
        let mut p = pool_of(vec![account("u1", "intl")]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", Some("gpt-5"), &body_ok(), Some(&server.uri())),
        )
        .await;
        assert!(
            matches!(out, Outcome::Served(_, 0)),
            "degraded retry must succeed"
        );
        // 降级窗口已触发
        assert!(d.active(NOW));
        // 同账号重试且未被 403 冷却
        let a = p.account(0).unwrap();
        assert!(a.model_cooldowns.is_empty());
        assert!(!a.last_error.contains("403"));
    }

    #[tokio::test(start_paused = true)]
    async fn all_accounts_429_yields_rate_limited_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"code": 429}})))
            .mount(&server)
            .await;
        // 单账号 → 全试完 → 429 信封 + retry_after
        let mut p = pool_of(vec![account("u1", "intl")]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let out = open_upstream(
            &c,
            &mut p,
            &e,
            &mut d,
            &opts("intl", Some("gpt-5"), &body_ok(), Some(&server.uri())),
        )
        .await;
        match out {
            Outcome::Rejected { status, body } => {
                assert_eq!(status, 429);
                assert_eq!(body["error"]["type"], "rate_limit_error");
                assert_eq!(body["error"]["code"], 429);
                let wait = body["error"]["retry_after"].as_i64().unwrap();
                assert!(wait >= 60, "retry_after={wait}");
            }
            _ => panic!("expected Rejected 429"),
        }
        // 账号已被冷却（model_cooldowns），retry_after 反映真实等待
        let a = p.account(0).unwrap();
        assert!(a.model_cooldowns.contains_key("gpt-5"));
    }

    #[tokio::test(start_paused = true)]
    async fn auto_switch_rotates_identity_on_429() {
        let server = MockServer::start().await;
        // intl 下 X-Domain 恒为 www.workbuddy.ai（endpoint_for intl 不分 product），
        // 用 product 特异头 X-IDE-Name 区分：desktop → WorkBuddy、vscode → VSCode
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header("X-IDE-Name", "WorkBuddy"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"code": 429}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/chat/completions"))
            .and(wiremock::matchers::header("X-IDE-Name", "VSCode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .mount(&server)
            .await;
        let mut p = pool_of(vec![account("u1", "intl")]);
        let mut d = DegradeGate::default();
        let e = EgressState::default();
        let c = UpstreamClient::new();
        let uri = server.uri();
        let b = body_ok();
        let mut o = opts("intl", Some("gpt-5"), &b, Some(&uri));
        o.auto_switch = true;
        let out = open_upstream(&c, &mut p, &e, &mut d, &o).await;
        assert!(
            matches!(out, Outcome::Served(_, 0)),
            "auto-switch must rotate to vscode"
        );
        // 身分已旋转到 vscode
        assert_eq!(p.account(0).unwrap().product, PRODUCT_VSCODE);
    }
}
