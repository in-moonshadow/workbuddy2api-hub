# M3 Rust 里程碑实施计划（gateway）

日期：2026-10-16
基分支：feat/rust-m2（已推送 origin/feat/rust-m2，PR 链接已在远程推送输出给出）
前置：M2（identity + storage）——/health 授权身份字段、key 三来源（effective_api_key）、契约测试框架就绪
spec：docs/superpowers/specs/2026-10-15-rust-refactor-design.md（:40「gateway/ # 上游代理、槽位调度、限流、健康探测」，:50 依赖链 identity → gateway/tasks → api）
调研依据：fork 子代理对 wb_proxy.py（18945 行）的只读契约提炼（2026-10-16），本节行号均指 Python 侧

## 范围

### M3-a（本次实施）——代理核心 + 调度状态机（内存态，不落盘）

| 模块 | 内容 | 权威参照 |
|---|---|---|
| gateway: endpoints | 上游端点表 `(realm, product) → base_url`，`CHAT_PATH=/v2/chat/completions` | wb_identity.py:51-68；wb_proxy.py:12532, 238 |
| gateway: build_body | 入站 body 变形管线：`normalize_roles → apply_prompt_mode → sanitize_messages → backfill_reasoning_content`，首条非 system 前置 SYSTEM_PROMPT，删 `_` 私有键，修复 tool-call 配对，**强制 `stream=true` + `stream_options.include_usage`** | 11803-11878 |
| gateway: outbound | 出站头 12 项（含 product 身分头）、每账号代理、per-realm 转发 | wb_accounts.py:1114-1175 |
| gateway: pool | 账号状态位建模 + 五级选择管道（见契约 §7）+ affinity/demote | wb_accounts.py Account.ready/pick/note_* |
| gateway: slots | 全局并发信号量：默认 32（env `WB_MAX_CONCURRENT_CHAT`），auto 模式每就绪账号 1 槽、下限 `CHAT_SLOTS_FLOOR=32`，获取 30s 超时 → 503 | 272, 45-52, 273-302；18349-18368 |
| gateway: proxy | 流式 SSE 转发（clean_chunk + leaked-tool 过滤器）+ 非流式聚合（aggregate + usage 兜底 token 估算）；重试预算；错误信封 | 18432-18599；15425-15436 |
| gateway: rate | 并发 503；上游 429/402/401/403 分类冷却（三范围）；`Retry-After` | 15442-15497, 12756-12789, 12639-12688 |
| api: routes | 接入 chat 路由 + `/realm` | 15819-15820 |

### M3-b（推迟，不在本次）

- 面板登录 IP 桶限流（17031-17056）——依赖 M5 面板端点
- 上游 keep-alive 池调优（wb_upstream_pool.py）——Rust 侧 reqwest 自带连接池，等价验证放 M6 shadow 回放
- `/v1/messages/count_tokens` 本地 token 估算（18097-18109）——保真估算成本高，先透传上游真实计数（见 Task 4 注）
- 后台线程（Scheduler/PriceRefresher/CreditsRefresher/UpdateChecker）——tasks 属 M4/M5

## 关键行为契约（13 条，全部带行号）

1. **上游永远收 `stream=true` + `stream_options={"include_usage":true}`**（11875-11877）；`stream=false` 客户端由网关本地聚合。非流式响应 = 聚合后完整 chat.completion。
2. 上游 URL = `account.chat_base_url() + /v2/chat/completions`；realm 判定优先级 `?realm=` → key 绑定 → `X-Realm` 头 → `detect_model_realm(model)`（12446）。
3. 出站头：Content-Type/Accept/X-Requested-With/Origin/Referer/X-CodeBuddy-Request:1/Accept-Language/X-Machine-ID/X-Session-ID/`Authorization: Bearer <token>` + product 身分头 + 可选 X-Device-Token（wb_accounts.py:1114-1175）。
4. 超时：`header_timeout` 默认 **120s**（connect/TLS/首字节）、`idle_timeout` 默认 **300s**（流式每读），无总时长上限（12377-12428）。
5. 重试预算：`max(2, pool.count_ready(realm,model)) + 1 + (4 若 auto_switch)`；每次失败 demote 对应 (session, account)；瞬时错误退避 `sleep(min(0.6*hits, 2.0))`，池耗尽重轮 `sleep(min(1.5*hits, 3.0))`（12512-12747）。
6. 并发闸：`BoundedSemaphore` 获取 30s 超时 → **503** `{"error":{"message":"gateway is at its concurrent chat limit (N in flight); retry shortly","type":"overloaded_error","code":503}}`；finally 必释放（18349-18368）。
7. 选择管道（wb_accounts.py:2862-2995, 3127-3152）：① session affinity 热号 → ② 手动权重>1 账号加权轮询 → ③ 临期积分优先（**默认关**）→ ④ 剩余用量优先（**默认关**）→ ⑤ 全局轮询。M3 实现③④开关位，默认行为=①→②→⑤。
8. 无可用账号 → **503** `no usable account for realm ...` + `pool_unavailable_detail` 逐账号列出 `unavailable_reason`（12790-12820, 18444-18449）。
9. 上游 429 分类（12208-12374）：`parse_rate_limit_reset` 解析 `(?:reset at|将在) YYYY-MM-DD HH:MM:SS` + `UTC±N`；仅正文明说账号级才按账号冷却；SOFT_LIMIT_MARKERS（"usage exceeds frequency limit"/"too many requests"/"请求过于频繁"/code 14003）→ 软限阶梯；code **6004** 是「额度用满」唯一标志 → 日额度。未识别 → 单模型 60s 窗口。
10. 错误信封：OpenAI 一律 `{"error":{"message","type","code"}}`（code=HTTP 状态码）+ 可选 `gateway_hint`；常见 type：`invalid_request_error`/`server_error`/`rate_limit_error`/`overloaded_error`/`authentication_error`（15425-15436）。Anthropic 路由用 `{type:"error",error:{type,message}}`（15437-15441）。
11. 上游 HTTPError 状态码**透传**，body 重包 `{"error":{"message":"upstream NNN: <上游原始前600字节>",...}}`；只有重试预算耗尽才透传（18432-18438）。兜底：`no usable account` 开头 → 503，否则 502 `upstream unreachable: <exc>`（18439-18449）。
12. CORS（259-269, 15690-15697）：API 前缀 `/v1`、`/chat`、`/completions`、`/models`、`/responses` 带 `Access-Control-Allow-Origin:*`；管理前缀不带；OPTIONS → 204。
13. `/realm` → `{"current":"intl"|"cn","options":["intl","cn"]}`（15819-15820）；`/health` 恒 200 报数（M2 已完成）；流式中断补 `data: [DONE]`、空流补 error 块（18545-18564）。

## 依赖决策（需用户确认）

- **新增生产依赖 `reqwest`**（upstream HTTP 客户端，streaming + 连接池 + 超时）——Rust 侧无既有 HTTP 客户端；hyper 手写流式转发成本高。托盘：默认不跟随重定向（Python 3xx 回退 urllib 语义），关闭自动解压？核对。
- **`tokio::time` 与 tokio-util**：SSE 读超时实现 idle_timeout。
- **dev-dependency：WireMock（`wiremock` crate）或自建 stub 上游**——M3 测试**不打真实上游**，用本地 stub 验证流式/非流式/429/错误信封。推荐 wiremock（维护活跃）。

## 验收

- `cargo test` 全绿（M2 77 + M3 新增），`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` 零警告
- M3 契约测试覆盖 13 条契约；先写测试后实现（TDD）
- Mock 上游全链路验证：流式转发、非流式聚合、429 冷却、503 并发、错误信封、CORS、TTFB/空闲超时
- 不触碰运行中 Python 8788；不写真实 accounts/；测试独立 tempdir
- 凭据明文不落地/不日志（现有约束延续）

## 风险

- 上游端点/头随上游变更漂移（rebase drift）——契约测试锁定形状
- SSE 聚合边界（chunk 切分、finish chunk 合成、usage 兜底估算）——wiremock 固定 fixture 覆盖
- 五级调度与 Python 细微差异（affinity 键语义、权重轮询步长）——先契约锁定，M6 shadow 回放对照
- reqwest 与 urllib keep-alive 池行为差（stale 重试、重定向）——孤岛验证后列 M6 噪声白名单

## 任务分解

### Task 1：gateway crate 骨架 + 端点表 + 出站头
- 新建 `crates/gateway/`（lib），`_ENDPOINTS` 表（realm/product → base_url）、`chat_base_url(product)`、出站头构造
- 对照 wb_identity._ENDPOINTS 写测试
- 产出物：`cargo test -p wb-gateway` 全绿

### Task 2：入站 body 变形管线
- `normalize_roles`/`apply_prompt_mode`/`sanitize_messages`/`backfill_reasoning_content`/SYSTEM_PROMPT/私有键删除/强制 stream=true + include_usage
- 对照样本 payload（wiremock 固定 fixture）写测试
- 产出物：管线纯函数测试全绿 + 变形后 body 形状断言

### Task 3：池与调度
- 账号状态位（cooldown/model_cooldowns/balance/breaker/degrade/caps 的第一步子集）+ `ready(model)` 近似 + 五级选择（affinity→权重→轮询；③④开关位）+ demote
- 并发闸/信号量（默认 32、auto 随池、FLOOR=32、30s→503）
- 产出物：调度测试全绿（顺序/冷却/无可用账号 503）

### Task 4：转发与聚合 + 重试
- SSE 流式转发（clean_chunk + leaked-tool 过滤器 + `[DONE]` 补发）、非流式 aggregate（usage 兜底估算）、重试预算 + 退避、429/401/402/403 分类冷却、错误信封、透传
- 注：`/v1/messages/count_tokens` 先透传上游 + `{"input_tokens":N}` 原样回（真实数字由上游计），保真估算留 M5 评估——**M3 不本地估算**
- 产出物：wiremock 全链路测试全绿

### Task 5：路由接线 + 账本 + CI
- api：chat 路由 10 条（OpenAI 6 + Anthropic 4）+ 404 + CORS + `/realm`
- 更新计划文档状态；`cargo fmt/clippy/test` 全绿
- 产出物：`cargo test` 全 workspace 全绿零警告 + ledger 更新

## 约束

- 不触碰 wb_proxy.py、dashboard.html、运行中 8788、真实 accounts/
- 测试只用 mock 上游，绝不向真实上游发请求
- 凭据明文不落地/不日志；面板/设置/用量端点（M5）、tasks（M4）不在本次
- commit 与推送按流程逐次确认

## 计划授权后开工顺序

授权后先做 Task 1 骨架（含 reqwest/wiremock 依赖登记），严格 TDD。每 Task 完成即 cargo test 验证并汇报，最后 Task 5 统一账本 + CI 确认。

---

## 实施状态（本小节为追加记录，只增不改）

### Task 1–4：已完成并提交

| Task | 提交 | 内容 |
|---|---|---|
| Task 1 | `7e561e18` | 端点表与出站身份头 |
| Task 2 | `ffd48ed2` | 入站 body 变形管线 |
| Task 3 | `e0917ecd` | 账号池与调度（+ `8b9362b9` 账号错误回调 `note_*`） |
| Task 4b | `e3ceeb19` `b1886eb6` | 上游错误分类（纯函数）+ 出站客户端与错误信封 |
| Task 4c | `dae906b2` | SSE 流清洗 `clean_chunk` 与工具调用聚合 |
| Task 4d | `45fe364e` | 重试循环与错误信封 |
| Task 4e | `ecaa17fc` | 流式转发编排与 wiremock 全链路契约测试 |

### Task 5：关卡 5.1–5.4 已完成（5.4 待本关提交）

| 关卡 | 内容 | 产出物 | 状态 |
|---|---|---|---|
| 5.1 | 池装载（storage → `AccountPool`） | `cargo test -p wb-gateway` 全绿 | ✅ 已提交 `b2c75b85` |
| 5.2 | `anthropic.rs` 翻译层（`messages_to_chat` / `chat_to_messages` / `estimate_chat_tokens`） | 同上 | ✅ 已提交 `b2c75b85` |
| 5.3 | `responses.rs` 翻译层（`responses_to_chat` / `chat_to_response` / `RequestMeta`） | 203 tests 全绿 + fmt/clippy 零警告 | ✅ 本次一起提交 |
| 5.4 | API 路由接线：`AppState` 扩展 + 10 条 POST chat 路由（OpenAI 6 + Anthropic 4）+ OPTIONS + `/realm` + 404 + CORS + 401 信封 + 并发闸接线；`/v1/responses` 接 `responses_to_chat` / `chat_to_response`（非流式路径） | ① `crates/api/Cargo.toml` 增加 `wb-gateway` path dep + tokio `sync`/`time`；② 新 `crates/api/tests/contract_chat_routes.rs`（10 用例，全走 wiremock 假上游）；③ 全 workspace 全绿 | ✅ 本次提交 |
| 5.5 | 账本更新 + 全 workspace 验证 + 提交 | 本小节 + `cargo test`/`fmt`/`clippy` 实测输出 | ✅ 本小节 |

### 关卡 5.4 验证数字（实测，非推断）

- `cargo test --workspace`：**TOTAL passed=294 failed=0**
- `cargo clippy --workspace --all-targets -- -D warnings`：`Finished dev profile ... in 0.39s`，零警告
- `cargo fmt --all --check`：通过
- 契约测试**全部**经 `Config.upstream_base_url` 注入 wiremock 假上游；未向真实上游发任何请求

### 计划偏差记录（append-only）

1. **`/v1/responses` web-tools 多轮重放 + `stream_responses_events`（496 行）推迟至 M3-b**：本次仅接非流式 `responses_to_chat` / `chat_to_response`。理由：web-tools 重放依赖工具执行闭环（M3-b 范围），流式 responses 事件形状独立且量大。
2. **storage `save_realm` 实际签名 `Result<bool, StorageError>`**（计划中写作 `Result<(), _>`）：返回 `bool` 用于区分「合法值已落盘」与「非法值被拒绝但未报错」，`/realm` POST 的 `persisted` 字段依赖此语义。
3. **storage 额外公开项（计划外但必需）**：`read_active_realm`、`pool_accounts` / `PoolAccount`。前者为 `/realm` GET 读取持久化值；后者为 gateway 池装载提供账号视图（`Account` 是 `representative` 的单账号视图，不足以装载全池）。
4. **api crate 新增 `reqwest` 依赖**：非流式路径需读上游 SSE body 再聚合（对齐 Python `_chat_nonstream_response` :18584），而 `Outcome::Served` 携带 `reqwest::Response`，api 层必须能具名其类型。
5. **`parse_body` / `read_chat_json` 返回 `Result<Value, Box<Response>>` + 调用点 `*resp` 解引用**：纯为满足 `clippy::result_large_err`（`Response<Body>` 达 128 字节），无语义变化。

### 已知未覆盖项（显式标为缺口，非「应该会跑」）

- **503 并发耗尽语义**：单个 axum 实例无法在一轮请求内自我占满 32 槽，`contract_chat_routes.rs` 改为钉「池空 → Rejected → 错误信封，不挂起」；真正的并发耗尽由 `gateway::pool` 的 `ConcurrencyGate` 单测覆盖。
- **流式 chat 转发**：`contract_chat_routes.rs` 只覆盖非流式聚合路径；流式 SSE 转发由 Task 4e 的 wiremock 测试覆盖。
