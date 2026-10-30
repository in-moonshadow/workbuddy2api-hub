//! 关卡 4.26（线 C）：agents 面板的**能力层**（纯视图 / 校验，不发外部写）。
//!
//! 权威源是 Python `wb_proxy.py` 的 agents 段与 `wb_agents.py`：
//!
//! - [`agents_cli_view`] —— `_get_agents_cli`（`:16880-16905`）：把仓库里的
//!   `wb_agents.py` 交给远端机器（CLI 分发）。
//! - [`agents_view`] —— `_get_agents`（`:16906-16935`）：智能体列表（客户端
//!   概览 + 模型目录 + 面板 key + 网关提示）。
//! - [`agents_available`] —— `/agents/available`（`:15773-15778`，issue #246）：
//!   看板启动时的廉价判定：不 import `wb_agents`、不做探测。
//! - [`agents_apply_validate`] —— `_handle_agents_apply`（`:16958-17015`）的
//!   **前半**：client / base_url / model / models 校验，models 清洗与 80 截断，
//!   key 解析。**不写外部文件**。
//! - [`agents_restore_plan`] —— `_handle_agents_restore`（`:17016-17030`）的
//!   校验 + 返回结构。**写客户端 config 文件的部分本关不实现**（标注）。
//!
//! ## 有意的边界
//!
//! 1. **写盘侧全部标注未做**：`wb_agents.integrate`（apply 后半）与
//!    `wb_agents.restore`（真正删文件 / 清 state）都属「集成引擎」，留接线层 /
//!
//!   后续关卡；本模块只产出「已清洗的 models + 校验结果」与 restore 的计划
//!   形状。`integrate` 里用 sha256 判断「外部改动」的检测因此也**未实现**，
//!   概览里的 `applied.external_change` 恒为 `false` 并标注。
//! 2. **`enabled` 的「回环」半判断不在本层**：Python 的 `agents_client_allowed`
//!    要求请求来源是 `127.0.0.1` / `::1`（`server_deployment_form` 的另一半）。
//!   那是个 per-request 属性，接线层拿到 `client_address` 才能算；本层
//!    [`agents_available`] 只算**部署形态**那半边（廉价、只读、无探测），
//!    [`agents_view`] 固定返回 `enabled: true` 的全量形状，接线层在请求非回环
//!    时自行回退到 [`AGENTS_DISABLED_REASON`] 的不可用分支。
//! 3. **models 只走静态目录**：Python `_agents_models` 先试网关 live 模型、
//!    失败回退静态目录；Rust 本关**只搬静态目录那半边**（
//!    [`crate::catalog::hub_model_ids`]），live 源留接线层/后续。
//! 4. **不返回 API key 明文**：`_agents_resolve_key` 解析出的 key 只用于给
//!    （未实现的）integrate 传参，本关把解析结果缩成 `key_status` 标记，
//!    不在返回值里携带 key 串 —— 谁也不能把本函数返回值直接当响应体转发。
//!
//! ## 键序
//!
//! `serde_json` 开了 `preserve_order`，JSON 键序刻意对齐 Python dict 字面量
//! （客户端概览按 `sorted(CLIENTS)` 升序）。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// Python `_get_agents` 里「不可用」分支的原因字符串（远端看板分支）。
pub const AGENTS_DISABLED_REASON: &str =
    "agent config is only available from the machine running the gateway";

/// 分发的 CLI 文件名（`/agents/cli` 响应的 `filename` 头）。
pub const WB_AGENTS_SCRIPT: &str = "wb_agents.py";

/// `wb_agents.STATE_FILE`：面板集成状态文件名（位于 accounts_dir 下）。
const STATE_FILE: &str = "integration-state.json";

/// Rust hub 网关默认端口（`crates/api/src/main.rs` 的 PORT 缺省值）。
const DEFAULT_PORT: u16 = 8789;

/// Python `_agents_models` 的静态目录回退分支：只含 `{"id": ...}`。
fn static_models() -> Vec<Value> {
    crate::catalog::hub_model_ids()
        .iter()
        .map(|id| json!({ "id": id }))
        .collect()
}

// ---------------------------------------------------------------------------
// 客户端静态元数据（wb_agents.py:CLIENTS 的只读投影）
// ---------------------------------------------------------------------------

/// 一个可一键配置的客户端（`wb_agents.CLIENTS` 条目的只读投影）。
struct ClientSpec {
    id: &'static str,
    label: &'static str,
    desc: &'static str,
    protocol: &'static str,
    /// 配置文件的基准目录（相对 home；环境变量覆盖时替换）。
    base_tail: &'static str,
    /// （角色, 相对 `base_tail` 的文件路径）。
    files: &'static [(&'static str, &'static str)],
    /// base 目录的环境变量覆盖（claude-code → CLAUDE_CONFIG_DIR，codex → CODEX_HOME）。
    base_env: Option<&'static str>,
    /// PATH 里存在的可执行名（探测信号之一；None 不查）。
    probe_bin: Option<&'static str>,
}

/// 六个客户端，顺序与 Python `sorted(CLIENTS)` 一致（概览键序）。
const CLIENTS: [ClientSpec; 6] = [
    ClientSpec {
        id: "claude-code",
        label: "Claude Code",
        desc: "Anthropic CLI; points ANTHROPIC_BASE_URL at this gateway.",
        protocol: "anthropic",
        base_tail: ".claude",
        files: &[("settings", "settings.json")],
        base_env: Some("CLAUDE_CONFIG_DIR"),
        probe_bin: Some("claude"),
    },
    ClientSpec {
        id: "codex",
        label: "Codex CLI",
        desc: "OpenAI Codex CLI; adds a wb-proxy model provider.",
        protocol: "openai",
        base_tail: ".codex",
        files: &[
            ("config", "config.toml"),
            ("auth", "auth.json"),
        ],
        base_env: Some("CODEX_HOME"),
        probe_bin: Some("codex"),
    },
    ClientSpec {
        id: "crush",
        label: "Crush",
        desc: "Charm Crush; adds a wb-proxy provider entry.",
        protocol: "openai",
        base_tail: ".config/crush",
        files: &[("config", "crush.json")],
        base_env: None,
        probe_bin: Some("crush"),
    },
    ClientSpec {
        id: "dsh",
        label: "DeepSeek Harness",
        desc: "DSH; adds a wb-proxy provider to settings.yaml.",
        protocol: "openai",
        base_tail: ".dsh",
        files: &[
            ("settings", "settings.yaml"),
            ("credentials", ".credentials.yaml"),
        ],
        base_env: None,
        probe_bin: Some("dsh"),
    },
    ClientSpec {
        id: "opencode",
        label: "OpenCode",
        desc: "OpenCode; registers an openai-compatible provider.",
        protocol: "openai",
        base_tail: ".config/opencode",
        files: &[("config", "opencode.json")],
        base_env: None,
        probe_bin: Some("opencode"),
    },
    ClientSpec {
        id: "zcode",
        label: "ZCode",
        desc: "ZCode; adds a wb-proxy provider to its provider config. Quit ZCode first - it rewrites that file while running.",
        protocol: "openai",
        base_tail: ".zcode",
        files: &[("config", "v2/provider_config.json")],
        base_env: None,
        // Python 对 zcode 的 probe 不做 which 检查（只认目录/文件）。
        probe_bin: None,
    },
];

/// 按 id 找客户端（`wb_agents._client` 的未知 id 分支返回 None）。
fn find_client(client_id: &str) -> Option<&'static ClientSpec> {
    CLIENTS.iter().find(|c| c.id == client_id)
}

// ---------------------------------------------------------------------------
// 小型工具
// ---------------------------------------------------------------------------

/// Python `bool()` 对 JSON 值的口径（falsy：Null / false / 0 / 空串 / 空数组 / 空对象）。
fn python_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(x or "")` 的标量口径：falsy → 空串；字符串原样；数字/True 转写；
/// 数组/对象（truthy 时 Python 给 repr）本关按空串处理并注明。
fn python_str_or(v: Option<&Value>) -> String {
    match v {
        Some(v) if python_truthy(v) => match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(true) => "True".to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// 进程 home 目录（Python `os.path.expanduser("~")`）。
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::new())
}

/// 客户端的基准目录（环境覆盖优先，空覆盖回退 home 下的 base_tail）。
fn client_base(client: &ClientSpec, home: &Path) -> PathBuf {
    if let Some(key) = client.base_env {
        if let Ok(raw) = std::env::var(key) {
            let v = raw.trim();
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    home.join(client.base_tail)
}

/// 客户端的全部配置文件路径（含不存在的，Python `client["files"](home)` 的 values）。
fn client_paths(client: &ClientSpec, home: &Path) -> Vec<PathBuf> {
    let base = client_base(client, home);
    client
        .files
        .iter()
        .map(|(_, tail)| base.join(*tail))
        .collect()
}

/// PATH 里是否存在可执行 `name`（`shutil.which` 的近似：只认普通文件）。
fn command_on_path(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':')
        .any(|dir| Path::new(dir).join(name).is_file())
}

/// 客户端「是否已安装」的探测（`wb_agents.CLIENTS[*]["probe"]`）。
///
/// 与 Python 对齐：基准目录存在 OR 任一配置文件存在 OR PATH 里有对应命令。
/// 可执行位检查（`shutil.which`）用「普通文件存在」近似，语义足够。
fn client_probe(client: &ClientSpec, home: &Path) -> bool {
    if client_base(client, home).is_dir() {
        return true;
    }
    if client_paths(client, home).iter().any(|p| p.is_file()) {
        return true;
    }
    client.probe_bin.map(command_on_path).unwrap_or(false)
}

/// 读 `accounts_dir/integration-state.json`（`wb_agents._load_state`）。
///
/// 文件缺失 / 非对象 → `{}`（Python 的裸 except 语义）。
fn load_integration_state(accounts_dir: &Path) -> Value {
    let path = accounts_dir.join(STATE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            _ => json!({}),
        },
        _ => json!({}),
    }
}

/// 面板 key 的只读视图（Python `_get_agents` 的 `keys` 段：只留 enabled）。
fn configured_keys_view(accounts_dir: &Path) -> Vec<Value> {
    wb_storage::api_keys(accounts_dir)
        .into_iter()
        .filter(|entry| entry.enabled)
        .map(|entry| {
            json!({
                // Python `entry.get("id")`：缺 id 时是 null，不是空串。
                "id": if entry.id.is_empty() { Value::Null } else { json!(entry.id) },
                "name": entry.name,
                "enabled": true,
            })
        })
        .collect()
}

/// 网关 base_url 提示（Python `_agents_base_url_hint` 的环境变量版）。
///
/// 从 HOST/PORT 环境推导（与 `crates/api/src/main.rs` 同口径）；`0.0.0.0` /
/// `::` 对客户端不可达，按 Python 逻辑改报 `127.0.0.1`。
fn gateway_base_url_hint() -> String {
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let host = if host.is_empty() || host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host.trim()
    };
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    format!("http://{host}:{port}/v1")
}

/// 服务器部署形态（Python `server_deployment_form`，`:98-124`）。
///
/// ⚠️ 与 Python 的两处差异：① 支持 `WB_SERVER_DEPLOYMENT` 环境覆盖（测试 /
/// 特殊部署注入）；② **不做进程级缓存**（Python 缓存，本实现每次现查——
/// 调用稀疏、读取廉价，且避免缓存把测试里的环境覆盖顶掉）。
fn server_deployment_form() -> bool {
    if let Ok(raw) = std::env::var("WB_SERVER_DEPLOYMENT") {
        let v = raw.trim();
        return v == "1" || v.eq_ignore_ascii_case("true");
    }
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return true;
    }
    if let Ok(cgroup) = std::fs::read_to_string("/proc/1/cgroup") {
        let low = cgroup.to_lowercase();
        if ["docker", "containerd", "kubepods"]
            .iter()
            .any(|m| low.contains(m))
        {
            return true;
        }
    }
    if Path::new("/etc/openwrt_release").exists() {
        return true;
    }
    if let Ok(osr) = std::fs::read_to_string("/etc/os-release") {
        if osr.to_lowercase().contains("id=openwrt") {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 客户端概览（wb_agents.overview）
// ---------------------------------------------------------------------------

/// 单个客户端的概览条目。
///
/// `applied` 只在 `integration-state.json` 有该客户端记录时非 null；
/// `external_change`（sha256 比对）依赖写盘侧的哈希，本关未实现，恒为
/// `false` 并标注。
fn client_overview(client: &ClientSpec, state: &Value, home: &Path) -> Value {
    let paths = client_paths(client, home);
    let existing = paths.iter().any(|p| p.exists());
    let installed = client_probe(client, home) || existing;
    let record = state.get(client.id);
    let configured = matches!(record, Some(Value::Object(_)));
    let applied = match record {
        Some(Value::Object(rec)) => {
            let files = rec
                .get("files")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let s = |k: &str| -> Value {
                rec.get(k)
                    .and_then(Value::as_str)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null)
            };
            json!({
                "at": s("applied_at"),
                "base_url": s("base_url"),
                "model": s("model"),
                "files": files,
                // sha256 外部改动检测属写盘侧，本关未实现。
                "external_change": false,
            })
        }
        _ => Value::Null,
    };
    json!({
        "id": client.id,
        "label": client.label,
        "desc": client.desc,
        "protocol": client.protocol,
        "installed": installed,
        "configured": configured,
        "config_paths": paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "applied": applied,
    })
}

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 智能体列表的**模型目录**（Python `_agents_models` 的静态回退分支）。
///
/// 从 [`crate::catalog::hub_model_ids`] 取静态 id 清单，逐条输出 `{"id": ...}`
/// ——去重保序、国际版在前，与 Python `wb_pricing.hub_model_ids()` 逐字同构。
/// live 目录那半边（网关在线模型）本关不取：面板必须在上游不可用时也能回答，
/// 少取只会让列表变短，绝不会报错。
pub fn agents_models() -> Vec<Value> {
    static_models()
}

/// CLI 智能体信息（Python `_get_agents_cli`，`:16880-16905`）。
///
/// 读取仓库里的 `wb_agents.py`（默认路径 = `CARGO_MANIFEST_DIR/../../../wb_agents.py`，
/// 与契约测试读 `wb_proxy.py`/`wb_catalog.py` 的路径约定一致；可用
/// `WB_AGENTS_SCRIPT` 环境变量覆盖，便于部署时脚本不在仓库附近）。只读、不带
/// 任何面板会话与回环检查——调用方是别的机器，这正是 CLI 的用途。
///
/// 成功返回：
/// ```json
/// {"available": true, "filename": "wb_agents.py", "content": "<文本>",
///  "bytes": N, "content_type": "text/plain; charset=utf-8",
///  "content_disposition": "inline; filename=\"wb_agents.py\""}
/// ```
/// 失败返回 `{"available": false, "error": "wb_agents.py unavailable: ..."}`，
/// 对应 Python 的 500 分支。
pub fn agents_cli_view(_accounts_dir: &Path) -> Value {
    let path = std::env::var("WB_AGENTS_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../wb_agents.py"));
    match std::fs::read(&path) {
        Ok(bytes) => {
            let len = bytes.len();
            json!({
                "available": true,
                "filename": WB_AGENTS_SCRIPT,
                "content": String::from_utf8_lossy(&bytes).into_owned(),
                "bytes": len,
                "content_type": "text/plain; charset=utf-8",
                "content_disposition": "inline; filename=\"wb_agents.py\"",
            })
        }
        Err(exc) => json!({
            "available": false,
            "error": format!("wb_agents.py unavailable: {exc}"),
        }),
    }
}

/// available 廉价判定（Python `/agents/available`，`:15773-15778`）。
///
/// 看板启动时用它决定要不要显示入口：不 import `wb_agents`、不做任何探测
/// （issue #246）。返回 `{"enabled": bool}`。本层只算**部署形态**那半边
/// （容器 / OpenWrt ⇒ 不可用）；Python 的 `agents_client_allowed` 还要求请求
/// 来源是回环地址，那属于 per-request 属性，由接线层拿到 `client_address`
/// 后与本返回值做 AND。
pub fn agents_available(_accounts_dir: &Path) -> Value {
    json!({ "enabled": !server_deployment_form() })
}

/// 智能体列表（Python `_get_agents`，`:16906-16935`）。
///
/// 返回 `enabled: true` 的全量形状（Python 的「不可用」分支是 per-request
/// 的回环判定，接线层在请求非回环时自行回退到
/// `{"enabled": false, "reason": AGENTS_DISABLED_REASON}`）。键序：
/// `enabled / clients / models / keys / global_key_set / auth_required / gateway`。
pub fn agents_view(accounts_dir: &Path) -> Value {
    let home = home_dir();
    let state = load_integration_state(accounts_dir);

    let mut clients = Map::new();
    for client in CLIENTS.iter() {
        clients.insert(
            client.id.to_string(),
            client_overview(client, &state, &home),
        );
    }

    let api_key_env = std::env::var("API_KEY").ok();
    let lan = std::env::var("WB_LAN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let global_key_set =
        wb_storage::effective_api_key(accounts_dir, api_key_env.as_deref()).is_some();

    json!({
        "enabled": true,
        "clients": Value::Object(clients),
        "models": agents_models(),
        "keys": configured_keys_view(accounts_dir),
        "global_key_set": global_key_set,
        "auth_required": wb_storage::auth_required(accounts_dir, api_key_env.as_deref(), lan),
        "gateway": {"base_url": gateway_base_url_hint()},
    })
}

/// apply：校验 + 清洗模型清单（Python `_handle_agents_apply` 前半，`:16958-17015`）。
///
/// 完整复刻前半的校验链：client 必填 → base_url 必须 `http(s)://` → models
/// 缺省回退静态目录、非 list 报错 → **80 截断** → 逐条清洗（dict 带 `id` 原样
/// 保留，非空字符串转 `{"id": <trimmed>}`，其余丢弃）→ key 解析
/// （`_agents_resolve_key`：`__global` → 全局 key；`key_id` 按面板 key 表精确
/// 匹配；缺省走全局 key，否则第一个 enabled 面板 key）→ 无 key 时按
/// `auth_required()` 决定报错或写入 `wb-local` 占位并追加 warning。
///
/// **不写外部文件**：`wb_agents.integrate`（写客户端 config + state 记录）属
/// 集成引擎，留接线层 / 后续关卡。解析出的 API key 明文**不随返回值透出**
/// （`key_status` 只给来源标记），避免本函数返回值被直接当响应体转发而泄密。
///
/// 错误返回 `(400, 消息)`，消息与 Python `_error(400, ...)` 逐字一致。
pub fn agents_apply_validate(payload: &Value, accounts_dir: &Path) -> Result<Value, (u16, String)> {
    // ---- client 必填（Python `payload.get("client") or payload.get("client_id") or ""`）----
    let client_val = [payload.get("client"), payload.get("client_id")]
        .into_iter()
        .flatten()
        .find(|v| python_truthy(v));
    let client_id = python_str_or(client_val).trim().to_string();
    if client_id.is_empty() {
        return Err((400, "client is required".to_string()));
    }

    // ---- base_url 必须 http(s)://（正则 `^https?://`）----
    let base_url = python_str_or(payload.get("base_url")).trim().to_string();
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err((
            400,
            "base_url must start with http:// or https://".to_string(),
        ));
    }

    // ---- model：`str(payload.get("model") or "").strip() or None`----
    let model = python_str_or(payload.get("model")).trim().to_string();
    let model = if model.is_empty() { None } else { Some(model) };

    // ---- models：falsy（缺键 / null / [] / {} / false / 0）回退静态目录 ----
    let models_raw = match payload.get("models") {
        Some(v) if python_truthy(v) => v.clone(),
        _ => Value::Array(static_models()),
    };
    let Some(models) = models_raw.as_array() else {
        return Err((400, "models must be a list".to_string()));
    };
    // 失控的选择器不得变成 MB 级配置：先截断再清洗（`models = models[:80]`）。
    let truncated = models.len() > 80;
    let cleaned_models: Vec<Value> = models
        .iter()
        .take(80)
        .filter_map(|item| match item {
            Value::Object(obj) if obj.get("id").is_some() => Some(Value::Object(obj.clone())),
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(json!({ "id": t }))
                }
            }
            _ => None,
        })
        .collect();

    // ---- key 解析（`_agents_resolve_key`，`:16936-16960`）----
    let key_id = python_str_or(payload.get("key_id")).trim().to_string();
    let api_key_env = std::env::var("API_KEY").ok();
    let lan = std::env::var("WB_LAN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // 解析结果：`(key 是否可用, 来源标记)`。key 明文只用于（未实现的）
    // integrate 传参，本关不返回值里携带。
    let (key_ok, key_status): (bool, String) = if key_id == "__global" {
        (
            wb_storage::effective_api_key(accounts_dir, api_key_env.as_deref()).is_some(),
            "global".to_string(),
        )
    } else if !key_id.is_empty() {
        let keys = wb_storage::api_keys(accounts_dir);
        match keys.iter().find(|k| k.id == key_id) {
            Some(entry) => {
                if entry.key.is_empty() {
                    (false, format!("panel:{key_id}"))
                } else {
                    (true, format!("panel:{key_id}"))
                }
            }
            None => {
                return Err((400, format!("no configured key with id {key_id:?}")));
            }
        }
    } else {
        let global = wb_storage::effective_api_key(accounts_dir, api_key_env.as_deref());
        if global.is_some() {
            (true, "global".to_string())
        } else if let Some(entry) = wb_storage::api_keys(accounts_dir)
            .into_iter()
            .find(|k| k.enabled)
        {
            if entry.key.is_empty() {
                (false, "panel".to_string())
            } else {
                (true, format!("panel:{}", entry.id))
            }
        } else {
            (false, String::new())
        }
    };

    let mut warnings: Vec<String> = Vec::new();
    let key_status = if key_ok {
        key_status
    } else {
        // 无可用 key：认证开启 → 400；关闭 → 占位 + warning（Python 逐字）。
        if wb_storage::auth_required(accounts_dir, api_key_env.as_deref(), lan) {
            return Err((
                400,
                "no gateway key available - 请先配置 API Key".to_string(),
            ));
        }
        warnings.push(
            "gateway has no key configured; wrote placeholder 'wb-local' \
             (auth is off, so any value works)"
                .to_string(),
        );
        "placeholder".to_string()
    };

    Ok(json!({
        "client": client_id,
        "base_url": base_url,
        "model": model,
        "models": cleaned_models,
        "models_truncated": truncated,
        "key_status": key_status,
        "warnings": warnings,
        "integrated": false,
        "note": "wb_agents.integrate（写客户端 config 文件与 state 记录）属集成引擎，本关未实现",
    }))
}

/// restore：校验 + 返回结构（Python `_handle_agents_restore`，`:17016-17030`）。
///
/// 只做本层能做的：client 必填、客户端是否为已知六种（对应
/// `wb_agents._client` 的 unknown-client 分支）。**真正删文件 / 清 state 记录
/// （`wb_agents.restore`）本关不实现**，`restored` 列表留给接线层回填，返回值
/// 里带 `note` 标注。
///
/// 校验失败返回 `{"ok": false, "status": 400, "error": "<消息>"}`；成功返回
/// `{"ok": true, "client", "label", "restored": [], "note"}`。
pub fn agents_restore_plan(payload: &Value) -> Value {
    let client_val = [payload.get("client"), payload.get("client_id")]
        .into_iter()
        .flatten()
        .find(|v| python_truthy(v));
    let client_id = python_str_or(client_val).trim().to_string();
    if client_id.is_empty() {
        return json!({
            "ok": false,
            "status": 400,
            "error": "client is required",
        });
    }
    let Some(client) = find_client(&client_id) else {
        let known: Vec<&str> = CLIENTS.iter().map(|c| c.id).collect();
        return json!({
            "ok": false,
            "status": 400,
            "error": format!("unknown client {client_id:?} (known: {})", known.join(", ")),
        });
    };
    json!({
        "ok": true,
        "client": client.id,
        "label": client.label,
        "restored": [],
        "note": "wb_agents.restore（写客户端 config 文件 + 清 integration-state.json 记录）本关未实现；restored 列表待接线层回填",
    })
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 测试间改环境变量的串行锁（`std::env::set_var` 是进程全局的）。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 作用域内设置一批环境变量，Drop 时还原。
    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&str, &str)]) -> EnvGuard {
            let mut guard = EnvGuard { saved: Vec::new() };
            for (k, v) in vars {
                guard.saved.push(((*k).to_string(), std::env::var(k).ok()));
                std::env::set_var(k, v);
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    // ---- agents_models ----

    #[test]
    fn models_flat_list_matches_static_catalog() {
        let models = agents_models();
        assert_eq!(models.len(), crate::catalog::HUB_MODEL_IDS.len());
        for (id, entry) in crate::catalog::HUB_MODEL_IDS.iter().zip(models.iter()) {
            assert_eq!(*entry, json!({ "id": id }));
        }
    }

    // ---- agents_cli_view ----

    #[test]
    fn cli_view_serves_an_existing_script() {
        // 用 WB_AGENTS_SCRIPT 指向受控文件，验证响应的形状与元数据。
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("wb_agents.py");
        let body = "print('hello')\n".repeat(10);
        std::fs::write(&script, &body).unwrap();
        let guard = EnvGuard::set(&[("WB_AGENTS_SCRIPT", script.to_str().unwrap())]);
        let v = agents_cli_view(Path::new("."));
        assert!(v["available"].as_bool().unwrap());
        assert_eq!(v["filename"], WB_AGENTS_SCRIPT);
        assert_eq!(v["content_type"], "text/plain; charset=utf-8");
        assert_eq!(
            v["content_disposition"],
            "inline; filename=\"wb_agents.py\""
        );
        assert_eq!(v["bytes"].as_u64().unwrap() as usize, body.len());
        assert_eq!(v["content"], body);
        drop(guard);
    }

    #[test]
    fn cli_view_default_path_resolves_repo_script_when_present() {
        // 默认路径 = CARGO_MANIFEST_DIR/../../../wb_agents.py。仓库里有该文件
        // 时验证整份可用；构建/部署副本缺文件时只验证降级分支不 panic。
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../wb_agents.py");
        let v = agents_cli_view(Path::new("."));
        if repo.exists() {
            assert!(
                v["available"].as_bool().unwrap(),
                "默认路径应能找到 wb_agents.py"
            );
            assert!(v["content"].as_str().unwrap().contains("def overview"));
        } else {
            assert_eq!(v["available"], false);
        }
    }

    #[test]
    fn cli_view_reports_missing_script_via_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::set(&[("WB_AGENTS_SCRIPT", "/definitely/not/here.py")]);
        let v = agents_cli_view(Path::new("."));
        assert!(!v["available"].as_bool().unwrap());
        assert!(v["error"]
            .as_str()
            .unwrap()
            .starts_with("wb_agents.py unavailable:"));
        drop(guard);
    }

    // ---- agents_available ----

    #[test]
    fn available_obeys_deployment_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g1 = EnvGuard::set(&[("WB_SERVER_DEPLOYMENT", "1")]);
        assert_eq!(
            agents_available(Path::new(".")),
            json!({ "enabled": false })
        );
        drop(g1);
        let g2 = EnvGuard::set(&[("WB_SERVER_DEPLOYMENT", "0")]);
        assert_eq!(agents_available(Path::new(".")), json!({ "enabled": true }));
        drop(g2);
    }

    // ---- agents_view ----

    #[test]
    fn view_builds_full_shape_with_empty_accounts_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::set(&[
            ("API_KEY", ""),
            ("WB_LAN", ""),
            ("HOST", "127.0.0.1"),
            ("PORT", "8789"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let v = agents_view(dir.path());
        assert_eq!(v["enabled"], true);
        assert_eq!(v["keys"], json!([]));
        assert_eq!(v["global_key_set"], false);
        assert_eq!(v["auth_required"], false);
        assert_eq!(v["gateway"]["base_url"], "http://127.0.0.1:8789/v1");
        assert_eq!(v["models"], json!(agents_models()));

        // 客户端概览：六条、升序、键齐。
        let clients = v["clients"].as_object().unwrap();
        let ids: Vec<&str> = clients.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            vec!["claude-code", "codex", "crush", "dsh", "opencode", "zcode"]
        );
        for (id, spec) in CLIENTS.iter().enumerate() {
            assert_eq!(clients[spec.id]["id"], spec.id);
            assert_eq!(clients[spec.id]["label"], spec.label);
            assert_eq!(clients[spec.id]["protocol"], spec.protocol);
            assert_eq!(clients[spec.id]["configured"], false);
            assert_eq!(clients[spec.id]["applied"], Value::Null);
            let paths = clients[spec.id]["config_paths"].as_array().unwrap();
            assert_eq!(paths.len(), spec.files.len());
            let _ = id;
        }
        drop(guard);
    }

    #[test]
    fn view_reads_integration_state_for_applied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(STATE_FILE),
            r#"{"claude-code": {"applied_at": "2026-10-26 09:00:00",
                               "base_url": "http://127.0.0.1:8789/v1",
                               "model": "gpt-5.5",
                               "files": [{"path": "/x", "existed": true, "backup": "/b",
                                          "sha256": "abc", "bytes": 1}]}}"#,
        )
        .unwrap();
        let v = agents_view(dir.path());
        let cc = &v["clients"]["claude-code"];
        assert_eq!(cc["configured"], true);
        assert_eq!(cc["applied"]["at"], "2026-10-26 09:00:00");
        assert_eq!(cc["applied"]["base_url"], "http://127.0.0.1:8789/v1");
        assert_eq!(cc["applied"]["model"], "gpt-5.5");
        assert_eq!(cc["applied"]["files"], 1);
        // sha256 外部改动检测未实现，恒 false（标注）。
        assert_eq!(cc["applied"]["external_change"], false);
        // 别的客户端没有记录。
        assert_eq!(v["clients"]["codex"]["configured"], false);
    }

    // ---- agents_apply_validate ----

    #[test]
    fn apply_requires_client() {
        let dir = tempfile::tempdir().unwrap();
        let err = agents_apply_validate(&json!({}), dir.path()).unwrap_err();
        assert_eq!(err, (400, "client is required".to_string()));
    }

    #[test]
    fn apply_client_accepts_client_or_client_id() {
        let dir = tempfile::tempdir().unwrap();
        // client 空 → 回落 client_id；字符串空白会被 strip 掉。
        let v = agents_apply_validate(
            &json!({"client_id": "claude-code", "base_url": "https://gw"}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(v["client"], "claude-code");
        assert_eq!(v["base_url"], "https://gw");
    }

    #[test]
    fn apply_requires_http_base_url() {
        let dir = tempfile::tempdir().unwrap();
        let err = agents_apply_validate(
            &json!({"client": "codex", "base_url": "ftp://nope"}),
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            (
                400,
                "base_url must start with http:// or https://".to_string()
            )
        );
    }

    #[test]
    fn apply_rejects_non_list_models() {
        let dir = tempfile::tempdir().unwrap();
        let err = agents_apply_validate(
            &json!({"client": "codex", "base_url": "https://gw", "models": "gpt-5.5"}),
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(err, (400, "models must be a list".to_string()));
    }

    #[test]
    fn apply_falls_back_to_static_catalog_when_models_falsy() {
        let dir = tempfile::tempdir().unwrap();
        for models in [Value::Null, json!([]), json!({}), json!(false), json!(0)] {
            let mut payload = json!({"client": "codex", "base_url": "https://gw"});
            if !models.is_null() {
                payload["models"] = models.clone();
            }
            let v = agents_apply_validate(&payload, dir.path()).unwrap();
            assert_eq!(
                v["models"],
                json!(static_models()),
                "models={models} 应回退静态目录"
            );
            assert_eq!(v["models_truncated"], false);
        }
    }

    #[test]
    fn apply_cleans_models_and_truncates_at_80() {
        let dir = tempfile::tempdir().unwrap();
        let mut raw = vec![
            json!({"id": "a", "name": "Alpha"}),
            json!("  b  "),
            json!(""),
            json!(42),
            json!({"x": 1}),
            json!("c"),
        ];
        // 补到 90 条验证 80 截断：截断发生在清洗**之前**（`models[:80]`），
        // 清洗再丢弃 3 条垃圾 ⇒ 90 → 80 → 77 条。
        for i in 0..84 {
            raw.push(json!(format!("m{i}")));
        }
        let v = agents_apply_validate(
            &json!({"client": "codex", "base_url": "https://gw", "models": raw}),
            dir.path(),
        )
        .unwrap();
        let cleaned = v["models"].as_array().unwrap();
        assert_eq!(cleaned.len(), 77);
        assert_eq!(cleaned[0], json!({"id": "a", "name": "Alpha"})); // dict 原样保留
        assert_eq!(cleaned[1], json!({"id": "b"})); // 字符串 strip 后转 {"id"}
        assert_eq!(cleaned[2], json!({"id": "c"})); // 空串 / 数字 / 无 id dict 被丢弃
        assert_eq!(cleaned[3], json!({"id": "m0"})); // m0 接在清洗后
        assert_eq!(cleaned[76], json!({"id": "m73"})); // 第 80 个原始位（idx 79）是 m73
        assert_eq!(v["models_truncated"], true);
    }

    #[test]
    fn apply_unknown_key_id_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let err = agents_apply_validate(
            &json!({"client": "codex", "base_url": "https://gw", "key_id": "nope"}),
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(err, (400, "no configured key with id \"nope\"".to_string()));
    }

    #[test]
    fn apply_uses_named_panel_key_when_key_id_matches() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::set(&[("API_KEY", ""), ("WB_LAN", "")]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"api_keys": [{"id": "k1", "name": "主 key", "key": "sk-x", "enabled": true}]}"#,
        )
        .unwrap();
        let v = agents_apply_validate(
            &json!({"client": "codex", "base_url": "https://gw", "key_id": "k1"}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(v["key_status"], "panel:k1");
        assert_eq!(v["warnings"], json!([]));
        drop(guard);
    }

    #[test]
    fn apply_placeholder_when_no_key_and_auth_off() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::set(&[("API_KEY", ""), ("WB_LAN", "")]);
        let dir = tempfile::tempdir().unwrap(); // 空 settings.json
        let v = agents_apply_validate(
            &json!({"client": "codex", "base_url": "https://gw"}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(v["key_status"], "placeholder");
        assert_eq!(v["integrated"], false);
        let warnings = v["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0]
            .as_str()
            .unwrap()
            .contains("wrote placeholder 'wb-local'"));
        drop(guard);
    }

    #[test]
    fn apply_placeholder_requires_auth_error_when_auth_on() {
        // WB_LAN=1 令 auth_required()=true（lan 模式强制要求 key）；
        // 无 key 可解析 → Python 的「请先配置 API Key」分支。
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::set(&[("API_KEY", ""), ("WB_LAN", "1")]);
        let dir = tempfile::tempdir().unwrap();
        let err = agents_apply_validate(
            &json!({"client": "codex", "base_url": "https://gw"}),
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            (
                400,
                "no gateway key available - 请先配置 API Key".to_string()
            )
        );
        drop(guard);
    }

    // ---- agents_restore_plan ----

    #[test]
    fn restore_requires_client() {
        let v = agents_restore_plan(&json!({}));
        assert_eq!(v["ok"], false);
        assert_eq!(v["status"], 400);
        assert_eq!(v["error"], "client is required");
    }

    #[test]
    fn restore_rejects_unknown_client() {
        let v = agents_restore_plan(&json!({"client": "not-a-client"}));
        assert_eq!(v["ok"], false);
        assert_eq!(v["status"], 400);
        assert!(v["error"].as_str().unwrap().starts_with("unknown client"));
    }

    #[test]
    fn restore_known_client_returns_plan_shape() {
        let v = agents_restore_plan(&json!({"client": "claude-code"}));
        assert_eq!(v["ok"], true);
        assert_eq!(v["client"], "claude-code");
        assert_eq!(v["label"], "Claude Code");
        assert_eq!(v["restored"], json!([]));
        assert!(v["note"].as_str().unwrap().contains("本关未实现"));
    }

    // ---- python_truthy / python_str_or ----

    #[test]
    fn python_truthy_matches_python_bool() {
        assert!(!python_truthy(&json!(null)));
        assert!(!python_truthy(&json!(false)));
        assert!(python_truthy(&json!(true)));
        assert!(!python_truthy(&json!(0)));
        assert!(python_truthy(&json!(1)));
        assert!(!python_truthy(&json!(0.0)));
        assert!(python_truthy(&json!(0.5)));
        assert!(!python_truthy(&json!("")));
        assert!(python_truthy(&json!("x")));
        assert!(!python_truthy(&json!([])));
        assert!(python_truthy(&json!(["x"])));
        assert!(!python_truthy(&json!({})));
        assert!(python_truthy(&json!({"a": 1})));
    }
}
