use std::collections::HashMap;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::auth::{key_ok, PanelSessions};
use crate::chat::{extract_session_key, run_chat, ChatParams, Upstream};
use crate::Config;

use wb_gateway::anthropic::{chat_to_messages, messages_to_chat};
use wb_gateway::body::BodyOptions;
use wb_gateway::pool::ConcurrencyGate;
use wb_gateway::responses::{chat_to_response, responses_to_chat, RequestMeta};
use wb_gateway::retry::{anthropic_error_obj, Outcome};
use wb_gateway::stream::{aggregate_stream, strip_data_prefix};
use wb_tasks::api_routes as task_routes;
use wb_tasks::scheduler::PanelSink;

/// 面板会话 TTL：7 天（对齐 wb_settings.SESSION_TTL）。
const SESSION_TTL: u64 = 7 * 24 * 3600;

/// 并发闸默认上限（Python `MAX_CONCURRENT_CHAT`，wb_settings 默认 32）。
const MAX_CONCURRENT_CHAT: usize = 32;

/// 槽位等待秒数（Python `CHAT_SLOT_WAIT_SECONDS`，env `WB_CHAT_SLOT_WAIT`）。
/// M3 影子实例不读 env（env 解析属 main.rs 职责），先用 Python 默认值 30。
const CHAT_SLOT_WAIT_SECONDS: f64 = 30.0;

/// 默认模型（Python `_dispatch_chat_post`：`payload.get("model") or "hy4-preview"`）。
const DEFAULT_MODEL: &str = "hy4-preview";

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
        }
    }
}

pub fn router(cfg: Arc<Config>) -> Router {
    app_with(
        cfg.clone(),
        Arc::new(PanelSessions::new(SESSION_TTL)),
        task_routes::TaskRuntime::production(&cfg.accounts_dir, Arc::new(ApiPanelSink)),
    )
}

pub fn app_with(
    cfg: Arc<Config>,
    panels: Arc<PanelSessions>,
    task: Arc<task_routes::TaskRuntime>,
) -> Router {
    let state = AppState::build(cfg, panels, task);
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard))
        .route("/realm", get(realm_get).post(realm_post))
        .route("/panel/status", get(panel_status).options(preflight))
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
        .route("/tasks", get(tasks_get).options(preflight))
        .route("/tasks/run", post(tasks_run).options(preflight))
        .route("/tasks/travel", post(tasks_travel).options(preflight))
        .route(
            "/tasks/streak-bonus",
            post(tasks_streak_bonus).options(preflight),
        )
        .route("/tasks/claim-all", post(tasks_claim_all).options(preflight))
        .route("/scheduler", get(scheduler_get).options(preflight))
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

/// 读上游 `reqwest::Response` 全文，逐行 `strip_data_prefix` 后聚合。
///
/// 对齐 Python `_chat_nonstream_response` :18584：上游回的是 **SSE 流**，
/// 非流式响应靠 `aggregate_stream` 把 SSE 事件拼成 chat JSON。
async fn aggregate_upstream(resp: reqwest::Response, model: &str, now: f64) -> Response {
    let text = match resp.text().await {
        Ok(t) => t,
        Err(err) => {
            return openai_error(
                502,
                &format!("upstream stream error: {err}"),
                "server_error",
            )
        }
    };
    let payloads: Vec<&str> = text.lines().map(strip_data_prefix).collect();
    let result = aggregate_stream(&payloads, model, "", now);
    (StatusCode::OK, Json(result)).into_response()
}

/// 把 `Outcome` 落到 HTTP 响应（非流式）。`Rejected` 直接透传上游状态与
/// 信封；`Served` 走 `aggregate_upstream`。
async fn outcome_to_response(outcome: Outcome, model: &str, now: f64) -> Response {
    match outcome {
        Outcome::Served(resp, _) => aggregate_upstream(resp, model, now).await,
        Outcome::Rejected { status, body } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(body),
        )
            .into_response(),
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
    let outcome = run_chat(
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

    let response = outcome_to_response(outcome, &model, now).await;
    if is_responses {
        wrap_response_response(response).await
    } else {
        response
    }
}

/// `chat_to_response` 包装（Python `_handle_responses` 的成功分支）。
///
/// 非流式：把上游聚合出的 chat JSON 转成 Responses 形状；错误信封原样返回
/// （错误路径由上游状态决定，不再二次翻译）。失败时回落原响应。
async fn wrap_response_response(response: Response) -> Response {
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
    let meta = RequestMeta::default();
    let created = now_secs() as i64;
    let out = chat_to_response(&chat_obj, &model, &[], &meta, created);
    Response::from_parts(parts, axum::body::Body::from(out.to_string()))
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
    let outcome = run_chat(
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

    match outcome {
        Outcome::Served(resp, _) => {
            let chat = match read_chat_json(resp, &model, now).await {
                Ok(v) => v,
                Err(resp) => return *resp,
            };
            let out = chat_to_messages(&chat);
            Json(out).into_response()
        }
        Outcome::Rejected { status, body } => {
            // 上游拒绝已是 Anthropic 形状（retry.rs 产出），直接透传。
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(body),
            )
                .into_response()
        }
    }
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

/// 生产面板 sink：no-op（面板日志暂未接入 dashboard，M5 对齐）。
#[derive(Clone, Default)]
pub struct ApiPanelSink;

impl PanelSink for ApiPanelSink {
    fn add_log_entry(&self, _msg: &str, _tag: &str) {
        // no-op
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

/// 面板密码是否还是默认值（对齐 Python `wb_settings.panel_password_is_default`）。
///
/// settings.json 里没有 `panel_password_hash` ⇒ 从没改过 ⇒ 仍是默认值；改过之后
/// 只有 `panel_password_default == true` 才算「还是默认」。
fn panel_password_is_default(accounts_dir: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(accounts_dir.join("settings.json")) else {
        return true;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return true;
    };
    let Some(obj) = doc.as_object() else {
        return true;
    };
    let hash = obj
        .get("panel_password_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if hash.is_empty() {
        return true;
    }
    obj.get("panel_password_default").and_then(Value::as_bool) == Some(true)
}

// ---------------------------------------------------------------------------
// 任务路由 handlers（关卡 4.10）
// ---------------------------------------------------------------------------

async fn accounts_checkin(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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

async fn tasks_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if unauthorized(&headers, &params, &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
    }
    let uid = params.get("uid").cloned();
    let res = task_routes::route_tasks_get(&state.task, uid.as_deref()).await;
    Json(res).into_response()
}

async fn tasks_run(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
    }
    let res = task_routes::route_scheduler_get(&state.task);
    Json(res).into_response()
}

async fn scheduler_trigger(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
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
    if unauthorized(&headers, &HashMap::new(), &state) {
        return openai_error(
            401,
            &format!("invalid api key - {}", auth_hint(&state)),
            "invalid_request_error",
        );
    }
    let _ = body;
    let res = task_routes::route_scheduler_toggle(&state.task);
    Json(res).into_response()
}
