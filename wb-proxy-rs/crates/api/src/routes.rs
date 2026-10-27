use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::auth::{key_ok, PanelSessions};
use crate::chat::{extract_session_key, run_chat, ChatParams, ChatRun, Upstream, UsageContext};
use crate::config::PanelConfig;
use crate::panel_auth::{
    panel_password_is_default, set_panel_password, verify_panel_password, LoginLimiter,
    DEFAULT_PANEL_PASSWORD,
};
use crate::Config;

use wb_gateway::anthropic::{chat_to_messages, messages_to_chat};
use wb_gateway::body::BodyOptions;
use wb_gateway::pool::ConcurrencyGate;
use wb_gateway::responses::{
    chat_to_response, custom_tool_names, is_truthy, request_meta_from_payload, responses_sse_frame,
    responses_to_chat, RequestMeta, ResponsesStreamTranslator,
};
use wb_gateway::retry::{anthropic_error_obj, Outcome};
use wb_gateway::stream::{aggregate_stream, strip_data_prefix};
use wb_tasks::api_routes as task_routes;
use wb_tasks::scheduler::PanelSink;
use wb_tasks::settings_view::{runtime_settings_view, save_settings};

/// 面板会话 TTL：7 天（对齐 wb_settings.SESSION_TTL）。
const SESSION_TTL: u64 = 7 * 24 * 3600;

/// 并发闸默认上限（Python `MAX_CONCURRENT_CHAT`，wb_settings 默认 32）。
const MAX_CONCURRENT_CHAT: usize = 32;

/// 槽位等待秒数（Python `CHAT_SLOT_WAIT_SECONDS`，env `WB_CHAT_SLOT_WAIT`）。
/// M3 影子实例不读 env（env 解析属 main.rs 职责），先用 Python 默认值 30。
const CHAT_SLOT_WAIT_SECONDS: f64 = 30.0;

/// 默认模型（Python `_dispatch_chat_post`：`payload.get("model") or "hy4-preview"`）。
const DEFAULT_MODEL: &str = "hy4-preview";

/// `previous_response_id` 不支持时的 400 文案（Python :17888-17895 **逐字**照抄）。
///
/// 网关是无状态的：不存上一次的回应。客户端拿 `previous_response_id` 来追问时，
/// 静默忽略会得到一个「看着正常、其实缺了上下文」的回答 —— 比直接报错更不诚实。
const PREVIOUS_RESPONSE_ID_UNSUPPORTED: &str = concat!(
    "previous_response_id is not supported: this gateway does not store response state. ",
    "Send the full conversation in 'input' instead, or use a stateless client.",
);

/// 带 CORS 头的响应路径前缀（Python `CORS_PATH_PREFIXES`）。
/// 注意：Python 用 `startswith` 逐前缀判 `server_path`（已剥 query），
/// 所以 `"/chat/completions".starts_with("/chat")` 为真 —— 这里保持前缀语义。
const CORS_PATH_PREFIXES: [&str; 5] = ["/v1", "/chat", "/completions", "/models", "/responses"];

/// 管理端点前缀：命中则不回 CORS 头（Python `MANAGEMENT_PATH_PREFIXES`）。
const MANAGEMENT_PATH_PREFIXES: [&str; 8] = [
    "/v1/usage",
    "/usage",
    "/accounts",
    "/settings",
    "/tasks",
    "/scheduler",
    "/panel",
    "/logs",
];

/// OpenAI 侧 6 条 chat 路由（Python `do_POST` 的 OpenAI 分支）。
const OPENAI_PATHS: [&str; 6] = [
    "/v1/chat/completions",
    "/chat/completions",
    "/v1/completions",
    "/completions",
    "/v1/responses",
    "/responses",
];

/// Anthropic 侧 4 条路由；前两条是 messages（占并发槽），后两条是
/// count_tokens（**不占槽**，Python `do_POST` 里 count_tokens 分支早于闸）。
const ANTHROPIC_MESSAGE_PATHS: [&str; 2] = ["/v1/messages", "/messages"];
const ANTHROPIC_COUNT_PATHS: [&str; 2] = ["/v1/messages/count_tokens", "/messages/count_tokens"];

/// 面板路由前缀表（Python `wb_proxy._is_panel_route` :15661-15689，逐条照抄）。
///
/// **命中这些前缀的路由必须有面板会话**（`X-Panel-Token` 有效），API key
/// **不够** —— 这是 Python 的双层门禁：外层 `_is_panel_route` 查面板会话，
/// 内层 `_authorized()` 才放行 API key。Rust 侧对齐这个强度。
///
/// ⚠️ `/panel` **不在**表里（面板自己的三条认证路由自己管门禁）；
/// `/v1/models` 也不在表里（Python 注释：模型列表只用 API key 就能访问，
/// 好让普通 OpenAI 客户端继续发现模型）。
///
/// 注意 `/usage` 与 `/v1/usage` **都**在表里（Python 用
/// `path.startswith("/usage") or path.startswith("/v1/usage")`）。
const PANEL_ROUTE_PREFIXES: [&str; 11] = [
    "/accounts",
    "/usage",
    "/v1/usage",
    "/activity",
    "/tasks",
    "/scheduler",
    "/settings",
    "/updates",
    "/logs",
    "/pricing",
    "/agents",
];

/// 聚合路由状态：配置 + 面板会话表 + 进程级上游依赖 + 并发闸。
///
/// `upstream` / `gate` 必须是**进程级单例**（Python 侧 `POOL`/`DEGRADE`/闸都是
/// 模块全局）；`router` 内只构造一次，之后随 `Arc` 共享。
#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    panels: Arc<PanelSessions>,
    upstream: Arc<Upstream>,
    gate: Arc<ConcurrencyGate>,
    task: Arc<task_routes::TaskRuntime>,
    /// 面板可调项（退避时长、可信代理网段）。
    panel_cfg: Arc<PanelConfig>,
    /// 登录限流器（Python 的模块全局 `_login_attempts`）。
    ///
    /// 进程级单例：跨请求共享失败计数，所以必须是 `AppState` 的一部分而不是
    /// handler 的局部变量。
    login_limiter: Arc<LoginLimiter>,
}

impl AppState {
    /// 从 `accounts_dir` 装载账号进池（M3 关卡 5.1 的装载逻辑）。
    ///
    /// 走 `pool_accounts`（**不做** enabled/cooldown 过滤——那些是池运行期
    /// 状态，Python 也是全量载入后由 `ready()` 判）。`access_token` 只在
    /// 进程内传递，**不打印**。装载失败（目录不可读）时以空池启动并让
    /// chat 路径回 503（池不可用），不 panic。
    fn build(
        cfg: Arc<Config>,
        panels: Arc<PanelSessions>,
        task: Arc<task_routes::TaskRuntime>,
        panel_cfg: PanelConfig,
    ) -> Self {
        let accounts: Vec<wb_gateway::pool::AccountState> =
            match wb_storage::pool_accounts(&cfg.accounts_dir) {
                Ok(entries) => entries
                    .into_iter()
                    .map(|entry| {
                        let mut state =
                            wb_gateway::pool::AccountState::new(&entry.uid, &entry.realm);
                        state.has_token = entry.has_token;
                        state.access_token = entry.access_token;
                        state.product = entry.product;
                        state
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
        let upstream = Arc::new(Upstream::new(accounts));
        let gate = Arc::new(ConcurrencyGate::new(
            MAX_CONCURRENT_CHAT,
            false,
            MAX_CONCURRENT_CHAT,
            CHAT_SLOT_WAIT_SECONDS,
        ));
        Self {
            cfg,
            panels,
            upstream,
            gate,
            task,
            panel_cfg: Arc::new(panel_cfg),
            login_limiter: Arc::new(LoginLimiter::new()),
        }
    }
}

pub fn router(cfg: Arc<Config>) -> Router {
    let task = task_routes::TaskRuntime::production(&cfg.accounts_dir, Arc::new(ApiPanelSink));
    start_background_tasks(&task);
    app_with(cfg.clone(), Arc::new(PanelSessions::new(SESSION_TTL)), task)
}

/// 启动后台任务（调度器 + 后台积分刷新器）。
///
/// ⚠️ **默认关闭**，需显式设置 `WB_BACKGROUND_TASKS=1` 才启动。
///
/// 与 Python 的差异（D49）：Python 侧 `wb_proxy.py:18757-18785` 在 `_bootstrap`
/// 里**无条件**起 `Scheduler` / `PriceRefresher` / `CreditsRefresher` /
/// `UpdateChecker` 四个后台线程；Rust 侧此时**尚未切流**（M7 之前生产跑的仍是
/// Python 实例），若默认就起，会出现「同一个账号池被两个实例同时签到/刷积分」。
/// 故 Rust 侧默认不启动后台任务，切流（M7）时再对齐 Python 的默认行为。
///
/// ⚠️ 本函数只在**生产入口** `router()` 里调用；契约测试走 `app_with()`，
/// 不会起任何后台线程 ⇒ 测试永不触发真实上游请求。
fn start_background_tasks(task: &Arc<task_routes::TaskRuntime>) {
    let enabled = std::env::var("WB_BACKGROUND_TASKS")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if !enabled {
        eprintln!("[wb-api] 后台任务未启动（需显式设置 WB_BACKGROUND_TASKS=1）");
        return;
    }
    if let Some(scheduler) = task.scheduler() {
        scheduler.start();
        eprintln!("[wb-api] 后台调度器已启动");
    }
    if let Some(refresher) = task.credits_refresher.get() {
        refresher.start();
        eprintln!("[wb-api] 后台积分刷新器已启动");
    }
    if let Some(pricing) = task.price_refresher.get() {
        pricing.start();
        eprintln!("[wb-api] 后台定价刷新器已启动");
    }
    if let Some(updater) = task.update_checker.get() {
        updater.start();
        eprintln!("[wb-api] 后台更新检查器已启动");
    }
}

pub fn app_with(
    cfg: Arc<Config>,
    panels: Arc<PanelSessions>,
    task: Arc<task_routes::TaskRuntime>,
) -> Router {
    app_with_config(cfg, panels, task, PanelConfig::from_env())
}

/// 同 [`app_with`]，但可注入面板可调项。
///
/// 契约测试用它在**不**等 0.5 秒退避的前提下跑登录失败用例（生产口径的
/// 退避时长由 [`PanelConfig::from_env`] 给默认值）。
pub fn app_with_config(
    cfg: Arc<Config>,
    panels: Arc<PanelSessions>,
    task: Arc<task_routes::TaskRuntime>,
    panel_cfg: PanelConfig,
) -> Router {
    let state = AppState::build(cfg, panels, task, panel_cfg);
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard))
        .route("/realm", get(realm_get).post(realm_post))
        .route("/panel/status", get(panel_status).options(preflight))
        .route("/panel/login", post(panel_login).options(preflight))
        .route("/panel/logout", post(panel_logout).options(preflight))
        .route("/panel/password", post(panel_password).options(preflight))
        .route("/settings", get(settings_get).options(preflight))
        .route("/settings/save", post(settings_save).options(preflight))
        .route(
            "/accounts/checkin",
            post(accounts_checkin).options(preflight),
        )
        .route(
            "/accounts/daily-chat",
            post(accounts_daily_chat).options(preflight),
        )
        .route(
            "/accounts/daily-chat-web",
            post(accounts_daily_chat_web).options(preflight),
        )
        // 账户管理 17 条 POST 路由（关卡 4.24 线 C）：统一收口到 `accounts_mgmt`，
        // 由 `inject_path` 中间件写进 `x-wb-path` 头的原始路径区分子路由，再
        // dispatch 到 wb_tasks::api_routes::route_accounts_*（线 B 实现）。
        // 与 /accounts/checkin 等既有路由同为「面板路由 + POST + OPTIONS preflight」。
        .route("/accounts/credits", post(accounts_mgmt).options(preflight))
        .route(
            "/accounts/credits/fetch",
            post(accounts_mgmt).options(preflight),
        )
        .route(
            "/accounts/credits/detail",
            post(accounts_mgmt).options(preflight),
        )
        .route(
            "/accounts/credits/grants",
            post(accounts_mgmt).options(preflight),
        )
        .route("/accounts/delete", post(accounts_mgmt).options(preflight))
        .route("/accounts/export", post(accounts_mgmt).options(preflight))
        .route("/accounts/import", post(accounts_mgmt).options(preflight))
        .route(
            "/accounts/import/desktop",
            post(accounts_mgmt).options(preflight),
        )
        .route(
            "/accounts/login/start",
            post(accounts_mgmt).options(preflight),
        )
        .route(
            "/accounts/login/poll",
            post(accounts_mgmt).options(preflight),
        )
        .route(
            "/accounts/login/cancel",
            post(accounts_mgmt).options(preflight),
        )
        .route("/accounts/product", post(accounts_mgmt).options(preflight))
        .route("/accounts/refresh", post(accounts_mgmt).options(preflight))
        .route("/accounts/set", post(accounts_mgmt).options(preflight))
        .route("/accounts/set-all", post(accounts_mgmt).options(preflight))
        .route(
            "/accounts/sync-profile",
            post(accounts_mgmt).options(preflight),
        )
        .route("/accounts/test", post(accounts_mgmt).options(preflight))
        .route("/tasks", get(tasks_get).options(preflight))
        .route("/tasks/run", post(tasks_run).options(preflight))
        .route("/tasks/travel", post(tasks_travel).options(preflight))
        .route(
            "/tasks/streak-bonus",
            post(tasks_streak_bonus).options(preflight),
        )
        .route("/tasks/claim-all", post(tasks_claim_all).options(preflight))
        .route("/scheduler", get(scheduler_get).options(preflight))
        .route("/pricing", get(pricing_get).options(preflight))
        .route("/updates", get(updates_get).options(preflight))
        .route("/updates/check", post(update_check).options(preflight))
        .route("/pricing/mapping", post(pricing_mapping).options(preflight))
        .route("/pricing/refresh", post(pricing_refresh).options(preflight))
        .route("/logs", get(logs_get).options(preflight))
        .route("/logs/export", get(logs_export).options(preflight))
        .route("/logs/clear", post(logs_clear).options(preflight))
        .route("/settings/reveal", get(settings_reveal).options(preflight))
        .route("/usage", get(usage_get).options(preflight))
        .route("/v1/usage", get(usage_get).options(preflight))
        .route(
            "/usage/by-account",
            get(usage_by_account).options(preflight),
        )
        .route("/usage/analytics", get(usage_analytics).options(preflight))
        .route("/usage/recent", get(usage_recent_get).options(preflight))
        .route(
            "/usage/timeseries",
            get(usage_timeseries_get).options(preflight),
        )
        .route("/usage/perf", get(usage_perf_get).options(preflight))
        .route(
            "/activity/history",
            get(activity_history_get).options(preflight),
        )
        .route(
            "/accounts/credits-refresher",
            get(credits_refresher_get).options(preflight),
        )
        .route(
            "/scheduler/trigger",
            post(scheduler_trigger).options(preflight),
        )
        .route(
            "/scheduler/toggle",
            post(scheduler_toggle).options(preflight),
        );
    for path in OPENAI_PATHS {
        router = router.route(path, post(chat_openai_entry).options(preflight));
    }
    for path in ANTHROPIC_MESSAGE_PATHS {
        router = router.route(path, post(chat_anthropic_entry).options(preflight));
    }
    for path in ANTHROPIC_COUNT_PATHS {
        router = router.route(path, post(count_tokens_entry).options(preflight));
    }
    router = router
        .route("/proxy/slots", get(proxy_slots_get).options(preflight))
        .route(
            "/proxy/slots/save",
            post(proxy_slots_save).options(preflight),
        )
        .route(
            "/proxy/slots/test",
            post(proxy_slots_test).options(preflight),
        )
        .route("/proxy/discover", post(proxy_discover).options(preflight))
        .route("/agents", get(agents_get).options(preflight))
        .route(
            "/agents/available",
            get(agents_available).options(preflight),
        )
        .route("/agents/cli", get(agents_cli).options(preflight))
        .route("/agents/apply", post(agents_apply).options(preflight))
        .route("/agents/restore", post(agents_restore).options(preflight))
        .route(
            "/usage/remaining",
            get(usage_remaining_get).options(preflight),
        );
    // 404 兜底：未匹配路径统一回 Python `_error(404, "not found")` 信封。
    let path_echo = axum::middleware::from_fn(inject_path);
    router
        .fallback(not_found)
        .layer(path_echo)
        .layer(axum::middleware::from_fn(cors_layer))
        .with_state(state)
}

/// 把原始请求路径写进 `x-wb-path` 头。
///
/// axum handler 拿不到 `Uri`（`chat_openai_entry` 要区分 `/v1/responses` 与
/// `/v1/chat/completions` 两条不同翻译路径），所以在路由前统一注入。
async fn inject_path(mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if let Ok(value) = HeaderValue::from_str(req.uri().path()) {
        req.headers_mut().insert("x-wb-path", value);
    }
    next.run(req).await
}

/// CORS 预处理（Python `do_OPTIONS` :15690-15697）：空白 204 + 三个 `*` 头。
async fn preflight() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response
}

/// cors_origin_allowed（wb_proxy.py:15679-15688）：先剥 `?` 后缀，Management
/// 前缀一律不回 CORS（安全：避免浏览器直读管理面），否则命中 chat 前缀才回。
fn cors_origin_allowed(path: &str) -> bool {
    let bare = path.split('?').next().unwrap_or(path);
    if MANAGEMENT_PATH_PREFIXES.iter().any(|p| bare.starts_with(p)) {
        return false;
    }
    CORS_PATH_PREFIXES.iter().any(|p| bare.starts_with(p))
}

/// 统一 CORS 中间件：OPTIONS 已在路由内短路，这里只给命中白名单的响应补
/// `Access-Control-Allow-Origin: *`（对齐 Python 在 `send_header` 阶段加头）。
async fn cors_layer(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let mut response = next.run(req).await;
    if method != Method::OPTIONS && cors_origin_allowed(&path) {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }
    response
}

/// 404 信封（Python `_error(404, "not found", "invalid_request_error")`）。
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {
            "message": "not found", "type": "invalid_request_error", "code": 404
        }})),
    )
        .into_response()
}

/// OpenAI 信封 `_error`（wb_proxy.py:1189）：`type` 默认 `server_error`。
fn openai_error(code: u16, message: &str, err_type: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error": {
            "message": message, "type": err_type, "code": code
        }})),
    )
        .into_response()
}

/// Anthropic 信封 `_anthropic_error`（wb_proxy.py:1193）：**无 code 字段**。
fn anthropic_error(code: u16, message: &str, err_type: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(anthropic_error_obj(code, message, Some(err_type))),
    )
        .into_response()
}

/// 401 判定（Python `_authorized` :15635-15659）：经 `key_ok` 三条路径，
/// 失败时 401 信封在两协议族下形状不同。
fn unauthorized(headers: &HeaderMap, params: &HashMap<String, String>, state: &AppState) -> bool {
    !key_ok(headers, params, &state.cfg, &state.panels)
}

// ---------------------------------------------------------------------------
// 面板路由的双层门禁（对齐 Python `wb_proxy.py:15709` + `:18318`）
// ---------------------------------------------------------------------------

/// 取 `X-Panel-Token`（Python `_panel_token` :15655-15658）。
fn panel_token(headers: &HeaderMap) -> String {
    headers
        .get("x-panel-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 面板会话是否有效（Python `_panel_ok` :15660：`PANEL.valid(token)`）。
fn panel_session_ok(headers: &HeaderMap, state: &AppState) -> bool {
    state.panels.valid(&panel_token(headers))
}

/// 是否命中面板路由前缀表（Python `_is_panel_route` :15661-15689，逐条照抄）。
fn is_panel_route(path: &str) -> bool {
    PANEL_ROUTE_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// 面板路由是否需要 401（Python 外层门禁 :15709 / :18318）。
///
/// 命中面板前缀**且**面板会话无效 ⇒ 需要 401「panel password required」。
/// 注意：**光有 API key 不算通过** —— 这正是 Python 的双层语义，
/// 外层只看面板会话。
fn panel_required(path: &str, headers: &HeaderMap, state: &AppState) -> bool {
    is_panel_route(path) && !panel_session_ok(headers, state)
}

/// 面板路由的 401 信封（Python `_error(401, "panel password required",
/// "invalid_request_error")`）。
///
/// ⚠️ 与模型接口的 401（`"invalid api key - …"`）是**两套文案**，不能混用：
/// 面板路由的 401 说的是「缺面板口令」，模型接口的 401 说的是「key 不对」。
fn panel_unauthorized() -> Response {
    openai_error(401, "panel password required", "invalid_request_error")
}

/// 面板路由的双层门禁：外层面板会话 + 内层 `_authorized()`。
///
/// 返回 `Some(response)` 表示应当拒绝请求。
///
/// - 外层：命中面板前缀但面板会话无效 ⇒ 401「panel password required」。
/// - 内层：`_authorized()`（API key 或面板会话）失败 ⇒ 401「invalid api key」。
///   外层已挡住「只有 API key」的情况，所以内层实际是兜底。
fn panel_gate(
    path: &str,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    state: &AppState,
) -> Option<Response> {
    if panel_required(path, headers, state) {
        return Some(panel_unauthorized());
    }
    if unauthorized(headers, params, state) {
        return Some(openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(state)),
            "invalid_request_error",
        ));
    }
    None
}

/// 并发闸应答（Python `do_POST` 超限分支）：Anthropic 用 `overloaded_error`，
/// OpenAI 用默认 `server_error`；正文文案一致。
fn overload_response(is_anthropic: bool) -> Response {
    let message = format!(
        "gateway is at its concurrent chat limit ({MAX_CONCURRENT_CHAT} in flight); retry shortly"
    );
    if is_anthropic {
        anthropic_error(503, &message, "overloaded_error")
    } else {
        openai_error(503, &message, "server_error")
    }
}

/// 一次 chat 的最终应答 + 记账所需的信息（关卡 4.19 线 C）。
struct ChatReply {
    /// 给客户端的响应。
    response: Response,
    /// 上游 usage 块（没拿到 ⇒ `None`）。
    usage: Option<Value>,
    /// 失败时的 HTTP 状态（成功 ⇒ `None`）。
    status: Option<u16>,
    /// 失败时的一句话说明（成功 ⇒ `None`）。
    message: Option<String>,
}

/// 读上游 `reqwest::Response` 全文，逐行 `strip_data_prefix` 后聚合。
///
/// 对齐 Python `_chat_nonstream_response` :18584：上游回的是 **SSE 流**，
/// 非流式响应靠 `aggregate_stream` 把 SSE 事件拼成 chat JSON。
async fn aggregate_upstream(resp: reqwest::Response, model: &str, now: f64) -> ChatReply {
    let text = match resp.text().await {
        Ok(t) => t,
        Err(err) => {
            let message = format!("upstream stream error: {err}");
            return ChatReply {
                response: openai_error(502, &message, "server_error"),
                usage: None,
                status: Some(502),
                message: Some(message),
            };
        }
    };
    let payloads: Vec<&str> = text.lines().map(strip_data_prefix).collect();
    let result = aggregate_stream(&payloads, model, "", now);
    let usage = result.get("usage").cloned();
    ChatReply {
        response: (StatusCode::OK, Json(result)).into_response(),
        usage,
        status: None,
        message: None,
    }
}

/// 把 `Outcome` 落到 HTTP 响应（非流式）。`Rejected` 直接透传上游状态与
/// 信封；`Served` 走 `aggregate_upstream`。
async fn outcome_to_response(outcome: Outcome, model: &str, now: f64) -> ChatReply {
    match outcome {
        Outcome::Served(resp, _) => aggregate_upstream(resp, model, now).await,
        Outcome::Rejected { status, body } => rejected_reply(status, body),
    }
}

/// 请求体解析：空体视作 `{}`（Python `json.loads` 失败时回 400，但 chat 路径
/// 的 Python 实现允许缺省字段，这里对空体取 `{}` 更贴近其宽容行为）。
///
/// `Err` 侧装箱：`Response<Body>` 达 128 字节，直接作 `Err` 会触发
/// `clippy::result_large_err`；调用点 `Err(resp) => return resp` 处的
/// `Box<Response>` → `Response` 解引用强转保证语义不变。
fn parse_body(bytes: &Bytes) -> Result<Value, Box<Response>> {
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(bytes).map_err(|err| {
        Box::new(openai_error(
            400,
            &format!("invalid JSON body: {err}"),
            "invalid_request_error",
        ))
    })
}

/// Unix 秒（浮点），对齐 Python `time.time()` 的入参语义。
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `utc_offset` 秒数（Python 用本地时区偏移；M3 影子实例固定 +8 小时，
/// 对齐 `wb_gateway::prompt::next_midnight_cst` 已硬编码 +8h 的取舍）。
const UTC_OFFSET_SECS: i64 = 8 * 3600;

/// OpenAI 侧入口（6 条路径）。`/v1/responses` 与 `/responses` 需先经
/// `responses_to_chat` 翻译（Python `_handle_responses`）。
async fn chat_openai_entry(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return preflight().await;
    }
    if unauthorized(&headers, &params, &state) {
        let hint = auth_hint(&state);
        return openai_error(
            401,
            &format!("invalid api key - {hint}"),
            "invalid_request_error",
        );
    }
    let raw = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let is_responses = matches!(
        request_path(&headers).as_deref(),
        Some("/v1/responses") | Some("/responses")
    );
    // 差距①（Python :17888-17895）：`previous_response_id` 直接 400。
    //
    // 判据用 Python 的真值口径（`payload.get("previous_response_id")`）：空串 /
    // null / false 不算「带了 id」，不拦。
    if is_responses && raw.get("previous_response_id").is_some_and(is_truthy) {
        return openai_error(
            400,
            PREVIOUS_RESPONSE_ID_UNSUPPORTED,
            "invalid_request_error",
        );
    }
    // 差距③：`stream` 标志必须从**原始** payload 读 —— 翻译后的 chat body
    // 不带它（Python 同样不复制，但 Python 从原始 payload 读，:17911）。
    let want_stream = is_responses && raw.get("stream").is_some_and(is_truthy);
    // 差距②：回显元数据从**原始** payload 取（翻译会把 tools 降级、丢掉
    // tool_choice/parallel_tool_calls 的原样形状）。
    let request_meta = if is_responses {
        request_meta_from_payload(&raw)
    } else {
        RequestMeta::default()
    };
    // custom（freeform）工具名：从**原始** payload 取（Python :17892）。
    let custom_names = if is_responses {
        custom_tool_names(raw.get("tools"))
    } else {
        Vec::new()
    };
    let payload = if is_responses {
        responses_to_chat(&raw, None)
    } else {
        raw.clone()
    };
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();

    // 并发闸：超限立刻回 503（Python 在 open_upstream 之前判）。
    let Some(_permit) = acquire_slot(&state).await else {
        return overload_response(false);
    };

    let session_key = extract_session_key(&headers, &payload);
    let now = now_secs();
    let opts = body_options(&state, &payload, now);

    // `total_tokens` 类估算在 gateway 内处理；api 层只搬运。
    let _ = &state;
    let ChatRun {
        outcome,
        account_uid,
    } = run_chat(
        &state.upstream,
        ChatParams {
            realm: &state.cfg.realm,
            model: Some(&model),
            session_key: session_key.as_deref(),
            conversation_id: session_key.as_deref(),
            payload: &payload,
            opts,
            base_url: state.cfg.upstream_base_url.as_deref(),
            auto_switch: true,
            egress_backoff: false,
            prompt_mode: "chat",
            now,
            utc_offset: UTC_OFFSET_SECS,
        },
    )
    .await;

    // 差距③：流式分支。上游 chat SSE 逐帧翻译成 Responses 事件帧写回客户端。
    //
    // ⚠️ **记账在流任务内部做**：流式的 usage 只有流结束才知道（Python 侧同样
    // 在 `_responses_stream_response` 结束后才 `record_usage(stream=True)`），
    // 处理器这一刻拿不到 usage。
    if want_stream {
        return match outcome {
            Outcome::Served(resp, _) => responses_sse_response(
                state,
                resp,
                ResponsesStreamArgs {
                    model,
                    request_meta,
                    custom_names,
                    client_payload: raw,
                    account_uid,
                    now,
                },
            ),
            Outcome::Rejected { status, body } => {
                // 上游拒绝：没有流可推，但**照样要记一行**（对齐非流式路径与
                // Python 的 `record_error`）。流式路径的记账在流任务内部做，
                // 这条拒绝发生在流开始之前，故在这里记。
                let reply = rejected_reply(status, body);
                record_chat_usage(
                    &state,
                    UsageContext {
                        model: &model,
                        payload: &raw,
                        usage: None,
                        account: account_uid.as_deref(),
                        realm: &state.cfg.realm,
                        key: "",
                        status: reply.status,
                        message: reply.message.as_deref(),
                        started: now,
                        or_models: None,
                        ttft_ms: None,
                        gen_ms: None,
                    },
                );
                reply.response
            }
        };
    }

    let reply = outcome_to_response(outcome, &model, now).await;
    record_chat_usage(
        &state,
        UsageContext {
            model: &model,
            // responses 路由用**原始**请求体记账：`stream` 标志在原始体里，
            // 翻译后的 chat body 不带它（否则流式请求会被记成非流式）。
            payload: if is_responses { &raw } else { &payload },
            usage: reply.usage.as_ref(),
            account: account_uid.as_deref(),
            realm: &state.cfg.realm,
            // 客户端 API key 的**标识**在 Rust 侧拿不到（只有密钥本体，
            // 绝不能落盘）⇒ 恒为空串，与 Python「没带 key 跑」的桶同款。
            key: "",
            status: reply.status,
            message: reply.message.as_deref(),
            started: now,
            or_models: None,
            ttft_ms: None,
            gen_ms: None,
        },
    );
    let response = reply.response;
    if is_responses {
        wrap_response_response(response, &custom_names, &request_meta).await
    } else {
        response
    }
}

/// `chat_to_response` 包装（Python `_handle_responses` 的成功分支）。
///
/// 非流式：把上游聚合出的 chat JSON 转成 Responses 形状；错误信封原样返回
/// （错误路径由上游状态决定，不再二次翻译）。失败时回落原响应。
async fn wrap_response_response(
    response: Response,
    custom_names: &[String],
    request_meta: &RequestMeta,
) -> Response {
    if response.status() != StatusCode::OK {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return openai_error(502, "response translation failed", "server_error"),
    };
    let chat_obj: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return openai_error(502, "response translation failed", "server_error"),
    };
    let model = chat_obj
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let created = now_secs() as i64;
    let out = chat_to_response(&chat_obj, &model, custom_names, request_meta, created);
    Response::from_parts(parts, axum::body::Body::from(out.to_string()))
}

/// 上游拒绝 ⇒ 透传其状态与信封（Python `Outcome::Rejected` 分支）。
///
/// 单独抽出来是为了让流式分支能复用同一条拒绝路径（流式与非流式的拒绝
/// 响应必须一致）。
fn rejected_reply(status: u16, body: Value) -> ChatReply {
    let message = body
        .get("error")
        .and_then(|err| err.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("upstream rejected with status {status}"));
    ChatReply {
        response: (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(body),
        )
            .into_response(),
        usage: None,
        status: Some(status),
        message: Some(message),
    }
}

/// `/v1/responses` 流式分支需要的参数（打包成结构体，避免参数列表过长）。
struct ResponsesStreamArgs {
    /// 回显与翻译用的模型名。
    model: String,
    /// 回显元数据（从原始 payload 取）。
    request_meta: RequestMeta,
    /// custom（freeform）工具名。
    custom_names: Vec<String>,
    /// 客户端**原始**请求体（记账取 `stream` 标志用）。
    client_payload: Value,
    /// 服务本次请求的账号 uid（记账用）。
    account_uid: Option<String>,
    /// 请求开始时刻（epoch 秒，记账算耗时用）。
    now: f64,
}

/// `/v1/responses` 的 `stream: true` 分支（Python `_responses_stream_response`
/// :17970-18050 + `stream_responses_events` :14590-15085）。
///
/// 上游 chat SSE 逐行喂进 [`ResponsesStreamTranslator`]，产出的 Responses 事件
/// 帧按 SSE 写回客户端（`event: <type>\ndata: <json>\n\n`）。
///
/// ⚠️ **记账在流任务收尾时做**：流式的 usage 只有流结束才知道（Python 侧同样
/// 在流结束后才 `record_usage(..., stream=True)`）。
fn responses_sse_response(
    state: AppState,
    mut resp: reqwest::Response,
    args: ResponsesStreamArgs,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let created = args.now as i64;
    let model = args.model.clone();
    let request_meta = args.request_meta;
    let custom_names = args.custom_names;
    let client_payload = args.client_payload;
    let account_uid = args.account_uid;
    let now = args.now;
    let stream_model = model.clone();
    tokio::spawn(async move {
        let mut translator =
            ResponsesStreamTranslator::new(&stream_model, &request_meta, &custom_names, created);
        let mut buf = String::new();
        let mut failure: Option<String> = None;
        let mut first_ms: Option<i64> = None;
        loop {
            match resp.chunk().await {
                Ok(Some(bytes)) => {
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    // SSE 以 \n 分行；\r\n 也要能切（上游偶有 CRLF）。
                    while let Some(pos) = buf.find('\n') {
                        let line = buf.drain(..=pos).collect::<String>();
                        for event in translator.feed(line.trim_end_matches('\r')) {
                            if tx
                                .send(Ok(Bytes::from(responses_sse_frame(&event))))
                                .await
                                .is_err()
                            {
                                // 客户端断开：停止搬运（对齐 Python 的 BrokenPipe 分支）
                                return;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    failure = Some(format!("stream aborted: {err}"));
                    break;
                }
            }
        }
        if failure.is_none() {
            // 末尾没有换行符的残留行
            if !buf.trim().is_empty() {
                for event in translator.feed(buf.trim()) {
                    if first_ms.is_none() {
                        first_ms = Some(((now_secs() - now) * 1000.0).round().max(0.0) as i64);
                    }

                    if tx
                        .send(Ok(Bytes::from(responses_sse_frame(&event))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            for event in translator.finish() {
                if first_ms.is_none() {
                    first_ms = Some(((now_secs() - now) * 1000.0).round().max(0.0) as i64);
                }

                if tx
                    .send(Ok(Bytes::from(responses_sse_frame(&event))))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        } else {
            // Python 的流异常路径：补一个 [DONE] 收尾（:18030-18035）
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
        }
        // 记账（流结束后才有 usage）
        let usage = translator.usage();
        record_chat_usage(
            &state,
            UsageContext {
                model: &model,
                payload: &client_payload,
                usage: usage.as_ref(),
                account: account_uid.as_deref(),
                realm: &state.cfg.realm,
                key: "",
                status: failure.as_ref().map(|_| 502),
                message: failure.as_deref(),
                started: now,
                or_models: None,
                ttft_ms: first_ms,
                gen_ms: None,
            },
        );
    });
    let mut response = Response::new(axum::body::Body::from_stream(ReceiverStream { rx }));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// 把 tokio 的 mpsc 接收端包成 `futures_core::Stream`。
///
/// axum 的 `Body::from_stream` 要求流实现 `futures_core::Stream`，而 tokio 的
/// `Receiver` 自己不实现它（tokio 有自己的 `tokio::stream::Stream`），故此处
/// 手写一层薄适配（`Receiver::poll_recv` 是 tokio 1.x 的公开 API）。
struct ReceiverStream {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

impl futures_core::Stream for ReceiverStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.rx.poll_recv(cx)
    }
}

/// Anthropic 侧入口（`/v1/messages`、`/messages`）：messages → chat → 回
/// messages 形状。**count_tokens 不在此入口**（不占槽，走 `count_tokens_entry`）。
async fn chat_anthropic_entry(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return preflight().await;
    }
    if unauthorized(&headers, &params, &state) {
        return anthropic_error(401, "missing or invalid API key", "authentication_error");
    }
    let raw = match parse_body(&body) {
        Ok(v) => v,
        Err(_) => return anthropic_error(400, "invalid request body", "invalid_request_error"),
    };
    let payload = match messages_to_chat(&raw, None) {
        Ok(v) => v,
        Err(msg) => {
            return anthropic_error(400, &msg, "invalid_request_error");
        }
    };
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();

    let Some(_permit) = acquire_slot(&state).await else {
        return overload_response(true);
    };

    let session_key = extract_session_key(&headers, &payload);
    let now = now_secs();
    let opts = body_options(&state, &payload, now);
    let ChatRun {
        outcome,
        account_uid,
    } = run_chat(
        &state.upstream,
        ChatParams {
            realm: &state.cfg.realm,
            model: Some(&model),
            session_key: session_key.as_deref(),
            conversation_id: session_key.as_deref(),
            payload: &payload,
            opts,
            base_url: state.cfg.upstream_base_url.as_deref(),
            auto_switch: true,
            egress_backoff: false,
            prompt_mode: "chat",
            now,
            utc_offset: UTC_OFFSET_SECS,
        },
    )
    .await;

    let reply = match outcome {
        Outcome::Served(resp, _) => match read_chat_json(resp, &model, now).await {
            Ok(chat) => {
                let usage = chat.get("usage").cloned();
                ChatReply {
                    response: Json(chat_to_messages(&chat)).into_response(),
                    usage,
                    status: None,
                    message: None,
                }
            }
            Err(resp) => ChatReply {
                response: *resp,
                usage: None,
                status: Some(502),
                message: Some("upstream stream error".to_string()),
            },
        },
        Outcome::Rejected { status, body } => {
            // 上游拒绝已是 Anthropic 形状（retry.rs 产出），直接透传。
            let message = body
                .get("error")
                .and_then(|err| err.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("upstream rejected with status {status}"));
            ChatReply {
                response: (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(body),
                )
                    .into_response(),
                usage: None,
                status: Some(status),
                message: Some(message),
            }
        }
    };

    record_chat_usage(
        &state,
        UsageContext {
            model: &model,
            payload: &payload,
            usage: reply.usage.as_ref(),
            account: account_uid.as_deref(),
            realm: &state.cfg.realm,
            key: "",
            status: reply.status,
            message: reply.message.as_deref(),
            started: now,
            or_models: None,
            ttft_ms: None,
            gen_ms: None,
        },
    );
    reply.response
}

/// 给一次 chat 记一行用量（关卡 4.19 线 C）。
///
/// **记账失败不影响原响应**（落盘失败只打日志，Python `_persist_usage`
/// 同款）。成功行记 `completed`，失败行记 `failed` + `status`。
fn record_chat_usage(state: &AppState, mut ctx: UsageContext<'_>) {
    // 从 PriceRefresher 取 OpenRouter 索引，供 ensure_policy / current_policy_id 使用。
    let or_models = state
        .task
        .price_refresher
        .get()
        .and_then(|p| p.last_or_models());
    ctx.or_models = or_models;
    crate::chat::record_usage(&state.task.usage_dir, &ctx);
}

/// 读并聚合上游 SSE 成 chat JSON（Anthropic 路径复用）。
/// `Err` 侧装箱理由同 `parse_body`。
async fn read_chat_json(
    resp: reqwest::Response,
    model: &str,
    now: f64,
) -> Result<Value, Box<Response>> {
    let text = resp.text().await.map_err(|err| {
        Box::new(anthropic_error(
            502,
            &format!("upstream stream error: {err}"),
            "api_error",
        ))
    })?;
    let payloads: Vec<&str> = text.lines().map(strip_data_prefix).collect();
    Ok(aggregate_stream(&payloads, model, "", now))
}

/// count_tokens 入口（Python 早于并发闸，故**不占槽**）。
async fn count_tokens_entry(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return preflight().await;
    }
    if unauthorized(&headers, &params, &state) {
        return anthropic_error(401, "missing or invalid API key", "authentication_error");
    }
    let raw = match parse_body(&body) {
        Ok(v) => v,
        Err(_) => return anthropic_error(400, "invalid request body", "invalid_request_error"),
    };
    let payload = match messages_to_chat(&raw, None) {
        Ok(v) => v,
        Err(msg) => {
            return anthropic_error(400, &msg, "invalid_request_error");
        }
    };
    let tokens = wb_gateway::anthropic::estimate_chat_tokens(&payload);
    Json(json!({ "input_tokens": tokens })).into_response()
}

/// 取并发槽；超时/超限返回 None（调用方转 503）。
async fn acquire_slot(state: &AppState) -> Option<wb_gateway::pool::PermitGuard> {
    let ready = state
        .upstream
        .count_ready(&state.cfg.realm, None, now_secs())
        .await;
    state.gate.resize(ready);
    state.gate.acquire().await.ok()
}

/// 组装 `BodyOptions`。M3 影子实例无 settings 面板接入，取 gateway 默认
/// （system_prompt 默认 + 非降级）；`degraded` 由 `DegradeGate` 状态决定。
fn body_options<'a>(state: &AppState, payload: &'a Value, now: f64) -> BodyOptions<'a> {
    let _ = (state, payload, now);
    BodyOptions::default()
}

/// 从 `HeaderMap` 取原始请求路径（`chat_openai_entry` 需区分 responses 路径）。
///
/// axum 不把 path 放进 `HeaderMap`；这里改用 `x-wb-path` 由中间件注入。
fn request_path(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-wb-path")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// 401 提示文案（Python `_authorized` 的 hint：区分「未提供」与「不匹配」）。
fn auth_hint(state: &AppState) -> String {
    if state.cfg.api_key.is_some() {
        "check the api key".to_string()
    } else {
        "missing Authorization header".to_string()
    }
}

/// `/realm` GET（Python `_get_realm`）：返回当前进程 realm 与可选列表。
async fn realm_get(State(state): State<AppState>) -> Response {
    let persisted = wb_storage::read_active_realm(&state.cfg.accounts_dir);
    Json(json!({
        "realm": state.cfg.realm,
        "persisted": persisted,
        "options": ["cn", "intl"],
    }))
    .into_response()
}

/// `/realm` POST（Python `_set_realm` + `save_persisted_realm`）：只落盘，
/// 进程内 realm 需重启才生效（与 Python 一致）。
async fn realm_post(State(state): State<AppState>, body: Bytes) -> Response {
    let raw = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let Some(realm) = raw.get("realm").and_then(|v| v.as_str()) else {
        return openai_error(400, "realm must be cn or intl", "invalid_request_error");
    };
    match wb_storage::save_realm(&state.cfg.accounts_dir, realm) {
        Ok(true) => Json(json!({"ok": true, "realm": realm, "persisted": true})).into_response(),
        Ok(false) => {
            Json(json!({"ok": true, "realm": state.cfg.realm, "persisted": false})).into_response()
        }
        Err(err) => openai_error(
            500,
            &format!("failed to persist realm: {err}"),
            "server_error",
        ),
    }
}

/// 复刻 Python `_get_health`（:15794-15815）：始终 200；账号身份字段只在
/// 授权时出现（panel 会话 / API key / 无需鉴权三条路径，见 auth::key_ok）。
async fn health(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let cfg = &state.cfg;
    match wb_storage::load_snapshot(&cfg.accounts_dir, cfg.api_key.as_deref(), cfg.lan) {
        Ok(snapshot) => {
            let mut body = json!({
                "ok": true,
                "realm": snapshot.realm,
                "accounts": snapshot.accounts,
                "accounts_ready": snapshot.accounts_ready,
                "api_key_required": snapshot.api_key_required,
            });
            if key_ok(&headers, &params, cfg, &state.panels) {
                apply_identity(&mut body, &snapshot.realm, cfg);
            }
            Json(body).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {
                "message": err.to_string(), "type": "server_error", "code": 500
            }})),
        )
            .into_response(),
    }
}

/// 授权后补身份字段（对齐 _get_health 的 info.update）。代表账号从 realm
/// 过滤后的池中取；池空时五个字段全部为 null（Python：rep 为 None）。
fn apply_identity(body: &mut Value, realm: &str, cfg: &Config) {
    const IDENTITY_FIELDS: [&str; 5] = ["uid", "domain", "issuer", "credential_file", "expires_at"];
    let Some(map) = body.as_object_mut() else {
        return;
    };
    match wb_storage::representative(&cfg.accounts_dir, realm) {
        Some(rep) => {
            map.insert("uid".into(), json!(rep.uid));
            map.insert("domain".into(), json!(rep.domain));
            map.insert(
                "issuer".into(),
                json!(wb_identity::jwt::jwt_issuer(&rep.access_token)),
            );
            map.insert("credential_file".into(), json!(rep.credential_file));
            map.insert("expires_at".into(), json!(rep.expires_at));
        }
        None => {
            for key in IDENTITY_FIELDS {
                map.insert(key.into(), Value::Null);
            }
        }
    }
}

/// 与 Python `_dashboard` 一致的静态面板：注入语言占位、发 ETag，
/// 命中 If-None-Match 时回 304 空体。
async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let cfg = &state.cfg;
    let body = match tokio::fs::read(&cfg.dashboard).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {
                    "message": format!("dashboard.html unavailable: {err}"),
                    "type": "server_error", "code": 500
                }})),
            )
                .into_response()
        }
    };
    let language = wb_storage::ui_language(&cfg.accounts_dir);
    let html = String::from_utf8_lossy(&body).replace(
        "data-ui-language=\"__WB_UI_LANGUAGE__\"",
        &format!("data-ui-language=\"{language}\""),
    );

    let etag = match tokio::fs::metadata(&cfg.dashboard).await {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| format!("\"{:x}-{:x}-{}\"", since.as_nanos(), meta.len(), language)),
        Err(_) => None,
    };

    if let (Some(tag), Some(supplied)) = (etag.as_deref(), headers.get(header::IF_NONE_MATCH)) {
        if supplied.to_str().ok() == Some(tag) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }

    let mut response = (StatusCode::OK, html).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Some(tag) = etag {
        if let Ok(value) = HeaderValue::from_str(&tag) {
            response.headers_mut().insert(header::ETAG, value);
        }
    }
    response
}

/// 生产面板 sink：把后台线程/路由日志写入 `wb_tasks::logs` 缓冲（/logs 面）。
#[derive(Clone, Default)]
pub struct ApiPanelSink;

impl PanelSink for ApiPanelSink {
    fn add_log_entry(&self, msg: &str, tag: &str) {
        wb_tasks::logs::append(msg, None, Some(tag));
    }
}

// ---------------------------------------------------------------------------
// 面板状态（对齐 Python `wb_proxy.py:15781 _get_panel_status`）
// ---------------------------------------------------------------------------

/// 面板状态：面板靠它决定是否显示登录闸门。
///
/// 这条**不在** Python 的面板路由前缀表里（`_is_panel_route` 不含 `/panel`），
/// 所以不需要会话就能问。返回里没有秘密——只有「有没有设 key」「密码是不是默认
/// 值」这类布尔（Python 注释同义）。
async fn panel_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get("x-panel-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let api_key_set =
        wb_storage::effective_api_key(&state.cfg.accounts_dir, state.cfg.api_key.as_deref())
            .is_some();
    Json(json!({
        "panel_password_required": true,
        "panel_password_is_default": panel_password_is_default(&state.cfg.accounts_dir),
        "authenticated": state.panels.valid(&token),
        "api_key_set": api_key_set,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// 面板认证三条路由 + 设置两条路由（对齐 Python `wb_proxy.py:17032 _handle_panel`
// 与 `:16112 _get_settings` / `:16401 _handle_settings_save`）
// ---------------------------------------------------------------------------

/// 面板路由的请求体解析（Python `_read_payload` + `_payload_or_error`）。
///
/// - 空体 ⇒ `{}`（Python `:16298`：`raw = ... if length else "{}"`）。
/// - 坏 JSON ⇒ 400 `"invalid JSON body"`（Python `:16331-16333`，**逐字**）。
/// - **非对象** JSON（数组/标量）⇒ `{}`（Python `:16323-16328`：`allow_list=False`
///   时非 dict 一律返回 `{}`，不报错）。
fn panel_payload(body: &Bytes) -> Result<Value, Box<Response>> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) => Ok(json!({})),
        Err(_) => Err(Box::new(openai_error(
            400,
            "invalid JSON body",
            "invalid_request_error",
        ))),
    }
}

/// Python `str(payload.get(key) or "")` 的等价物。
///
/// Python 的 `or ""` 把 `None` / `False` / `0` / `""` 归一成空串，其余按
/// `str()` 转文本（`True` → `"True"`，数字 → 十进制文本）。
fn python_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => {
            if *flag {
                "True".to_string()
            } else {
                String::new()
            }
        }
        Some(Value::Number(number)) => {
            if number.as_f64() == Some(0.0) {
                String::new()
            } else {
                number.to_string()
            }
        }
        // 数组/对象当口令没有意义；Python 的 `str()` 给的是 Python repr，
        // 这里给 JSON 文本（行为不同但两种情况下都会被口令校验拒绝）。
        Some(other) => other.to_string(),
    }
}

/// 面板登录（Python `_handle_panel` 的 `/panel/login` 分支 `:17036-17064`）。
///
/// 语义逐条照抄：
/// - **先限流后验口令**：窗口 60s 内同一 IP 已有 5 次失败 ⇒ 直接 429，
///   不验口令、**不**再计数（Python `:17042-17047`）。
/// - 口令错 ⇒ 记一次失败 + 退避 0.5s 再回 401（Python `:17049-17057`）。
///   Rust 侧退避用 `tokio::time::sleep`，不占 worker 线程。
/// - 口令对 ⇒ 清零该 IP 的失败计数 + 发新 token（Python `:17058-17064`）。
async fn panel_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
    body: Bytes,
) -> Response {
    let payload = match panel_payload(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    // 限流键不能直接取对端地址：反代后对端是代理本身（Python
    // `login_rate_limit_key` :380-397）。拿不到对端地址时退化成 `"unknown"`
    // —— 此时所有这类客户端共用一个限流桶（保守方向：更容易被限流）。
    let peer_ip = peer
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let client_key = LoginLimiter::client_key(&peer_ip, &headers, &state.panel_cfg.trusted_proxies);
    let now = now_secs();
    if let Some(wait) = state.login_limiter.is_blocked(&client_key, now) {
        return openai_error(
            429,
            &format!("too many login attempts, please wait {wait}s"),
            "rate_limit_error",
        );
    }
    let password = python_str(payload.get("password"));
    if !verify_panel_password(&state.cfg.accounts_dir, &password) {
        state.login_limiter.record_failure(&client_key, now);
        tokio::time::sleep(state.panel_cfg.login_backoff).await;
        return openai_error(401, "invalid panel password", "invalid_request_error");
    }
    state.login_limiter.clear(&client_key);
    let token = state.panels.create();
    Json(json!({
        "ok": true,
        "token": token,
        "using_default_password": panel_password_is_default(&state.cfg.accounts_dir),
    }))
    .into_response()
}

/// 面板登出（Python `_handle_panel` 的 `/panel/logout` 分支 `:17063-17065`）。
///
/// 撤销请求里带来的那个 token（带了才撤），**无条件**回 `{"ok": true}`
/// —— 不需要已有会话（没有 token 时就是空操作）。
async fn panel_logout(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // Python 的 `_handle_panel` 先读体再分发（`:17034-17036`），所以坏 JSON
    // 在这条路由上也是 400。
    if let Err(response) = panel_payload(&body) {
        return *response;
    }
    state.panels.revoke(&panel_token(&headers));
    Json(json!({"ok": true})).into_response()
}

/// 面板改口令（Python `_handle_panel` 的 `/panel/password` 分支 `:17066-17085`）。
///
/// - 必须有**有效面板会话**，否则 401「panel password required」。
/// - 当前口令不对 ⇒ 401「current password is wrong」。
/// - 新口令不足 4 个**字符** ⇒ 400（Python `len()` 数的是码点，故用
///   `chars().count()` 而不是字节长度）。
/// - 改完口令后：新口令不是默认值 ⇒ 撤销**所有**会话（Python `:17078-17081`）；
///   无论是否撤销都发一个新 token，让当前浏览器用新 token 继续用
///   （Python `:17082-17085`）。
async fn panel_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload = match panel_payload(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if !panel_session_ok(&headers, &state) {
        return panel_unauthorized();
    }
    let current = python_str(payload.get("current"));
    let new_password = python_str(payload.get("new"));
    if !verify_panel_password(&state.cfg.accounts_dir, &current) {
        return openai_error(401, "current password is wrong", "invalid_request_error");
    }
    if new_password.chars().count() < 4 {
        return openai_error(
            400,
            "new password must be at least 4 characters",
            "invalid_request_error",
        );
    }
    if let Err(err) = set_panel_password(&state.cfg.accounts_dir, &new_password) {
        return openai_error(
            500,
            &format!("could not set panel password: {err}"),
            "server_error",
        );
    }
    if new_password != DEFAULT_PANEL_PASSWORD {
        state.panels.revoke_all();
    }
    let token = state.panels.create();
    Json(json!({"ok": true, "token": token})).into_response()
}

/// 当前生效的 API key（供 `runtime_settings_view` 用）。
///
/// Python 侧是模块全局 `API_KEY`：启动时 = env `API_KEY` > 面板存的 key
/// （`wb_proxy.py:18722-18737`），面板改过 key 后**立即**变成新值
/// （`:16709-16711`）。Rust 侧没有可变全局，这里每次请求按 settings 文档里的
/// `api_key_set` 重算，逼近 Python 的可见行为。
///
/// ⚠️ 与 Python 的**唯一**差异：env `API_KEY` 存在时，Python 启动后仍优先用
/// env 那个（面板改过 key 也一样，直到进程重启），而这里只要文档里
/// `api_key_set=true` 就用面板存的那个。
fn effective_api_key(state: &AppState) -> Option<String> {
    let Ok(doc) = wb_storage::read_settings(&state.cfg.accounts_dir) else {
        return state.cfg.api_key.clone();
    };
    if doc.get("api_key_set").and_then(Value::as_bool) == Some(true) {
        return Some(
            doc.get("api_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    state.cfg.api_key.clone()
}

/// 读面板设置（Python `_get_settings` :16112-16116）。
///
/// 门禁：Python 侧 `/settings` 命中面板前缀表 ⇒ 外层要面板会话；handler 内部
/// 再查 `_authorized()`。Rust 侧同样走双层门禁。
async fn settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/settings", &headers, &params, &state) {
        return response;
    }
    let view = runtime_settings_view(
        &state.cfg.accounts_dir,
        &state.task.usage_dir,
        effective_api_key(&state).as_deref(),
    );
    Json(view).into_response()
}

/// 保存面板设置（Python `_handle_settings_save` :16401-16725）。
///
/// 门禁：**只认面板会话**（Python `:18264-18267` 明确 `if not self._panel_ok()`），
/// API key 不能走这条路。
///
/// 回包：Python 最后一行的 `reply.update(runtime_settings_view())` ⇒
/// **视图覆盖增量**；键序与 Python 的 `dict.update` 一致（reply 里已有的键
/// 保持原位、视图独有的键追加到末尾）。
async fn settings_save(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = panel_gate("/settings/save", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match panel_payload(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let (_doc, reply) = match save_settings(&state.cfg.accounts_dir, &payload) {
        Ok(pair) => pair,
        Err((code, message)) => {
            let err_type = if code >= 500 {
                "server_error"
            } else {
                "invalid_request_error"
            };
            return openai_error(code, &message, err_type);
        }
    };
    apply_settings_side_effects(&state, &reply);
    let view = runtime_settings_view(
        &state.cfg.accounts_dir,
        &state.task.usage_dir,
        effective_api_key(&state).as_deref(),
    );
    // `reply.update(view)`：视图覆盖增量，键序与 Python 的 dict.update 一致。
    let mut body = reply;
    if let (Some(body_map), Some(view_map)) = (body.as_object_mut(), view.as_object()) {
        for (key, value) in view_map {
            body_map.insert(key.clone(), value.clone());
        }
    }
    Json(body).into_response()
}

/// 设置保存后的运行时副作用（Python `_handle_settings_save` 的副作用分支）。
///
/// Python 侧共 12 处副作用；Rust 侧能落地的是定价刷新与更新检查两组：
/// - `pricing_enabled` ⇒ `PRICING.wake()`（Python `:16506-16513`）
/// - `pricing_refresh_minutes` ⇒ `PRICING.set_interval(minutes)`（Python `:16530-16534`）
/// - `pricing_variant_inherit` **且值确实变了** ⇒ 立刻起一次取价
///   （Python `:16575-16583`）。Rust 侧用 `tokio::spawn` **不阻塞**应答。
/// - `update_check_enabled` ⇒ `UPDATES.wake()`（Python `:16679-16683`）
/// - `credits_refresh_hours` ⇒ `CREDITS_REFRESHER.wake()`（Python `:16550-16557`）
///   —— Rust 侧 `CreditsRefresher` **没有** `wake()` 方法（只有 `start`/`stop`/
///   `run_once`），此处**跳过**，见交付报告的「待补」清单。
/// - `restart_scheduler` ⇒ 调度器 stop+start —— 线 B 把该键标成不支持（400），
///   故这里不处理。
///
/// ⚠️ 后台线程默认**不启动**（需 `WB_BACKGROUND_TASKS=1`，见
/// `start_background_tasks`），所以这些 `wake()` 在默认配置下是空操作 ——
/// 这是有意的（D49/D66），不要改。
fn apply_settings_side_effects(state: &AppState, reply: &Value) {
    let Some(reply_map) = reply.as_object() else {
        return;
    };
    if reply_map.contains_key("pricing_enabled") {
        if let Some(pricing) = state.task.price_refresher.get() {
            pricing.wake();
        }
    }
    if let Some(minutes) = reply_map
        .get("pricing_refresh_minutes")
        .and_then(Value::as_f64)
    {
        if let Some(pricing) = state.task.price_refresher.get() {
            pricing.set_interval_minutes(minutes);
        }
    }
    if reply_map.contains_key("pricing_variant_inherit")
        && reply_map
            .get("pricing_refresh_started")
            .and_then(Value::as_bool)
            == Some(true)
    {
        if let Some(pricing) = state.task.price_refresher.get() {
            let pricing = Arc::clone(pricing);
            // 不阻塞应答：Python 也是起后台线程再继续（`:16578-16583`）。
            tokio::spawn(async move {
                pricing.run_once().await;
            });
        }
    }
    if reply_map.contains_key("update_check_enabled") {
        if let Some(updater) = state.task.update_checker.get() {
            updater.wake();
        }
    }
}

// ---------------------------------------------------------------------------
// 任务路由 handlers（关卡 4.10）
// ---------------------------------------------------------------------------

async fn accounts_checkin(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/accounts/checkin", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_accounts_checkin(&state.task, &payload).await;
    Json(res).into_response()
}

/// 手动每日活跃打卡（对齐 Python `_route_accounts_daily_chat`）：完整打卡，
/// 桌面腿 + 按设置决定要不要再跑网页腿。
async fn accounts_daily_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/accounts/daily-chat", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_accounts_daily_chat(&state.task, &payload).await;
    Json(res).into_response()
}

/// 手动**网页通道**打卡（对齐 Python `_route_accounts_daily_chat_web`）：只建
/// 网页端会话，**不写** `lastDailyChat`。
async fn accounts_daily_chat_web(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate(
        "/accounts/daily-chat-web",
        &headers,
        &HashMap::new(),
        &state,
    ) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_accounts_daily_chat_web(&state.task, &payload).await;
    Json(res).into_response()
}

/// 账户管理路由的统一入口（关卡 4.24 线 C）。
///
/// 对齐 Python `_handle_accounts`（wb_proxy.py:17087-17138）的「按子路径 dispatch」
/// 风格：17 条 `/accounts/*` POST 路由都注册到这个 handler，靠 `inject_path`
/// 中间件写进 `x-wb-path` 头的原始路径区分子路由，再 dispatch 到
/// wb_tasks::api_routes::route_accounts_*（线 B 实现）。
///
/// 门禁与其它面板路由一致：`panel_gate` 双层（外层面板会话 + 内层 API key）。
///
/// ⚠️ 线 B（api_routes.rs 的 route_accounts_* 实现）尚未就绪：当前各分支先回
/// 「not implemented」占位信封，编译期可过；线 B 落地后把对应分支替换成
/// `task_routes::route_accounts_xxx(&state.task, &payload).await` 即可。
async fn accounts_mgmt(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let path = request_path(&headers).unwrap_or_default();
    if let Some(response) = panel_gate(&path, &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = match path.as_str() {
        "/accounts/credits" | "/accounts/credits/fetch" => {
            task_routes::route_accounts_credits_fetch(&state.task, &payload).await
        }
        "/accounts/credits/detail" => {
            task_routes::route_account_credits_detail(&state.task, &payload).await
        }
        "/accounts/credits/grants" => {
            task_routes::route_accounts_credits_grants(&state.task, &payload).await
        }
        "/accounts/delete" => task_routes::route_accounts_delete(&state.task, &payload).await,
        "/accounts/export" => task_routes::route_accounts_export(&state.task, &payload).await,
        "/accounts/import" => task_routes::route_accounts_import(&state.task, &payload).await,
        "/accounts/import/desktop" => {
            task_routes::route_accounts_import_desktop(&state.task, &payload).await
        }
        "/accounts/login/start" => {
            task_routes::route_accounts_login_start(&state.task, &payload).await
        }
        "/accounts/login/poll" => {
            task_routes::route_accounts_login_poll(&state.task, &payload).await
        }
        "/accounts/login/cancel" => {
            task_routes::route_accounts_login_cancel(&state.task, &payload).await
        }
        "/accounts/product" => task_routes::route_accounts_product(&state.task, &payload).await,
        "/accounts/refresh" => task_routes::route_accounts_refresh(&state.task, &payload).await,
        "/accounts/set" => task_routes::route_accounts_set(&state.task, &payload).await,
        "/accounts/set-all" => task_routes::route_accounts_set_all(&state.task, &payload).await,
        "/accounts/sync-profile" => {
            task_routes::route_accounts_sync_profile(&state.task, &payload).await
        }
        "/accounts/test" => task_routes::route_accounts_test(&state.task, &payload).await,
        _ => {
            return openai_error(404, "not found", "invalid_request_error");
        }
    };
    Json(res).into_response()
}

async fn tasks_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/tasks", &headers, &params, &state) {
        return response;
    }
    let uid = params.get("uid").cloned();
    let res = task_routes::route_tasks_get(&state.task, uid.as_deref()).await;
    Json(res).into_response()
}

async fn tasks_run(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = panel_gate("/tasks/run", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_tasks_run(&state.task, &payload).await;
    Json(res).into_response()
}

async fn tasks_travel(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = panel_gate("/tasks/travel", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_tasks_travel(&state.task, &payload).await;
    Json(res).into_response()
}

async fn tasks_streak_bonus(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/tasks/streak-bonus", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_tasks_streak_bonus(&state.task, &payload).await;
    Json(res).into_response()
}

async fn tasks_claim_all(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/tasks/claim-all", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !payload.is_object() {
        return openai_error(400, "expected a JSON object", "invalid_request_error");
    }
    let res = task_routes::route_tasks_claim_all(&state.task, &payload).await;
    Json(res).into_response()
}

async fn scheduler_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = panel_gate("/scheduler", &headers, &HashMap::new(), &state) {
        return response;
    }
    let res = task_routes::route_scheduler_get(&state.task);
    Json(res).into_response()
}

/// 后台积分刷新器的状态（关卡 4.18）。
///
/// 形状与 Python `CreditsRefresher.status()`（`wb_accounts.py:2578-2586`）
/// 逐字一致：六个键 `ttl_hours / interval_seconds / last_run / last_uid /
/// last_error / logs`。
async fn credits_refresher_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = panel_gate(
        "/accounts/credits-refresher",
        &headers,
        &HashMap::new(),
        &state,
    ) {
        return response;
    }
    let res = task_routes::route_credits_refresher_status(&state.task);
    Json(res).into_response()
}

/// 后台定价刷新器的状态（关卡 4.19）。
///
/// 形状与 Python `PRICING.status()`（`wb_pricing.py:2511-2554`）一致。
async fn pricing_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = panel_gate("/pricing", &headers, &HashMap::new(), &state) {
        return response;
    }
    let res = task_routes::route_pricing_status(&state.task);
    Json(res).into_response()
}

/// 后台更新检查器的状态（关卡 4.19）。
///
/// 形状与 Python `UPDATES.status()`（`wb_updates.py:257-271`）一致。
async fn updates_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = panel_gate("/updates", &headers, &HashMap::new(), &state) {
        return response;
    }
    let res = task_routes::route_updates_status(&state.task);
    Json(res).into_response()
}

async fn scheduler_trigger(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/scheduler/trigger", &headers, &HashMap::new(), &state) {
        return response;
    }
    let _ = body;
    let res = task_routes::route_scheduler_trigger(&state.task);
    Json(res).into_response()
}

async fn scheduler_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/scheduler/toggle", &headers, &HashMap::new(), &state) {
        return response;
    }
    let _ = body;
    let res = task_routes::route_scheduler_toggle(&state.task);
    Json(res).into_response()
}

// ---------------------------------------------------------------------------
// 用量路由（关卡 4.19 线 C）
//
// Python 侧对应 `_get_v1_usage`（:15839）、`_get_usage_by_account`（:16015）
// 与 `_get_usage_analytics`（:16003）。三者都在 `_is_panel_route` 的管辖下
// （`/usage` 前缀，:15662-15691）⇒ **需要鉴权**；鉴权模式与 `/scheduler`
// 一样：`key_ok` 走 API key / 面板会话 / 免鉴权三条路径。
//
// ⚠️ Python 还把 `/v1/usage` 也指向同一份快照（:15719），Rust 侧自关卡
// 4.20 起两条路径挂同一个 handler ⇒ 同一份响应体。
// ---------------------------------------------------------------------------

/// 读用量行。
///
/// **读失败降级成空集**（只打日志）：面板每几秒轮询一次，一次 IO 抖动不该
/// 让整个页面 500。**日志不存在不是错误** —— 新装的实例回一份空聚合。
fn read_usage_rows(state: &AppState) -> Vec<Value> {
    match wb_tasks::usage::load_usage_rows(&state.task.usage_dir) {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!("[wb-api] 读用量日志失败: {err}");
            Vec::new()
        }
    }
}

/// 把聚合结果包装成带 ETag 的响应：命中 `If-None-Match` ⇒ 304 空体，
/// 否则附上 `ETag` 头返回 JSON。
fn usage_etag_response(headers: &HeaderMap, value: serde_json::Value) -> Response {
    let etag = wb_tasks::usage::usage_etag(&value);
    if let Some(supplied) = headers.get(header::IF_NONE_MATCH) {
        if supplied.to_str().ok() == Some(etag.as_str()) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }
    let mut response = Json(value).into_response();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, v);
    }
    response
}

/// `GET /usage` 与 `GET /v1/usage`：用量快照（Python `_get_v1_usage`）。
///
/// **两条路径同一份实现**（Python `wb_proxy.py:15719`：
/// `if path in ("/usage", "/v1/usage")` ⇒ 同一个 handler、同一个响应体）。
///
/// **按 realm 过滤**（Python `_get_v1_usage` :15839-15842）：
/// `?realm=` → `X-Realm` 头 → 当前 realm；`all` 表示不过滤。
///
/// **按区间过滤**（Python `range_query` + `range_window`）：`range` /
/// `since` / `until` 三个参数决定这次快照描述哪段窗口；缺省或认不出来的
/// `range` ⇒ **不过滤**（旧面板的老行为）。
async fn usage_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage", &headers, &params, &state) {
        return response;
    }
    let realm = realm_filter(&params, &headers, Some(&state.cfg.realm));
    let now = wb_tasks::activity::now_epoch();
    let (since, until) = resolve_window(&params, now);
    let rows: Vec<Value> = read_usage_rows(&state)
        .into_iter()
        .filter(|row| wb_tasks::usage::realm_matches(row, &realm))
        .filter(|row| wb_tasks::usage::row_in_window(row, since, until))
        .collect();
    let decor = snapshot_decor(&state, &realm, now).await;
    // 定价表（关卡 4.21 线 A）：读策略库里的最新一份快照。读不到 ⇒ 传
    // `None` ⇒ 成本相关的键**整键省略**（不是 0），与「无定价表」的口径一致。
    let policies = wb_tasks::pricing_store::load_policies(&state.task.usage_dir);
    let pricing = wb_tasks::pricing_store::load_pricing(&state.task.usage_dir);
    wb_tasks::pricing_store::set_usage_dir(&state.task.usage_dir);
    usage_etag_response(
        &headers,
        wb_tasks::usage::build_usage_snapshot_with_priced(
            &rows,
            &decor,
            &policies,
            pricing.as_ref(),
            true,
        ),
    )
}

/// 快照的装饰键（Python `_usage_snapshot_uncached` 追加的那一组）。
///
/// `accounts_map` / `account` 来自**账号池的真实状态**；拿不到的子键整个
/// 省略（`domain` / `issuer` Rust 侧没有 ⇒ 不写假值）。`usd_cny` 依赖定价
/// 表，Rust 侧本关不接定价 ⇒ 整个键省略。
async fn snapshot_decor(state: &AppState, realm: &str, now: f64) -> wb_tasks::usage::SnapshotDecor {
    let mut decor = wb_tasks::usage::SnapshotDecor {
        realm: realm.to_string(),
        log_file: wb_tasks::usage::usage_log_path(&state.task.usage_dir)
            .to_string_lossy()
            .into_owned(),
        accounts_map: None,
        account: None,
    };
    let pool = state.upstream.pool.lock().await;
    let mut accounts_map = serde_json::Map::new();
    for account in pool.iter_accounts() {
        // 昵称：Rust 的账号池结构体不携带昵称，Python 那边缺省也是空串。
        accounts_map.insert(
            account.uid.clone(),
            json!({"nickname": "", "realm": account.realm}),
        );
    }
    decor.accounts_map = Some(Value::Object(accounts_map));

    // 代表账号：Python `POOL.representative(realm=r)` —— 先挑有 token 的，
    // 没有就挑第一个候选；一个都没有时 Python 也给一个「空代表」（各字段是
    // 空串 / 0），Rust 侧同样给一个 ⇒ 这个键恒定存在。
    let candidates: Vec<&wb_gateway::pool::AccountState> = pool
        .iter_accounts()
        .filter(|account| realm.is_empty() || realm == "all" || account.realm == realm)
        .collect();
    let representative = candidates
        .iter()
        .find(|account| account.has_token)
        .or_else(|| candidates.first());
    let mut value = serde_json::Map::new();
    value.insert(
        "uid".into(),
        json!(representative
            .map(|account| account.uid.clone())
            .unwrap_or_default()),
    );
    value.insert(
        "credential_file".into(),
        json!(representative
            .map(|account| account.credential_file.clone())
            .unwrap_or_default()),
    );
    value.insert(
        "expires_at".into(),
        json!(representative
            .and_then(|account| account.expires_at)
            .unwrap_or(0.0)),
    );
    value.insert("accounts".into(), json!(pool.len()));
    value.insert(
        "accounts_ready".into(),
        json!(pool.count_ready(None, None, now)),
    );
    // Python 的 `account` 还有 `domain` / `issuer`：Rust 的账号池拿不到这两个
    // 值 ⇒ **省略这两个子键**，不写假的空串。
    decor.account = Some(Value::Object(value));
    decor
}

/// 解析面板的区间参数（Python `range_query` + `range_window`）。
///
/// 缺省 / 认不出来的 `range` ⇒ `(None, None)` = **不过滤**。
fn resolve_window(params: &HashMap<String, String>, now: f64) -> (Option<f64>, Option<f64>) {
    wb_tasks::usage::resolve_range_window(
        params.get("range").map(String::as_str).unwrap_or(""),
        params.get("since").map(String::as_str),
        params.get("until").map(String::as_str),
        now,
    )
}

/// 读一个整型查询参数（Python `int(...)` 的近似）。
///
/// 解析不了 ⇒ `None`（调用方按 Python 的兜底值处理）。Python 的
/// `int("12.5")` 抛 `ValueError` ⇒ 同样回 `None`。
fn parse_int(params: &HashMap<String, String>, key: &str) -> Option<i64> {
    params
        .get(key)
        .and_then(|raw| raw.trim().parse::<i64>().ok())
}

/// `GET /usage/recent`：分页的最近用量行（Python `_get_usage_recent`）。
///
/// `limit` 夹在 1..1000（默认 100）、`page` ≥ 1（默认 1）；解析不了回默认值。
///
/// ⚠️ 返回的**行没有定价字段**（`cost_cny` / `cost_band` / …）：那些依赖定价
/// 表，Rust 侧本关不接定价 ⇒ 整组省略，不写假的 0 值。
async fn usage_recent_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage/recent", &headers, &params, &state) {
        return response;
    }
    let realm = realm_filter(&params, &headers, Some(&state.cfg.realm));
    let limit = parse_int(&params, "limit")
        .map(|value| value.clamp(1, 1000))
        .unwrap_or(100);
    let page = parse_int(&params, "page")
        .map(|value| value.max(1))
        .unwrap_or(1);
    let rows = read_usage_rows(&state);
    let policies = wb_tasks::pricing_store::load_policies(&state.task.usage_dir);
    let pricing = wb_tasks::pricing_store::load_pricing(&state.task.usage_dir);
    wb_tasks::pricing_store::set_usage_dir(&state.task.usage_dir);
    usage_etag_response(
        &headers,
        wb_tasks::usage::build_usage_recent_priced(
            &rows,
            &realm,
            page,
            limit,
            &policies,
            pricing.as_ref(),
            true,
        ),
    )
}

/// `GET /usage/timeseries`：分桶的用量序列（Python `_get_usage_timeseries`）。
///
/// 桶粒度自适应（Python `_usage_timeseries_scope`）：窗口 ≤6h ⇒ 60s；
/// ≤14d ⇒ 3600s；更长 ⇒ 86400s；`?bucket=` 可覆盖（下限 60 秒）。
/// 缺省窗口是「最近 24 小时」。
async fn usage_timeseries_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage/timeseries", &headers, &params, &state) {
        return response;
    }
    let realm = realm_filter(&params, &headers, Some(&state.cfg.realm));
    let now = wb_tasks::activity::now_epoch();
    let (since, until) = resolve_window(&params, now);
    let until = until.unwrap_or(now);
    let since = since.unwrap_or(until - 86400.0);
    let span = (until - since).max(1.0);
    let step = wb_tasks::usage::pick_bucket_seconds(span, parse_int(&params, "bucket"));
    let rows = read_usage_rows(&state);
    Json(wb_tasks::usage::build_usage_timeseries(
        &rows, &realm, since, until, step,
    ))
    .into_response()
}

/// `GET /usage/perf`：延迟分位与派生速率（Python `_get_usage_perf`）。
///
/// `sample` 夹在 10..20000（默认 5000），指的是日志**末尾**多少行参与统计
/// （不是时间窗）。分位一律从原始样本算。
async fn usage_perf_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage/perf", &headers, &params, &state) {
        return response;
    }
    let realm = realm_filter(&params, &headers, Some(&state.cfg.realm));
    let sample = parse_int(&params, "sample")
        .map(|value| value.clamp(10, 20000))
        .unwrap_or(5000);
    let now = wb_tasks::activity::now_epoch();
    let (since, until) = resolve_window(&params, now);
    let rows = read_usage_rows(&state);
    usage_etag_response(
        &headers,
        wb_tasks::usage::build_usage_perf(&rows, &realm, sample, since, until),
    )
}

/// `GET /activity/history`：账号每日活动的结构化历史（Python
/// `_get_activity_history`）。
///
/// **筛选值非法 ⇒ 400**（不是静默忽略）：调用方拿着一个被忽略的
/// `result=failed` 会以为「这几天没有失败」，实际上它拿回的是全部结果。
async fn activity_history_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/activity/history", &headers, &params, &state) {
        return response;
    }
    let rows = wb_tasks::activity::history_rows(&state.task.usage_dir);
    let now = wb_tasks::activity::now_epoch();
    let outcome = wb_tasks::activity::query(
        &rows,
        params.get("range").map(String::as_str).unwrap_or(""),
        params.get("uid").map(String::as_str).unwrap_or(""),
        params.get("task").map(String::as_str).unwrap_or(""),
        params.get("result").map(String::as_str).unwrap_or(""),
        parse_int(&params, "limit"),
        now,
    );
    match outcome {
        Ok(result) => Json(result.to_json()).into_response(),
        Err(message) => openai_error(400, &message, "invalid_request_error"),
    }
}

/// `GET /usage/by-account`：按账号聚合（Python `_get_usage_by_account`）。
///
/// **不过滤 realm**（Python `_usage_by_account_uncached` :6104 从头到尾没有
/// realm 过滤：这份视图一直描述**整份日志**）。
async fn usage_by_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage/by-account", &headers, &params, &state) {
        return response;
    }
    let rows = read_usage_rows(&state);
    usage_etag_response(&headers, wb_tasks::usage::build_usage_by_account(&rows))
}

/// `GET /usage/analytics`：按日期分组的分析（Python `_get_usage_analytics`）。
///
/// **默认不过滤 realm**（Python `_get_usage_analytics` :16003-16006：
/// `req_realm = query.get("realm")[0] or None` ⇒ 缺省是 **None = 全 realm**
/// 合并统计）；给了 `?realm=` 才过滤。
async fn usage_analytics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage/analytics", &headers, &params, &state) {
        return response;
    }
    let realm = realm_filter(&params, &headers, None);
    let rows: Vec<Value> = read_usage_rows(&state)
        .into_iter()
        .filter(|row| wb_tasks::usage::realm_matches(row, &realm))
        .collect();
    let policies = wb_tasks::pricing_store::load_policies(&state.task.usage_dir);
    let pricing = wb_tasks::pricing_store::load_pricing(&state.task.usage_dir);
    wb_tasks::pricing_store::set_usage_dir(&state.task.usage_dir);
    usage_etag_response(
        &headers,
        wb_tasks::usage::build_usage_analytics_priced(&rows, &policies, pricing.as_ref(), true),
    )
}

/// 解析 realm 过滤条件（Python 的 `realm_scope` 语义）。
///
/// 优先级：`?realm=` → `X-Realm` 头 → `fallback`（`None` = 不过滤）。
/// `"all"` 与空串都表示「所有 realm」（Python `realm_scope` 把 `"all"`
/// 归一成 `None`）。
fn realm_filter(
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    fallback: Option<&str>,
) -> String {
    params
        .get("realm")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-realm")
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| fallback.map(str::to_string))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 关卡 4.25：logs / pricing 管理 / updates 管理 / settings reveal
// ---------------------------------------------------------------------------

/// `/logs`：面板日志列表（Python `_get_logs` :16136-16149）。
async fn logs_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/logs", &headers, &params, &state) {
        return response;
    }
    Json(task_routes::route_logs_get(&state.task, &params)).into_response()
}

/// `/logs/export`：导出纯文本日志（Python `_get_logs_export` :16152-16165）。
async fn logs_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/logs/export", &headers, &params, &state) {
        return response;
    }
    let text = task_routes::route_logs_export(&state.task, &params);
    let filename = format!(
        "wb-proxy-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let mut response = text.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// `/logs/clear`：清空日志缓冲（Python `_route_logs_clear` :17477-17479）。
async fn logs_clear(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = panel_gate("/logs/clear", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_logs_clear(&state.task, &payload)).into_response()
}

/// `/settings/reveal`：按 id 返回未脱敏 key（Python `_get_settings_reveal`
/// :16169-16178）。找不到 ⇒ 404 信封。
async fn settings_reveal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/settings/reveal", &headers, &params, &state) {
        return response;
    }
    let out = task_routes::route_settings_reveal(&state.task, &params);
    if out.is_null() {
        return openai_error(404, "no such key", "invalid_request_error").into_response();
    }
    Json(out).into_response()
}

/// `/pricing/mapping`：面板手填「hub 模型 → OpenRouter id」映射
/// （Python `_route_pricing_mapping` :17481-17520）。
async fn pricing_mapping(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/pricing/mapping", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let out = task_routes::route_pricing_mapping(&state.task, &payload).await;
    if let Some(status) = out.get("status").and_then(Value::as_i64) {
        return openai_error(
            status as u16,
            out["error"].as_str().unwrap_or("bad request"),
            "invalid_request_error",
        )
        .into_response();
    }
    Json(out).into_response()
}

/// `/pricing/refresh`：后台触发一次取价（Python `_route_pricing_refresh`
/// :17463-17476）。
async fn pricing_refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/pricing/refresh", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_pricing_refresh(&state.task, &payload).await).into_response()
}

/// `/updates/check`：立即检查一次发布（Python `_route_update_check` :17438-17450）。
async fn update_check(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = panel_gate("/updates/check", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_update_check(&state.task, &payload).await).into_response()
}

// ---------------------------------------------------------------------------
// 关卡 4.26：proxy slots / agents / remaining 接线层
// ---------------------------------------------------------------------------

async fn proxy_slots_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/proxy/slots", &headers, &params, &state) {
        return response;
    }
    Json(task_routes::route_proxy_slots_view(&state.task, &json!({}))).into_response()
}

async fn proxy_slots_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/proxy/slots/save", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_proxy_slots_save(&state.task, &payload).await).into_response()
}

async fn proxy_slots_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/proxy/slots/test", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_proxy_slots_test(&state.task, &payload).await).into_response()
}

async fn proxy_discover(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/proxy/discover", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_proxy_discover(&state.task, &payload)).into_response()
}

async fn agents_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/agents", &headers, &params, &state) {
        return response;
    }
    Json(task_routes::route_agents_view(&state.task, &params)).into_response()
}

async fn agents_available(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/agents/available", &headers, &params, &state) {
        return response;
    }
    Json(task_routes::route_agents_available(&state.task, &params)).into_response()
}

async fn agents_cli(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/agents/cli", &headers, &params, &state) {
        return response;
    }
    Json(task_routes::route_agents_cli(&state.task, &params)).into_response()
}

async fn agents_apply(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = panel_gate("/agents/apply", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_agents_apply(&state.task, &payload).await).into_response()
}

async fn agents_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = panel_gate("/agents/restore", &headers, &HashMap::new(), &state) {
        return response;
    }
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    Json(task_routes::route_agents_restore(&state.task, &payload)).into_response()
}

async fn usage_remaining_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = panel_gate("/usage/remaining", &headers, &params, &state) {
        return response;
    }
    Json(task_routes::route_usage_remaining(&state.task, &params).await).into_response()
}
