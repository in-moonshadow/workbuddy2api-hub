# M4 Rust 里程碑实施计划（tasks + 调度器）

日期：2026-10-17
基分支：`feat/rust-m2`（HEAD `5487120d`，M3 已收口：294 passed / 0 failed、fmt/clippy 零警告）
前置：M2（identity + storage）、M3（gateway：池、并发闸、出站客户端、错误信封）
spec：`docs/superpowers/specs/2026-10-15-rust-refactor-design.md`（:40 tasks 属于 gateway/tasks 层，:50 依赖链 `identity → gateway/tasks → api`）
调研依据：本文件的全部行号来自本轮**直接读取**的 Python 源码（`wb_scheduler.py` 全文 231 行、`wb_tasks.py` :1-40 与 :200-879、`wb_accounts.py` :557-575/:1315-1414/:1596-1670/:2384-2443/:2600-2615、`wb_proxy.py` :5647-5649/:5866-5892/:16065-16091/:17241-17425/:17451-17461），未引用任何未经核对的二手结论。

## 本轮授权范围

用户选择：**「写 M4 计划文档」** — 先只读调研契约、产出本文件，**不写 Rust 代码**，等审阅授权后开工。
范围四块（多选全中）：`growth 每日活跃`、`签到 checkin`、`claim-all 全量任务`、`只做骨架+调度器`。

## 范围

### M4-a（本次计划覆盖）

| 模块 | 内容 | 权威参照 |
|---|---|---|
| `crates/tasks`（新） | 成长任务引擎：`fetch_growth_tasks` / `accept_tasks` / `claim_task` / `build_event` / `report_events` / `_progress_one` / `run_growth_tasks` | wb_tasks.py:107-138, 205-266, 269-318, 321-408, 525-631, 634-709 |
| `crates/tasks` | 签到与活跃上报（原挂 `Account` 上的方法） | wb_accounts.py:1319-1380, 1596-1670 |
| `crates/tasks` | claim-all 编排（跨账号、intl 优先、步骤表） | wb_proxy.py:17306-17396 |
| `crates/tasks` | 调度器状态机（整点表、初次巡检、手动触发、开关、状态视图） | wb_scheduler.py:16-231 |
| `crates/tasks` | growth 快照 TTL 缓存（20s 默认，双路并发） | wb_proxy.py:5866-5892, 16065-16086 |
| `crates/api` | 接线：`GET /tasks`、`GET /scheduler`、`POST tasks/run`、`POST tasks/travel`、`POST tasks/claim-all`、`POST tasks/streak-bonus`、`POST accounts/checkin`、`POST scheduler/trigger`、`POST scheduler/toggle` | wb_proxy.py:17098-17120, 16065-16091, 17241-17425 |

### M4-b（明确不在本次）

- `run_streak_bonus` 内部四步（补签卡 / 新手礼包+活动补偿 / 连登兑换 / 成长抽奖，wb_tasks.py:771-879）——只**保留调用位**（claim-all 与调度器里有这一步），本轮实现为「显式未实现」占位，不静默跳过。
- 猫猫旅行 `do_cat_travel`（wb_tasks.py:463-522）——同理只保留调用位；它属于「M4-b 猫猫日常」。
- 夜猫子 `run_night_growth`（wb_tasks.py:712-769）——保留 01:00 小时表位，但 01:00 的夜猫分支本轮不实现。
- 国际版 `daily_chat` 网页闭环（wb_accounts.py:1394-1412 + `wb_webagent`）——claim-all 的 intl 分支本轮只输出「未实现」步骤。
- `CreditsRefresher` / `PriceRefresher` / `UpdateChecker`（其余三个后台线程）——全部归 M5。
- 面板、设置、用量端点——M5。
- 真实上游联调与影子上线——M6。

### 后台线程清单（本轮核实，限定 M4 只取 Scheduler）

| 线程 | 类定位 | 启动点 | 归属 |
|---|---|---|---|
| `Scheduler` | wb_scheduler.py:16 | wb_proxy.py:16720、:18757（+ `:18756 SCHEDULER = Scheduler(POOL)`） | **M4** |
| `CreditsRefresher` | wb_accounts.py:2384 | wb_proxy.py:18774-18775 | M5 |
| `PriceRefresher` | wb_pricing.py:2306 | wb_proxy.py:18767；另有按需线程 `price-refresh-inherit` :16581-16582、`price-refresh-manual` :17470、`price-refresh-mapping` :17505 | M5 |
| `UpdateChecker` | wb_updates.py:192 | wb_proxy.py:18780 | M5 |

三个全局句柄：`wb_proxy.py:5647-5649`（`SCHEDULER / PRICING / CREDITS_REFRESHER = None`），Rust 侧对应 `AppState` 的一个 `Arc<Scheduler>` 可选槽。

## 闸门 0：storage 账号写回（阻塞前置，先于关卡 4.1）

**结论来源**：本轮直接读取 `wb-proxy-rs/crates/storage/src/lib.rs`，非推断。

四个 M4 项目（checkin / report_activity / growth tasks / claim-all）全部以「回写账号运行时字段」收尾（`wb_accounts.py:1594`、`:1638`、`wb_proxy.py:17396`），而 Rust 侧现在**做不到**：

| 编号 | 缺口 | 证据 | 处置 |
|---|---|---|---|
| G0-1 | `storage` 无任何账号写 API | `lib.rs` 的 `pub fn` 全表只有 `ui_language / read_active_realm / pool_accounts / save_realm / auth_required / load_snapshot / representative / effective_api_key / api_keys / match_api_key`；唯一的写函数 `save_realm`（:150）只写 `active_realm.json` | 新增单账号原子写回 |
| G0-2 | **`save_realm` 不是原子写、也不缩进**：`fs::write(&path, serde_json::json!({..}).to_string())`（:150-196） | 直接 `fs::write`，无 temp+rename，无 `indent(2)` | 新写回函数**不得**复用该实现；按 Python `wb_accounts.py:764-794` 的 temp+`os.replace` 语义重写（Rust 对应 `std::fs::rename`） |
| G0-3 | **`account_entries`（:275-300）丢弃 `PathBuf`**，返回 `Vec<(String, serde_json::Value)>` 只留文件名 | Python 在构造时保存 `path`（`wb_accounts.py:2560`），`save()` 直接写回同一文件 | 写回 API 必须携带真实 `PathBuf`，不得靠重拼文件名 |
| G0-4 | **枚举顺序不一致**：Python `sorted(os.listdir(self.dir))`（:2555）vs Rust `fs::read_dir` 未排序（:275-300） | `representative()`（:305-343）取「首个 token 非空账号」，顺序不同即选中不同账号 → `/health` 身份字段静默漂移 | 枚举改为按文件名排序，并在关卡 4.2 用测试钉住 |

字段三分类**已存在，必须复用**，不得定义第二份真相：`crates/identity/src/export.rs` 的 `VOLATILE_FIELDS`（:17，7 项）/ `PORTABLE_EXPORT_FIELDS`（:28，15 项）/ `RUNTIME_ONLY_FIELDS`（:47，`["lastError","cooldownUntil"]`），对应 `wb_accounts.py:3578-3593`。落盘前剥离 `RUNTIME_ONLY_FIELDS` 是 Python 的既有行为（:764-794）。

## 关键行为契约（23 条，全部带 Python 行号）

### A. 任务引擎（wb_tasks.py）

1. **日志注入是硬要求**：`_log` 默认是 `lambda msg: None`（:24），`set_logger(fn)`（:27-35）由调度器注入自身 `log`（wb_scheduler.py:37）。后果没有日志时「上游端点失效」与「今天没有任务」**完全不可区分**。Rust 侧不得用默认 no-op logger；`Scheduler::new` 必须把 trace sink 注入任务引擎。
2. **任务归一化形状**（:107-138，`:107-138` 把上游 `data.tasks[]` 映射为）：`task_code / name / description / jump_url / status / task_type / current / target / reward_credit / reward_energy / unforgeable / reason`；企业版账号直接返回 `[]`；任何异常 → `[]`（吞异常**只在这一层**，由契约 1 的日志兜住可观测性）。
3. **五端点**：`GET {CHAT_BASE}/v2/activity/growth/tasks`；`POST {CHAT_BASE}/v2/activity/growth/tasks/accept` body `{"task_codes": [...]}` 分块 20（:205-266）；`POST {CHAT_BASE}/activity/growth/tasks/{code}/claim` body `{}`（:269-318）；`POST {base}/v2/report` body 为**裸事件数组**（:390-408）；`POST {CHAT_BASE}/v2/report` 与 `POST {BILL_BASE}/v2/report` 唯一差异是头 `purpose="billing" | "chat"`。
4. **claim 的 400 降级**（:269-318）：只有 **HTTP 400** 才把 base 换成 `{WEB_BASE}` 并附网页头（`Authorization / Accept / Content-Type / Origin / Referer /profile/growth-center / x-client-platform: web / UA / X-User-Id / X-Domain`）；其它 HTTP 码直接失败，**不降级**。
5. **accept 的部分失败语义**（:205-266）：HTTPError 时**整块**标 failed（msg `"HTTP %s: %s"` 且 detail 截 200 字）；`code != 0` 时取 `d["msg"]`；`results` 里缺席的 code 也算 failed（靠 `seen` 集合）。→ Rust 侧必须逐 code 出结果，不能只回 bool。
6. **事件去重键是 `(eventCode, id)`**（:321-387 docstring）：`cid = f"wb-task-{now_ms}-{idx}"`，`rid = f"{cid}-req"`。**重复 id 永远不推进进度**。→ Rust 生成 id 必须带毫秒时间戳 + 序号，且同一轮重试不得复用同一 id（否则重试变空转）。
7. **各类事件的 eventCode 表**（:321-387）：`canvas→wbx_design_canvas_task_create`；`template→agent_task_created_with_template`；`expert/team/lighthouse→expert_actual_use`（默认专家 `ContentCreator` / `CloudOpsTeam` / `ex_2cvvUZQhDyeJ`，`requestModelId: deepseek-v4-flash`）；`skill→skill_info`（`skill_2096525080079265792`）；`automation→automated_task_create_suc`；`playbook→playbook_prompt_send`（id `worker-ledger-freedom-dashboard`）；`skin→appearance_skin_apply`；`chat/glmchat/cat→chat_request_send`（模型 `glm-5.2` / 大写成 `GLM-5.2`；`mode` 猫猫为 `night` 否则 `craft`）；其余 → `heartbeat`。**专家/团队 id 按 `pool[(cur + i) % len(pool)]` 轮换**（:525-631）。
8. **`_progress_one` 判定顺序不可重排**（:525-631），实际执行序为：① 未知 spec 或 `unforgeable` → 0（有 reason 则记「需真实操作完成」）；② `status == "claimed"` → 0；③ `completed` 或 `cur >= tgt` → claim；④ `code ∈ DESKTOP_ONLY_TASKS` → 0；⑤ `code ∈ NIGHT_TASK_CODES` 且不在夜间窗口 → 0；⑥ `not_accepted && task_type != "auto"` → 0；⑦ 否则按 `need = max(1, tgt - cur)` 逐条上报，条间 `sleep(gap)`，块后 `sleep(1.5)`。
9. **轮询复核**（:525-631）：`for attempt in range(6)`：重取任务；`prog >= tgt` 或 status 已 completed/claimed → 跳出；`prog > cur` → `sleep(2.5)` 继续；否则 `attempt < 2` → `sleep(1.5)`，再否则跳出。未达标 → 「领奖顺延到下次运行」返回 0（**不是失败**）。
10. **`run_growth_tasks` 四步顺序**（:634-709）：0) 先跑所有 `task_type == "auto"`（`first_buddy` 必须先 claim，否则后续 single 接取报 `prerequisite not met: first_buddy`）；1) `_needs_accept = status=="not_accepted" && !unforgeable && task_type != "auto"` → `accept_tasks`，`sleep(gap)` 后**重取并二次接取**残余；2) 逐任务 `_progress_one`；3) `do_cat_travel` 计入；4) `fetch_credits()` 后输出「本次累计新增到账 +N，当前总剩余 N」。返回 `{ok, logs, earned_credit, credits}`。
11. **企业版/非国内版短路**：`fetch_growth_tasks`（:107-138）、`run_growth_tasks`（:634-709）、`run_night_growth`（:712-769）、`run_streak_bonus`（:771-879）四处都各自短路；「国际版不适用国内成长任务中心」用 `ok: False`，「企业版无成长体系」用 `ok: True` + 跳过语义。**两者不可混同**。

### B. 签到 / 活跃上报（wb_accounts.py）

12. **`can_checkin`**（:1319-1327）：非 cn → False；企业版 → False；无 `last_checkin` → True；否则 `!startswith(today_str)`，其中 `today_str = strftime("%Y-%m-%d")`（本地时区）。`CHECKIN_PATH = "/v2/billing/meter/daily-checkin"`（:211）。
13. **`checkin` 成功判定**（:1596-1641）：`code == 0` **或** `code == 10001` 都算成功；HTTPError 分支中 `code == 10001` 或 msg 含 `"今天已签到"` / `"已签到"` 也算成功。返回 `{ok, code, msg, data}`。
    **⚠️ 与 Python 的刻意分叉（用户裁定）**：Python 在 `:1616-1619` **先写 `last_checkin` 并落盘，之后才判 `ok`**；Rust 必须**先判成功、仅在 `ok == true` 时才写盘**。详见风险 R4 修正规格。
14. **`can_report_activity`**（:1371-1380）：仅 cn + 非企业版 + `last_activity_report` 日期前缀判定。
15. **`report_activity`**（:1643-1670）：`build_event(self, "chat")` → `report_events`，非 `code=0` → `{"ok":False,"error":"活跃上报被上游拒绝（未返回 code=0）"}`；成功才写 `last_activity_report` 并落盘；随后读 `fetch_streak_days` 拼尾部（None → 「连登天数未知」，`<=0` → 「可能被上游静默丢弃」，否则「连续打卡 N 天」）。事件**必须带 `userId`**，否则上游 200 静默丢弃。
16. **`fetch_streak_days`**（:189-203）：失败返回 `None` 并记日志 `fetch_streak_days failed: {exc}`；**`None` 与 `0` 语义不同**，不得合并。
17. **`daily_chat_granted_today` / `can_daily_chat`**（:1336-1436）：判「今天是否已完成一次**计入官方每日活跃**的对话」，**以本地证据为准**：`last_daily_chat` 日期前缀（只有网页腿真正跑完才写），外加活动历史里今天「网页通道 completed」的成功记录（账号文件写盘失败时的兜底）。上游 `self.credits.packages[]`（name 含 `bonus`、`size ∈ {30.0, 50.0}`、`grant_reason` 为空）**只作兜底，且必须走 D+1 口径**：对话发生在 D 天，奖励包的 `create_time` 落在 D+1 天，「今天的包」是**昨天**的结算，不能拿来判「今天已完成」—— 那会让账号隔天才拿一包（今天被昨天的包挡住 → 今天没有对话 → 明天没有包）。2026-10-20 逐包核对 20 个 30/50 包：19 个的前一日有成功对话，20/20 的发放时刻都早于当日首次成功对话。→ Rust claim-all 的 intl 分支需要先有 credits 快照（M5 字段），本轮按 M4-b 占位；若届时从快照取值，只能查**明天**日期的包。

### C. claim-all 编排（wb_proxy.py）

18. **目标集与顺序**（:17306-17396）：`uid` 为空或 `"all"` 时取全量启用账号；**`ordered = intl_targets + cn_targets`（国际版优先）**；未知 uid → `"未找到指定账号"`；无目标 → `"未找到已启用的账号"`；每账号先打一行 `"====== [claim-all] %s (%s) realm=%s ======"` 头。
19. **cn 步骤序列**（:17306-17396）：`can_checkin` → `checkin("claim-all")` + 可选 `credit_refresh` + `sleep(0.8)`；`can_report_activity` → `report_activity()` + `sleep(0.5)`；`run_growth_tasks(gap=1.0)` + `sleep(1.0)`；`run_streak_bonus` + `sleep(0.8)`；`do_cat_travel` + `sleep(1.0)`。每个账号的 `steps` 以 `"  steps=%s"` 追加进 `lines`；异常 → `("error", False, str(exc))` 且**不中断其它账号**；账号间 `sleep(1.2)`；每次落盘。返回 `{ok: True, credit_added, accounts_count, logs}`。
20. **缓存失效**（:5866-5892, :17241-17278, :17306-17396）：`TASKS_CACHE_TTL` 默认 20s（env `WB_TASKS_CACHE_TTL`），键 `account.uid`，值 `(expiry, (tasks, summary))`；`growth_snapshot` 用 **2 线程并发**跑 `fetch_growth_tasks` 与 `fetch_growth_summary`，**任一抛异常都向上抛且不缓存**；`TTL <= 0` 直通；**每个变更路由（tasks/run、claim-all、checkin 等）先 `invalidate_tasks_cache()`**。

### D. 调度器（wb_scheduler.py）

21. **小时表与循环语义**：`checkin_hours=[9,21]`、`travel_hours=[9,21]`、`keepalive_hours=[22]`、`cat_hours=[1]` → `all_hours=[1,9,21,22]`（:20-24）；`enabled=True`（:25）；启动后 **`sleep(10)` 再跑初次巡检**（:73-77，异常只记日志）；主循环每 **30s** 轮询（:94），`enabled && tm_min == 0 && tm_hour ∈ all_hours` 时触发并 **`sleep(65)` 防当分钟二次触发**（:85-93）；`_calc_next_fire` 的当日命中条件含 `tm_min == 0 && tm_sec < 10`（:101）。
22. **互斥与视图**：`_run_cycle` 用 `_run_lock.acquire(blocking=False)`，失败记「跳过本次巡检（reason）：上一轮仍在执行」（:122-129，**巡检体入口**）；`trigger_now`（:115-120，**手动触发入口**）先做 `acquire(blocking=False)`，被占则直接返回 `{"ok":False,"msg":"已有巡检正在执行，请稍候再试"}`，否则起线程返回 `{"ok":True,"msg":"已触发后台调度执行"}`。两处是**同一把锁的两个不同入口**，不是同一段逻辑，须分别实现、分别测试。`status()` 七键 `{enabled, mode, mode_cn, mode_intl, last_run_time, next_run_time, logs}`，`logs` 取最后 **20** 条，内部环形缓冲 **60** 条（:30, :42-44, :222-231）；三条 `mode*` 字符串必须逐字对齐；日志自动分 tag：消息含 签到/打卡/活跃/猫猫/连登/夜猫/抽奖/兑换 → `tasks`，否则 `scheduler`（:49-54）。
23. **`_run_cycle` 的账号过滤与保活**（:131-220）：`for acc in list(pool.accounts): if not acc.enabled: continue`（:147-149，历史上漏过 enabled 判断）；`exp = expires_at or 0; if exp and (exp - now) < 7200 → refresh()`；cn 分支依次 checkin / report_activity / `do_cat_travel` / `run_streak_bonus` / 01:00 夜猫，各自 `sleep(1.0)`，`do_cat_travel` 仅在 `action ∈ {"claim","depart"}` 时计入；intl 分支 `can_daily_chat` → `daily_chat("scheduler")`（日志必须带 `res["msg"]`，issue #236）+ `sleep(1.5)`；末行汇总 `巡检完成：Token保活 N 个，国内签到 N 个，活跃上报 N 个，猫猫日常 N 个，国际活跃 N 个`。

## 依赖决策（需用户确认）

1. **新增 `crates/tasks`**，并更新 workspace `members`（当前只有 `storage / api / identity / gateway`）。建议命名 `wb-tasks`，依赖 `wb-storage`（读账号池）与 `wb-gateway`（复用 reqwest 出站客户端与 realm 端点表）。**反向依赖禁止**：`wb-api → wb-tasks`，不允许 `wb-tasks → wb-api`。
2. **HTTP 客户端复用，不新增依赖**：沿用 M3 的 `reqwest`。若 `wb-gateway` 未导出可复用的出站客户端构造器，则 `wb-tasks` 直接依赖 `reqwest`（已在 workspace 内，无新增第三方包）。
3. **时间语义**：`all_hours` / `in_night_window` / 日期前缀判定统统一使用**本地时区**。workspace 已有 `chrono`（`clock` + `std` feature），用它取 `Local::now()`；**不得用 UTC**，否则 22:00 保活与 09:00 签到会整体偏移 8 小时。
4. **调度循环实现**：tokio `spawn` 一个常驻任务，`interval(30s)` + `tokio::time::sleep`；`65s` 抑制期用「记录上次触发分钟」等价实现（行为等价即可，不必照抄 `sleep`）。互斥用 `Arc<tokio::sync::Mutex<()>>::try_lock()`，对齐 Python 的非阻塞语义。
5. **测试夹具**：继续用 `wiremock` 做假上游；**不得对真实上游发任何请求**。时间相关的测试需要可注入的「now」，建议 `Scheduler` 接受一个 `Clock` trait（默认 `Local` 实现），否则 30s/65s/10s 会拖垮 CI。
6. **账号落盘字段**：见「闸门 0」。要点是 `crates/storage/src/lib.rs` 的 `Account`（:264-271）只暴露 `uid/domain/realm/access_token/expires_at/credential_file`，`pub fn` 面上无任何账号写函数。M4 的 `lastCheckin / lastActivityReport / lastDailyChat` 回写需要 storage 新增写接口——**这是 M2 划定的「随 gateway 里程碑补齐」项，现在到期，且为阻塞前置**。

## 验收

- `cargo test --workspace` 全绿（M3 基线 294 passed / 0 failed 不得回退），`cargo fmt --all --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 零警告。
- 22 条契约各有对应测试；**先写测试后实现**。
- 不触碰运行中的 Python 8788 服务、不写真实 `accounts/`、测试全部使用 tempdir + wiremock。
- 凭据明文不进日志、不落盘到测试产物。

## 风险与已知缺口（显式记录，不做静默归一化）

| 编号 | 缺口/风险 | 处理方式 |
|---|---|---|
| R1 | **旅行状态路径前缀不一致**：`_travel_status`（wb_tasks.py:411-416）用 `/activity/growth/buddy/travel/status`，而 `fetch_growth_summary`（:141-186）用 `/v2/activity/growth/buddy/travel/status` | **用户已裁定：两条都保留、各带测试。**Rust 侧提供**两个独立函数**（如 `travel_status()` 与 `growth_summary()` 内的旅行段），**不得抽公共路径常量、不得合并、不得猜哪一个正确**；各自 wiremock 测试**逐字断言请求 path**（一条含 `/v2`、一条不含）。任一函数请求到错误前缀即测试失败。留 M6 影子回放定论 |
| R2 | **`claimed` 短路早于 desktop-only / 夜间窗口判定**（:525-631 顺序 ② 在 ④⑤ 之前） | 照抄顺序；写测试钉住「claimed 任务不再做任何判定」 |
| R3 | **`run_night_growth` 的 `earned_credit` 靠 `re.search(r"\+(\d+) 积分", line)` 从自己的日志行里反解**（:712-769） | 属 M4-b，本轮不实现；计划阶段先登记，实现时改为显式累加，**并在账本里记录这是对 Python 的改进** |
| R4 | **`_checkin_upstream` 在检查 `code` 之前就写 `last_checkin` 并落盘**（:1616-1619，早于同函数 `:1619` 的 `return {"ok": (code == 0 or code == 10001), ...}`） | **用户已裁定：这是缺陷，Rust 侧必须修正，不照抄 Python。**详见下方「R4 修正规格」 |
| R5 | **`wb_tasks._log` 默认 no-op**（:24） | 契约 1 已要求强制注入；Rust 类型上用 `tracing` 或显式 `EventSink`，不提供默认空实现 |
| R6 | **`daily_chat` 网页闭环依赖 `wb_webagent` + ACP 会话驱动**（wb_accounts.py:1394-1412，issue #90） | M4-b；claim-all 的 intl 分支本轮输出显式「未实现」步骤而非成功步骤 |
| R7 | **`can_daily_chat` 依赖 `credits.packages` 服务器快照**（:1329-1369），而该字段在 M5 才补齐 | 同上，本轮占位 |
| R8 | **`accept`/`claim` 的网络异常被宽泛吞掉**（:205-318、:107-138） | Rust 侧保留「不因单任务失败中断整轮」的总语义，但**必须让失败可见**（返回值 + 日志双通道），不得静默 `None` |
| R9 | **时区漂移**：Python 全靠 `time.localtime()` | 契约 3（依赖决策）已锁定本地时区；测试用注入时钟覆盖跨日边界（23:59 → 00:01） |

### R4 修正规格（用户 2026-10-17 裁定：签到失败不得标记为已签）

**缺陷机理**（`wb_accounts.py:1616-1619`）：正常响应分支里

```python
payload = http_json(...)                  # 可能返回任何 code
code = payload.get("code", -1)
self.last_checkin = time.strftime(...)    # ← 无条件写
self.save(...)                            # ← 无条件落盘
return {"ok": (code == 0 or code == 10001), ...}   # ← 之后才判成败
```

只要 `http_json` 成功返回（哪怕 `code` 是 1/2/-1/其它），`last_checkin` 就已持久化。而 `can_checkin`（:1319-1327）判的是 `not last_checkin.startswith(today_str)`，于是**当天后续所有重试（调度器 21:00、手动触发、claim-all）全部被跳过**，该账号当天的签到积分直接丢失且无任何错误可见——`ok: false` 已经返回，但没人会再试。

**Rust 侧行为（明确规格）**：

1. `last_checkin` 的写入**只在 `ok == true` 时发生**，即：
   - 正常分支：`code == 0 || code == 10001` → 写 `last_checkin` 并落盘 → 返回 `ok: true`
   - 正常分支其它 code → **不写、不落盘** → 返回 `ok: false, code, msg`（不做成功伪装）
   - HTTPError 分支：`code == 10001` 或 msg 含「今天已签到」/「已签到」 → 写盘 + `ok: true`（此分支 Python 本就正确，等价保留）
   - 网络异常 / 超时 / 解析失败 → **不写、不落盘** → `ok: false`
2. **写盘失败不得翻转签到结果**（对齐 Python `checkin` docstring「记录写失败不会改变签到结果」，:1599-1601）：上游已判定成功但落盘失败时，仍返回 `ok: true`，同时把落盘错误**单独可见地**带回（新增 `persist_error: Option<String>` 或日志双通道），不得静默吞掉。
3. **可重试性**：未写盘的情况下，`can_checkin` 次日判定必须仍为「可签」，从而使 21:00 那一轮与手动触发都能真正重试。这是本修正的**验收核心**。

**测试要求**（关卡 4.6 产出物必须包含）：
- `code=1`（上游明确失败）→ 返回 `ok:false`；**重新读取账号文件，`lastCheckin` 必须不存在或保持旧值**；随后 `can_checkin` 仍为 `true`。
- `code=0` → `ok:true` 且 `lastCheckin` 为今天；`can_checkin` 变 `false`。
- `code=10001`（正常响应）→ `ok:true` 且写盘。
- HTTP 400 + msg「今天已签到」→ `ok:true` 且写盘。
- HTTP 400 + 其它 msg → `ok:false` 且**不写盘**。
- 网络超时 → `ok:false` 且**不写盘**。
- 落盘失败（tempdir 只读模拟）→ `ok:true` 但 `persist_error` 非空。

**与 Python 的分叉登记**：此为**有意行为差异**，须在账本（关卡 4.11）显式记录，并在 M6 影子回放时列为「预期差异」而非噪声。Python 侧本轮**不修改**（不在授权范围内），仅登记。

## 关卡分解（每关产出物为可独立复核的客观痕迹）

### 关卡 4.1：`crates/tasks` 骨架 + 契约类型
- 新建 crate（`Cargo.toml` + `lib.rs`），定义 `GrowthTask`（12 字段，对齐契约 2）、`AcceptOutcome`、`ClaimOutcome`、`TravelStatus`、`GrowthSummary`。
- 产出物：`cargo test -p wb-tasks` 全绿（字段 serde 反序列化用例：完整样本 / 缺字段 / 企业版空集）；`Cargo.toml` workspace `members` diff 行号。

### 关卡 4.2：storage 幂等字段写回（**依赖闸门 0，必须第一关做**）
- `crates/storage/src/lib.rs` 新增 `Account` 的 `last_checkin / last_activity_report / last_daily_chat: Option<String>` 读字段 + 单账号原子写回函数。**原子性不得复用 `save_realm`（它用裸 `fs::write`，见闸门 0 G0-2）**，须按 Python `wb_accounts.py:764-794` 重写：唯一临时文件名 `"<path>.<pid>.<tid>.tmp"` → 写 `indent(2)` / `ensure_ascii=false` 等价的 JSON → `std::fs::rename(tmp, path)` → 异常时清理 tmp。
- **写回必须只改这三个键，其余键（含未知键）原样保留**；落盘前剥离 `identity::export::RUNTIME_ONLY_FIELDS`（复用，不新定义）。
- 同关内修 G0-3/G0-4：`account_entries` 改为携带 `PathBuf` 并按文件名排序，`representative()` 行为因此确定化。
- 临时文件必须与目标**同目录**（跨文件系统 `rename` 会失败），不得用 `std::env::temp_dir`。`crates/storage/Cargo.toml` 已有 `[dev-dependencies] tempfile = "3"`，测试侧无需新增依赖；生产代码不引入依赖。
- 产出物：新测试文件 `crates/storage/tests/contract_idempotent_fields.rs` 全绿（写入后重新读取一致；未涉及的键逐字不变；`RUNTIME_ONLY_FIELDS` 不被落盘；临时文件无残留；排序后 `representative()` 选中预期账号）；`cargo test -p wb-storage` 全绿。

### 关卡 4.3：事件构造与上报
- `build_event`（8 种 kind + 专家/团队轮换）、`report_events`（裸数组 body、`purpose` 头切换）、id 生成 `wb-task-{ms}-{idx}`。
- **R1 双路径**：`travel_status()` 与 `growth_summary()` 内的旅行段各走各的前缀，**不抽公共常量**。
- 产出物：wiremock 捕获请求体断言（数组根、`eventCode` 表逐项、`(eventCode, id)` 不重复）；**R1 两条路径各一个用例，逐字断言请求 path**（`/activity/growth/buddy/travel/status` 与 `/v2/activity/growth/buddy/travel/status`）；`cargo test -p wb-tasks` 对应用例输出。

### 关卡 4.4：任务拉取 / 接取 / 领奖
- `fetch_growth_tasks`、`accept_tasks`（分块 20、部分失败逐 code）、`claim_task`（HTTP 400 → web base 降级，其余不降级）。
- 产出物：wiremock 场景测试（200 成功 / code≠0 / 400 降级 / 500 不降级 / 分块边界 20+1）；测试名与契约编号（§3、§4、§5）一一对应。

### 关卡 4.5：`_progress_one` 全判定链 + `run_growth_tasks`
- 契约 8 的七条分支按序实现，契约 9 的 6 次轮询，契约 10 的四步顺序与二次接取。
- 产出物：`crates/tasks/tests/contract_progress.rs` 全绿；**钉住顺序的用例**：`claimed` 任务不触发 desktop-only 判定（R2）、`prerequisite not met: first_buddy` 场景、进度未达标不报失败（契约 9）。

### 关卡 4.6：签到 / 活跃上报
- 契约 12-16：`can_checkin` / `checkin`（code 0 与 10001、HTTPError 文案匹配）/ `can_report_activity` / `report_activity`（先判成功再写字段）/ `fetch_streak_days`（`None` 与 `0` 分离）。
- **R4 修正**：`checkin` 按「先判成功、仅 `ok == true` 时写盘」实现，须完成「R4 修正规格」列出的 7 条用例。
- 产出物：`crates/tasks/tests/contract_checkin.rs` 全绿；**R4 用例必须包含**：`code=1` → `ok:false` 且重读文件 `lastCheckin` 未变、随后 `can_checkin` 仍为 `true`（这是修正的验收核心）；`code=0`/`code=10001`/HTTP400「今天已签到」→ 写盘；HTTP400 其它 msg 与网络超时 → 不写盘；落盘失败 → `ok:true` 且 `persist_error` 非空。

### 关卡 4.7：growth 快照缓存
- 契约 20：`TtlCache<String, (Vec<GrowthTask>, GrowthSummary)>`，TTL 注入、并发双路、异常不缓存、`invalidate`。
- 产出物：`crates/tasks/tests/contract_snapshot_cache.rs` 全绿（TTL=0 直通、命中不重复请求、异常不写入、invalidate 后重取）。

### 关卡 4.8：调度器状态机
- 契约 21-23：小时表、初次 10s、30s 轮询、65s 抑制、`_calc_next_fire`、非阻塞互斥、`trigger_now`、`status()` 七键 + 60/20 环形日志、日志 tag 分流、`enabled=False` 账号跳过、`expires_at - now < 7200` 保活。
- 产出物：`crates/tasks/tests/contract_scheduler.rs` 全绿（注入时钟推进：09:00 命中一次且当分钟不重复；23:59→00:01 跨日 `next_run_time`；锁占用时 `trigger_now` 返回固定文案；`status()` 键集合逐字断言）。

### 关卡 4.9：claim-all 编排
- 契约 18-19：目标集、intl 优先顺序、步骤表、异常不中断、间隔 sleep（测试中注入零间隔）。
- 产出物：`crates/tasks/tests/contract_claim_all.rs` 全绿；断言「intl 账号先于 cn 账号出现」「单账号异常后其余账号仍执行」「返回体四键形状」。

### 关卡 4.10：api 接线
- 9 条路由（见范围表）；错误信封与 Python 一致；`uid` 缺省 / `"all"` / 未知 uid 三态。
- 产出物：`crates/api/tests/contract_task_routes.rs` 全绿（全走 wiremock 假上游 + tempdir 账号池）。

### 关卡 4.11：账本 + 全 workspace 验证
- 更新本文件「实施状态」小节；跑三项验证。
- 产出物：`cargo test --workspace` / `fmt --check` / `clippy -D warnings` 的**实测输出原文**写入账本（非推断数字）。

### 关卡 4.12：端点按账号身份选择（`daily_chat` 前置）
- 现状缺陷：Rust 侧成长任务用国内版常量（`events.rs:25 CHAT_BASE`），国际版账号的每日活跃会打到 `copilot.tencent.com`；旧的 `ProductionExecutor::daily_chat` 打的是**臆造端点** `/v2/activity/daily-chat`（Python 全仓 0 命中）。
- 做法：新增 `crates/tasks/src/daily_chat.rs`，端点由 `wb_gateway::endpoints::chat_base_url(realm, product)` 按**账号身份**解析（国际版 → `https://www.workbuddy.ai`），不新增任何端点常量。
- 产出物：`DailyChatConfig::resolved_chat_base` 的单元测试（国际版 → `www.workbuddy.ai`；国内版 → `copilot.tencent.com`，证明是按身份解析而非写死）。

### 关卡 4.13：网页腿 ACP 会话（`wb_webagent.py` → `wb_gateway::acp`）
- 移植 Python `wb_webagent.py`（258 行）到 `crates/gateway/src/acp.rs`：`PROTOCOL_VERSION=1`、clientCapabilities、GET `<link>`（SSE + `Acp-Connection-Id`）、POST JSON-RPC 三步（`initialize` / `session/load` / `session/prompt`）、按 `poll_interval` 轮询到 `completed`。
- **放在 wb-gateway 而非 wb-tasks**：wb-tasks 刻意无 HTTP 依赖（`Cargo.toml` 注释：「契约类型全为 serde_json::Value / 纯数据；无任何 HTTP 依赖（HTTP 由 wb-gateway 提供）」）。
- Python 的 `poll_status` 是同步闭包，Rust 需异步 → 用 boxed future 的 `PollStatus` 类型别名表达同一语义。
- 产出物：`cargo test -p wb-gateway acp` = 10 passed / 0 failed。

### 关卡 4.14：国际版 `daily_chat` 两腿编排
- 桌面腿：`POST {chat_base}/v2/chat/completions`，body 逐字照抄 Python（`model` / `messages` / `stream` / `max_tokens` / `reasoning_effort`）再过 `wb_gateway::body::build_upstream_body`。
- 渠道校验（`"code": 11128`）命中 → **降级**：网页腿单独跑，不判失败。
- 网页腿：建会话 → 取沙箱 `link`/`token` → `acp::run_turn` → 轮询到 `completed`。
- **`lastDailyChat` 只在网页腿真的跑完时写**（Python :1650-1656）；桌面腿成功但网页腿失败时**不写**，当天后续巡检才能重试。
- `daily_chat_web` 开关读 `accounts_dir/settings.json`（默认开，对齐 `wb_settings.py:1200`）。
- 产出物：`crates/tasks/tests/contract_daily_chat.rs` 全绿（10 条：两腿成功写盘 / 网页腿失败不写盘 / 11128 降级 / 11128+网页失败 / 桌面腿其他失败不跑网页腿 / 非国际版拒绝 / 关网页腿走旧语义 / 请求体逐字对齐 Python / 账号不找到不发请求 / 写盘后账号视图刷新）。

### 关卡 4.15：账本 + 全 workspace 验证
- 更新本文件「实施状态」小节；跑三项验证。
- 产出物：`cargo test --workspace` / `fmt --check` / `clippy -D warnings` 的**实测输出原文**写入账本（非推断数字）。

### 关卡 4.16：M4-b 代码评审修复（出站超时 + 4 处语义补齐）
- **M1（必须修）**：出站 HTTP 无限时 ⇒ 上游一次挂起就把巡检循环卡死。给 `UpstreamClient` 加**带时限的变体**（`post_with_timeout` / `get_with_timeout`），既有 `post` / `get` 一字不动（M3 流式聊天要把响应体转给用户，总时限会腰斩长回答）；daily_chat + ACP 链路的缓冲型调用全部切到带时限的变体。
- **S1**：上游的会话 id 可能不是字符串（Python `str(conversation)` 拼 URL）⇒ 接受数字，URL 用字符串形态、结果里保留上游原值。
- **S2**：ACP 轮询循环退出后**补查一次状态**（Python `wb_webagent.py:249-253`）——漏查会把「刚跑完但轮询没赶上」的会话判成超时。
- **S3**：ACP 的 SSE 建链必须**恰好 HTTP 200**（Python `if resp.status != 200`，不是 `is_success()`）。
- **S5**：补契约用例（数字型 id、损坏的 `settings.json`）。
- **S4**：记为 **M5 待办**（Python 的两个手动端点 Rust 侧尚无）。
- 产出物：见「实施状态」小节末尾的关卡 4.16 记录。

## 约束

- 本文件之外不动任何代码；等用户审阅本计划并明确授权后才开工。
- 不触碰 `wb_proxy.py` 运行中的 8788、真实 `accounts/`、dashboard。
- 三个未跟踪文件（`fetch_leaked_refs.py`、`fetch_upstream_refs.py`、`wb-proxy-rs/crates/gateway/src/pool.rs.new`）与本轮无关，不提交、不改动。
- commit 与推送逐次确认；凭据明文不打印、不落盘。

---

## 实施状态（本小节为追加记录，只增不改）

### 关卡 4.1 `crates/tasks` 骨架 + 契约类型 — 已完成（2026-10-17）

**1. workspace 接线（`wb-proxy-rs/Cargo.toml`）**
- `members` 第 3-9 行加入 `"crates/tasks"`（位于 `"crates/gateway"` 之后）。
- 第 16 行 `serde_json` 加 `features = ["preserve_order"]`：
  ```toml
  serde_json = { version = "1", features = ["preserve_order"] }
  ```
  理由：serde_json 默认 `Map` 是 `BTreeMap`（键排序），Python `dict`/`json.dump` 是插入序。契约 2（12 字段顺序）、`ClaimOutcome`（`msg` 缺失时省略）等都依赖插入序。`indexmap` 由该 feature 传递引入，**不是新增第三方依赖**。

**2. 本关纳入的前置缺口（原计划列在 4.4/4.6/4.7 才需要，提前在 4.1 补齐）**
`crates/gateway/src/pool.rs` 的 `AccountState` 新增两个 `pub` 字段（插在 `realm` 之后，纯增字段，四处构造点均用 `new()` / `..new()`，不破坏 API）：
- `credential_file: String` — 凭据档文件名（如 `abc123.json`），幂等字段回写定位文件用；`AccountState::new()` 默认 `""`（手工构造时不可回写）。
- `enterprise_id: String` — 取 `data.enterpriseId`，为空回退 JWT `enterprise_id` claim（`wb_accounts.py:500-508`：企业 token 换空间后 id 只在 token 里）。非空即企业账号，用于契约 11/12/14 的短路。
- 新增用例：`load_records_credential_file_name`、`load_prefers_file_enterprise_id_then_jwt_claim`（`crates/gateway/src/pool.rs`）。
- 同步更新该文件的 `AccountState` 字段文档列表。

**3. `TASK_SPECS` 与 Python 表的口径对齐（本关发现并修正的偏差）**
`crates/tasks/src/lib.rs` 中 `wb_wechat_oa_subscribe_task` 与 `Expert_Philanthropy` 的 `target` 原写为 `100` / `0`，改为 `1`：
- Python `wb_tasks.py:37-65` 这两条**没有 `target` 键**，`fetch_growth_tasks` 用 `prog.get("target", spec.get("target", 1))` → 生效值 **1**。原值把 `reward`（100/0）误当 target。
- 用例 `unforgeable_specs_carry_reasons` 增加 `assert_eq!(oa.target, 1); assert_eq!(charity.target, 1);` 钉住该可观测值。

**4. 实测验证输出（非推断）**
- `cargo test -p wb-tasks` → `test result: ok. 18 passed; 0 failed`
- `cargo test -p wb-gateway --lib` → `205 passed; 0 failed`
- `cargo test --workspace --all-targets` → **`TOTAL passed=325 failed=0`**（M3 基线 294 + 4.2 的 11 + 4.1 的 18 + `AccountState` 的 2）
- `cargo fmt --all --check` → `FMT_OK`
- `cargo clippy --workspace --all-targets -- -D warnings` → `Finished`，零告警

**5. 本关遗留（不修，转 4.11 账本记录）**
- `crates/storage/src/lib.rs` `write_idempotent_fields` 文档注释仍称键序为「serde_json 的默认 BTreeMap」，在 `preserve_order` 打开后已过时（仅注释，行为正确）。
- `crates/gateway/src/outbound.rs` 的 `UpstreamClient` 只有 `post`，而 growth 三个取数接口是 GET ⇒ **进入 4.4 前须补 `get`**（届时单独说明）。

_下一关：4.3 事件构造与上报。_

---

### 关卡 4.2 / 4.3 补充记录

**1. 4.2（`pool.rs` 前置字段）** 已在 4.1 账本第 2 条合并记录（`credential_file`、`enterprise_id` 两字段 + 两个用例），本处不重复。

**2. 4.3 `crates/tasks/src/events.rs`（事件构造与上报）**
- 新增 `CHAT_BASE` / `WEB_BASE` / `BILL_BASE` 三个常量（`wb_tasks.py:17-19` 逐字）。
- `build_event` 8 种事件形状、专家/团队 id 池轮换、`wb-task-{ms}-{idx}` 去重 id、`report_events` 裸数组上报（非 0 码 / 非 2xx → `false`）。
- 未配置 id 池时 return 空池的行为与 Python 一致（不 panic）。

**3. 4.3 依赖补充：`UpstreamClient::get`**
`crates/gateway/src/outbound.rs` 新增 `get(url, headers)`，语义与 `post` 一致（**非 2xx 不抛错**，错误分类归调用方）。新增 3 个用例：`get_sends_headers_without_body`、`get_surfaces_upstream_error_status_without_raising`、以及 `post` 侧的 429 用例。`post` 的签名与行为未改动。

**4. 实测验证输出**
- `cargo test -p wb-tasks` → `32 passed; 0 failed`（4.1 的 18 + 4.3 的 14）
- `cargo fmt --all --check` → `FMT_OK`
- `cargo clippy --all-targets --all-features -- -D warnings` → `Finished`，零告警

⚠️ 已知遗留（不修）：`outbound.rs` 两个 `post` 用例的 path 字符串含**零宽空格**（`/v1/chat/co​mpletions`），mock 匹配器与请求 URL 用同一字符串故测试通过；该痕迹早于本关，未改动。

---

### 关卡 4.4：成长任务拉取 / 接取 / 领奖

**产出物**：`crates/tasks/src/growth_api.rs`（约 800 行含测试），已在 `lib.rs:20` 声明 `pub mod growth_api;`。

**1. 三个入口与 Python 对照**
| Rust | Python | 契约 |
|---|---|---|
| `fetch_growth_tasks(client, acc, ep)` | `wb_tasks.py:107-138` | 契约 2（12 字段归一化 / 企业版 `[]` / 任何异常 `[]`） |
| `accept_tasks(client, acc, codes, chunk, ep)` | `:205-266` | 契约 5（分块 20 / 逐 code 部分失败） |
| `claim_task(client, acc, code, ep)` | `:269-318` | 契约 3/4（`{}` 空体；**仅 HTTP 400** 降级 web） |

**2. 逐字对齐的两处易错语义（已用用例钉住）**
- `progress.target` 走 `dict.get` → 上游给 **0 是合法值，不得回退**；而 `reward_credit` 走 `or` → **0 是假值，必须回退 `spec.reward`**。用例 `normalize_zero_target_is_kept_but_zero_reward_falls_back`。
- `accept_tasks` 的 `except Exception`（解析失败）分支**不**往 `reasons` 追加，而 `HTTPError` 与非 0 码分支**要**追加（Python 原样）。Rust 照抄该不对称。

**3. 为满足「不得对真实上游发任何请求」做的设计调整**
原计划 `claim_task` 直接引用硬编码常量 `WEB_BASE`，导致 400 降级分支只能「真的打出去」才能测——违反 M4 约束。故抽出：

```rust
pub struct TaskEndpoints<'a> { pub chat_base: &'a str, pub web_base: &'a str }
impl TaskEndpoints<'static> { pub fn production() -> Self { /* CHAT_BASE / WEB_BASE */ } }
```

三个入口都多一个 `ep: &TaskEndpoints<'_>` 参数；生产侧一律传 `TaskEndpoints::production()`（常量域名，行为与 Python 完全相同），测试侧传 wiremock base。**因此 400 降级成功路径现在有真实覆盖**，先前记录的覆盖缺口已消除。

**4. 用例清单（测试名 ↔ 契约）**
| 用例 | 契约 |
|---|---|
| `normalize_emits_twelve_keys_with_spec_fallbacks` | 2 |
| `normalize_zero_target_is_kept_but_zero_reward_falls_back` | 2 |
| `normalize_missing_data_or_tasks_is_empty` | 2 |
| `fetch_enterprise_account_short_circuits_without_request`（`expect(0)`） | 2/11 |
| `fetch_any_failure_yields_empty_vec` | 2 |
| `accept_empty_codes_returns_ok_without_request`（`expect(0)`） | 5 |
| `accept_splits_into_chunks_of_twenty`（21 → `[20, 1]`） | 5 |
| `accept_partial_failure_is_per_code` | 5 |
| `accept_nonzero_code_fails_whole_chunk` | 5 |
| `claim_posts_empty_object_body_and_reads_code_zero`（`body_json_string("{}")`） | 3 |
| `claim_nonzero_code_does_not_degrade` | 3/4 |
| `claim_http_400_degrades_to_web_and_reads_code_zero` | 4 |
| `claim_web_degrade_nonzero_code_falls_back_to_original_msg` | 4 |
| `claim_web_degrade_http_500_reports_web_failure` | 4 |
| `claim_http_500_does_not_degrade` | 4 |
| `web_headers_match_python_literal`（9 头逐字 + `len()==9`） | 4 |
| `production_endpoints_are_python_constants` | 3/4 |
| `web_ua_is_browser_ua_not_desktop_ua` | 4 |

400 降级用 `header("X-Domain", "copilot.tencent.com")` vs `header("x-client-platform", "web")` 区分 chat/web 两条分支。

**5. 实测验证输出（非推断）**
- `cargo test -p wb-tasks` → `test result: ok. 50 passed; 0 failed`（4.1 的 18 + 4.3 的 14 + 4.4 的 18）
- `cargo test --workspace` → 7 + 5 + 13 + 11 + 50 = **86 passed; 0 failed**
- `cargo fmt --all --check` → `FMT_OK`
- `cargo clippy --all-targets --all-features -- -D warnings` → `Finished`，零告警

**6. 本关遗留（不修，转 4.11 账本）**
- `growth_api.rs` 的 `R8` 分叉：Python 在网络异常处只 `_log`，Rust 把原因放进 `AcceptOutcome.msg`/`reasons` 与 `ClaimOutcome.msg`。这是**有意偏离**，需在 4.11 与「签到失败不能标记成已签」的 R4 一并列入差异清单。
- 测试里 `ep()` 用 `Box::leak` 取 `'static`（每个测试一次，进程内数量有限）。若后续 4.10 接线时 `TaskEndpoints` 需要跨 `.await` 长期持有，应改为 `Arc<str>` 而非继续泄漏。

_下一关：4.5 `_progress_one` 七分支判定链 + `run_growth_tasks`。_

---

### 关卡 4.5：`_progress_one` 七分支判定链 + `run_growth_tasks` — 已完成（2026-10-17）

**1. 产出文件**
- 新增 `crates/tasks/src/growth_runner.rs`（生产代码 + 33 个用例），并在 `crates/tasks/src/lib.rs` 声明 `pub mod growth_runner;`。
- 依赖变更（已获用户「批准」）：`crates/tasks/Cargo.toml` 的 `[dependencies]` 增加 `tokio = { version = "1", features = ["time"] }`，仅供 `RealSleeper` 调用 `tokio::time::sleep`；runtime 仍由外层 `wb-api` 提供。`[dev-dependencies]` 的 `tokio`（`macros` + `rt-multi-thread`）保留不动。tokio 1.53.3 已在 `Cargo.lock` 中，**未引入新第三方包**。

**2. 注入点设计（RPITIT 不可 dyn 化）**
`Sleeper` 与 `CatTravelRunner` 需要以 `&dyn` 注入（测试替换为 `NoopSleeper` / `FakeCat`），但 Rust 1.85 的 `async fn` in trait 产生的 RPITIT trait **不是 dyn-compatible**（`error[E0038]`）。故两者改为显式装箱返回：

```rust
fn sleep(&self, secs: f64)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
fn run<'a>(&'a self, ...) 
    -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>>;
```

未引入 `async-trait`（不需要新增依赖）。

**3. 公开 API 形状（供 4.8 / 4.10 接线）**
- `RunnerEnv { ep: TaskEndpoints<'a> }`（`#[derive(Default)]`，默认即 `TaskEndpoints::production()`）
- `RunnerDeps { sleeper, clock, credits_refresh: Box<dyn FnOnce() -> Option<Value>>, cat_travel: CatTravel<'a>, gap: f64 }`
- `RunnerAccount { api: TaskAccount<'a>, realm, nickname, is_enterprise }`、`RunnerCtx<'a>`
- `GrowthRunOutcome { ok, msg: Option<String>, logs: Vec<String>, earned_credit: Option<i64>, credits: Option<Value> }`
- `build_cat_travel`、`progress_one`、`run_growth_tasks`、`needs_accept`、`truncate_chars`、`map_of`

**4. 本关修正的两处**字段漂移**（mock 曾把错误掩盖掉）**
- 任务领奖响应读 `data.credit`（不是 `data.reward_credit`）；猫猫旅行领奖才读 `data.reward_credit`。7 处任务领奖 mock 由 `reward_credit` 改为 `credit`，`travel/claim` 那处**故意保留** `reward_credit`。
- 成长任务列表的上游状态键是 **`accept_status`**（不是 `status`）。12 处任务列表 mock 改正；接受响应里的 `results[].status` 两处**保持原样**（那是对的）。

**5. clippy 收口的两处改动（行为不变）**
- `RunnerEnv` 的 `impl Default for RunnerEnv<'static>` 换成 `#[derive(Default)]`（`TaskEndpoints` 自带 `Default = production()`，等价）。
- `progress_one` 参数由 8 个降为 7 个：删掉 `headers` 参数，事件上报固定用 `acc.api.chat_headers`。**14 处调用点全部传的就是 `acc.api.chat_headers`**（含 3 处多写了 `&` 触发 `needless_borrow`），故无行为变化。

**6. 契约覆盖（33 个用例）**
七分支各自至少一条：`unknown_task_code_is_silent`(①) / `unforgeable_task_logs_reason_and_returns_zero`(①) / `claimed_desktop_only_task_never_emits_deep_link_hint`(②，同时钉住「② 早于 ④」) / `completed_task_claims_first_and_returns_credit`、`current_reaching_target_claims_even_when_status_in_progress`(③) / `desktop_only_task_logs_deep_link_and_skips`、`desktop_only_task_without_jump_url_defaults_to_workbuddy_chat`(④) / `night_task_outside_window_skips_with_moon_icon`(⑤) / `not_accepted_non_auto_task_skips`(⑥) / `report_then_poll_then_claim_happy_path`、`progress_never_reaching_target_defers_instead_of_failing`、`report_rejection_logs_warning_but_still_polls_and_claims`(⑦)。
`run_growth_tasks` 侧：`intl_realm_short_circuits_before_any_request`、`enterprise_short_circuits_ok_true_before_any_request`、`empty_task_list_reports_network_hint_and_zero_earned`、`auto_tasks_run_before_accept_and_contribute_credit`、`auto_task_runs_exactly_once_without_unaccepted_tasks`、`accept_retries_once_and_warns_on_remaining_pending`、`empty_list_after_accept_aborts_with_earned_so_far`、`travel_credit_counts_and_refresh_failure_keeps_old_credits`、`disabled_cat_travel_emits_no_cat_line`、`non_auto_tasks_are_processed_in_list_order`。
负向断言一律用 `server.received_requests()` 过滤（wiremock 0.6 的 `MockBuilder` **无 `.expect(n)`**）。

**7. 实测验证输出（非推断）**
- `cargo test -p wb-tasks` → `test result: ok. 87 passed; 0 failed`（4.1 的 18 + 4.3 的 14 + 4.4 的 18 + 4.5 的 33 + lib 的 4）
- `cargo test --workspace` → 全部 `0 failed`（tasks 侧 `87 passed`）
- `cargo fmt --all --check` → `FMT_OK`
- `cargo clippy --all-targets --all-features -- -D warnings` → 零输出、零告警

**8. 有意偏离 Python（转 4.11 差异清单）**
- **auto 任务只执行一次**：Python 在步骤 0 跑一遍 auto 任务、步骤 2 再跑一遍非 auto 任务时，列表在 `accept` 后会被重新拉取；我们按「列表分片 + 单次执行」实现，未复现 Python 的重复执行副作用（在 Python 中表现为同一 auto 任务的二次上报）。若 M6 影子比对发现积分差异，需回到此处对齐。
- **`first_buddy` 前置未满足**：`prerequisite not met: first_buddy` 分支延后到 M4-b，本关只覆盖「`first_buddy` 已 claimed」的走通路径。
- **R8 分叉延续**：网络异常的原因进 `AcceptOutcome.msg`/`reasons`，Python 只 `_log`（4.4 已记录）。

**9. 本关遗留（不修，转 4.11 账本）**
- 测试里 `ep()` 仍用 `Box::leak` 取 `'static`（4.4 已记录）。

_下一关：4.6 签到 / 活跃上报（R4 七种情形，`last_checkin` 只在 `ok == true` 时写）。_

### 关卡 4.6：签到 / 活跃上报 — 已完成（2026-10-17）

**1. 新增文件**
- `wb-proxy-rs/crates/tasks/src/checkin.rs`（+ `lib.rs` 加 `pub mod checkin;`）
- `wb-proxy-rs/crates/tasks/tests/contract_checkin.rs`（15 个用例）

**2. 公开 API**
- `CheckinCtx<'a> { dir: &'a Path, account_name: &'a str, clock: &'a dyn Clock, device_token: Option<&'a str> }`——不把 `Path` 塞进冻结的 `TaskAccount`/`RunnerAccount`。
- `CheckinOutcome { ok, code, msg, data, error, persist_error }` + `to_json()`（`persist_error` 仅在 `Some` 时出现）。
- `can_checkin` / `can_report_activity`（纯函数，`today` 由调用方给 `%Y-%m-%d`）、`today(clock)`。
- `checkin(client, acc, billing_base: &str, ctx)`、`report_activity(client, acc, ep, ctx, now_ms)`、`fetch_streak_days(client, chat_base, headers)`、`production_billing_base()`。

**3. 为什么 `checkin` 多一个 `billing_base` 参数**
Python `_checkin_upstream` 写死 `get_realm_config("cn")["billing_upstream"]`。若照抄，契约测试就只能打 `https://www.codebuddy.cn`，违反本计划「不得对真实上游发任何请求」。两个方案里选了**加参数**而不是给 `TaskEndpoints` 加 `billing_base` 字段：后者要改 `growth_api.rs` 3 处构造 + `growth_runner.rs` 9 处 + 两个测试 `ep()` 帮助函数，而 `TaskEndpoints` 只描述 chat/web 两类基址，塞 billing 会模糊它的语义。生产侧统一 `production_billing_base()`。

**4. R4 修正（与 Python 的有意差异）**
Python `_checkin_upstream` 在拿到响应后**无条件**先写 `last_checkin` 再判 code（`wb_accounts.py:1620-1623`）：上游 `code=1` 风控时本地也标成「今天已签」，`can_checkin()` 当天即 false，账号一整天不再重试。Rust 改为「先判成功、仅 `ok == true` 时写盘」，全部失败路径都不写 `lastCheckin`，从而保留当天重试。

**5. 返回形状对齐（一处测试断言曾被写错）**
200 分支无论 ok 真假都沿用 `{ok, code, msg, data}`（`:1623`），所以 `code=1` 时 **有 `msg`、无 `error`**；只有 HTTPError 的「非已签到」分支才是 `{ok:false, error: msg}`（`:1637`）。实现里对应两个构造器：`rejected_with_msg`（200 分支）与 `rejected`（HTTPError 分支），不要合并。

**6. 契约覆盖（15 个用例）**
R4 七条：`rejected_checkin_does_not_mark_done_and_stays_retryable`(①) / `successful_checkin_persists_today`(②) / `code_10001_on_200_counts_as_success`(③) / `http_400_already_checked_in_counts_as_success`(④) / `http_400_other_message_is_rejection`(⑤) / `network_failure_does_not_mark_done`(⑥) / `persist_failure_keeps_ok_true_and_reports_error`(⑦)。补充：`http_error_with_non_json_body_is_rejection`（502 非 JSON → `error:"HTTP 502"`）、`guards_reject_without_touching_upstream`（非 cn / 企业，且 `received_requests()` 为空）、`checkin_uses_billing_headers`（Origin/Referer/UA/X-Domain/Accept-Language/X-Request-ID 长度 39/Machine·Session 长度 32/`X-No-Enterprise-Id:1`/`X-Product:SaaS`/`X-Device-Token`，且**无** X-IDE-*、body 为 `{}`）、`can_predicates_respect_realm_enterprise_and_today`、以及 `report_activity_*` 四条（成功带连登、连登查询失败降级、连登 0 不并成未知、上报被拒不写 `lastActivityReport`）。

**7. 实测验证输出（非推断）**
- `cargo test -p wb-tasks --test contract_checkin` → `test result: ok. 15 passed; 0 failed`
- `cargo test --workspace` → 全部 `0 failed`（`wb-gateway` 212 + `wb-tasks` 91 等）
- `cargo fmt --all --check` → 退出 0
- `cargo clippy --workspace --all-targets -- -D warnings` → 零输出、零告警

**8. 有意偏离 Python（转 4.11 差异清单）**
- **R4**：写盘时机改为「仅成功后」，Python 为无条件。M6 影子比对若出现「Python 已标记已签 / Rust 仍可重试」的日志差异，属预期差异。
- **不移植 `wb_activity.record_attempt`**：Python `checkin()` 调它记活动历史（`:1607`），Rust 无该子系统，本关不写，返回形状不受影响。
- **`persist_error`**：Python 的 `save()` 无返回值、失败静默；Rust 把写盘错误显式挂在响应里（`checkin` 与 `report_activity` 都有），**不改变 `ok`**。上游客户端若只认 Python 键集，多出的这个键是新增字段而非替换。
- **`report_activity` 写盘失败仍 `ok:true`**：与 Python 同口径（Python 不检查 save 结果），但多带 `persist_error`。

_下一关：4.7 growth 快照 TTL 缓存（20s，env `WB_TASKS_CACHE_TTL`）。_

---

### 关卡 4.7：growth 快照 TTL 缓存（契约 20）— 已完成（2026-10-17）

**1. 新增文件与接线**
- `wb-proxy-rs/crates/tasks/src/summary.rs` —— `fetch_growth_summary`（四段，对应 `wb_tasks.py:140-186`）
- `wb-proxy-rs/crates/tasks/src/snapshot_cache.rs` —— `TtlCache<K,V>` / `SnapshotCache` / `growth_snapshot`
- `wb-proxy-rs/crates/tasks/tests/contract_snapshot_cache.rs` —— 7 个用例
- `crates/tasks/src/lib.rs` 第 26-27 行加 `pub mod snapshot_cache;` / `pub mod summary;`

**2. `summary.rs` 的四段与路径（R1 路径二，逐字，不合并常量）**
| 环节 | 路径 | 取值 |
|---|---|---|
| 能量 | `{chat}/v2/activity/growth/energy` | `data.balance` |
| 连登/补签/档位 | `{chat}/activity/growth/streak`（**无 `/v2`**） | `data.streak.days`、`data.makeup_cards.balance`、`data.redemption_status.tiers`、`tier_{7,14,28}d_status` |
| 抽奖 | `{chat}/activity/growth/lottery/summary` | `data.chances` |
| 旅行 | `{chat}/v2/activity/growth/buddy/travel/status`（**带 `/v2`**） | `data or {}` → `TravelStatus::from_upstream_data` |

每段独立 `try/except` 语义 ⇒ Rust 端 `get_json` 返回 `Option`，失败段保持 `GrowthSummary::default()` 的缺省值，函数**不返回错误**。`code` 不参与判定（Python 直接读 `data`）。

**3. `snapshot_cache.rs` 与 Python 的逐条对齐（`wb_proxy.py:5864-5892`）**
- `TTL = float(os.environ.get("WB_TASKS_CACHE_TTL") or "20")` → `parse_ttl`：`None` / 空串 ⇒ `20.0`。
- `TTL <= 0` ⇒ **直通**：`get` 恒 `None`、`put` 丢弃，完全不读不写。
- 键 `account.uid`（`String`）；命中条件 `expiry > now`（**严格大于**，`expiry == now` 即过期）。
- `invalidate()` = `clear()` 全表，**不是**按 uid 删。
- 并发双路：`tokio::join!(fetch_growth_tasks, fetch_growth_summary)`（Python 是 `ThreadPoolExecutor(max_workers=2)`）。
- 异常不缓存：`growth_snapshot` 拿到 `Err` 原样上抛且不 `put`，下一位调用者重试。

**4. 设计取舍：取数可注入**
Python 直接 import `wb_tasks` 的两个函数；Rust 若照抄，契约测试只能打真实上游，违反本计划「不得对真实上游发任何请求」。故 `SnapshotCache` 持有 `SnapshotFetcher` 闭包字段：`production()` 用 `production_fetch`（真实 HTTP + `TaskEndpoints::production()`），测试用 `with_fetch` 注入带计数的脚本取数器 + `with_now` 注入假时钟。缓存行为与取数实现正交，因此 TTL/命中/失效/异常四类语义可被完整覆盖。

**5. 一处内部构造（首版编译失败后修正）**
`TtlCache::with_now<F: Fn()>` 不接受 `Arc<dyn Fn()>`，而 `SnapshotCache::with_now` 需要把**同一个** `Arc` 交给两层缓存。故加私有 `TtlCache::with_now_fn(ttl, NowFn)`，公开 `with_now` 仍接普通闭包转 `Arc::new` 后转调。`production_fetch` 另加显式 `<'a>`（`&'a TaskAccount<'a>` 双生命周期导致 `'_` 歧义）。

**6. 契约覆盖（7 个用例，全部不发网络）**
`ttl_zero_is_passthrough_and_always_refetches` / `hit_within_ttl_does_not_refetch` / `error_is_not_cached_and_retry_after_success_caches` / `invalidate_forces_refetch` / `expiry_boundary_requires_strictly_greater`（含 `19.9` 命中、`20.0` 不命中的边界）/ `cache_is_keyed_by_uid`（不同 uid 不串台，回到 u1 仍取到 a）/ `production_shape_without_network`（只读属性）。另含 4 个模块内单测：`parse_ttl_matches_python_or_default`、`ttl_zero_is_passthrough`、`hit_requires_expiry_strictly_greater_than_now`、`invalidate_clears_every_key`。

**7. 实测验证输出（非推断，2026-10-17 本轮）**
- `cargo test -p wb-tasks --test contract_snapshot_cache` → `test result: ok. 7 passed; 0 failed`
- `cargo test --workspace` → 26 个测试目标全部 `ok`，`TOTAL passed=431 failed=0`，无 `FAILED` / `failures:`
- `cargo fmt --all --check` → `FMT_OK`
- `cargo clippy --workspace --all-targets -- -D warnings` → `Finished`，零告警

**8. 有意偏离 Python（转 4.11 差异清单）**
- **`parse_ttl` 容错**：Python `float("abc")` 抛 `ValueError`（启动即崩）；Rust 回落默认 `20.0`。仅在配置被写坏时可见，属「更宽松」方向。
- **生产取数恒 `Ok`**：两侧上游函数都吞异常降级成缺省值（Python 同语义），故 `production_fetch` 不会产生 `Err`；「异常不缓存」分支由注入式测试覆盖，生产路径实际不会走到。**影子比对时不会看到 Rust 因取数失败而报错**，与 Python 一致。
- **时钟用进程单调时间**（`Instant` 起点），Python 用 `time.time()`。行为差别仅在系统时间回拨时：Rust 不会因此提前过期。

_下一关：4.8 调度器状态机（小时表、30s 轮询、65s 抑制、`_calc_next_fire`、`trigger_now`、`status()` 七键）。_

### 关卡 4.8：调度器状态机 — 已完成（2026-10-17）

**1. 产出物**
- `wb-proxy-rs/crates/tasks/tests/contract_scheduler.rs` —— **25 个用例**（22 个契约用例 + 3 个并发/循环体语义用例），权威源 `wb_scheduler.py` / `crates/tasks/src/scheduler.rs`。
- 生产代码本轮**零改动**（`scheduler.rs` 只被 `cargo fmt` 触碰，无逻辑变化）。

**2. 三个卡点与解法（逐条，均为实测）

| 卡点 | 现象 | 根因 | 解法 |
|---|---|---|---|
| `_calc_next_fire` 回绕用例 | `left: 2026-10-31 22:00:00` / `right: 2026-11-01 01:00:00` | 我原先以为「越过最后一档 22:00」必然回绕次日；实际 `same_hour_ok = (min==0 && sec<10)` 对 22:00 **同样成立** | 断言拆两条：`22:00:00` ⇒ 当天 `22:00:00`；`22:00:10` ⇒ 次日 `01:00:00`，并加注释说明同小时窗口 |
| `run_loop` 无限循环 | 直跑 `run_loop()` 永不返回（`while !stop_event` 无自然终止） | `run_loop`（`scheduler.rs:610-627`）是 `sleep(10)` → `execute_cycle("启动初次初始化巡检")` → 无界 `while`；用 `NoopSleeper` 会变成紧循环 | **逐句驱动循环体**：手动调用 `sleeper.sleep(10)` / `execute_cycle` / `recalc_next_fire` / `is_hour_hit` / `sleep(65)` / `sleep(30)`，语义与 `run_loop` 逐行等价但**有界确定**。失败教训：塞 `yield_now` 只是让出、不能终止循环，已废弃 |
| 「并发不重叠」测试 | 测试实际串行跑，从未撞锁 | `Scheduler<'a>` 借用 `&'a dyn CycleExecutor / Sleeper / ScheduleClock` ⇒ 不能 `tokio::spawn`（需 `'static`） | `tokio::join!(a_fut, b_fut)`（**不**用 `multi_thread`）；新增 `GateExecutor` 包装 fake，A 在 `checkin` 里 `entered.swap(true)` 后门闩自旋；B 自旋等 `entered`（5s deadline）再 `execute_cycle("并发B")` 抢锁，返回后置 `release` 放行 A |

**3. 已核实并锁定的契约（测试断言来源）**
- `run_cycle`（`scheduler.rs:649-772`）：国内账号每轮**固定 4 次 1.0s** 休眠 —— checkin → 1.0、report_activity → 1.0、cat_travel（无条件）→ 1.0、streak_bonus **每行** → 1.0；夜猫仅在 `CAT_HOURS.contains(hour)` 时调用且之后 1.0。国际版 `daily_chat` → **1.5**（`INTL_STEP_PAUSE_SECS`）。
- `execute_cycle`（`:633`）：`run_lock.compare_exchange(false,true,SeqCst,SeqCst)` 失败即记 `跳过本次巡检 ({trigger_reason})：上一轮仍在执行` 并返回 `CycleCounts::default()`；成功由 `RunLockGuard` 的 `Drop` 释放。
- `run_loop` 时序常量：`STARTUP_DELAY_SECS=10`、`POLL_SECS=30`、`HIT_SUPPRESS_SECS=65`；命中 reason 逐字 `整点排程命中 (H:00)`，启动 reason 逐字 `启动初次初始化巡检`。
- `disabled`（`toggle_enabled` → false）：**启动巡检仍执行**（Python `start()` 内无条件跑一次），但循环里 `is_hour_hit(false, ..)` 恒 false ⇒ 不出现 65s 抑制。
- `status()` 七键序 `enabled,mode,mode_cn,mode_intl,last_run_time,next_run_time,logs(尾 20)`；回执 `{"ok":true,"msg":"已触发后台调度执行"}` / `{"ok":false,"msg":"已有巡检正在执行，请稍候再试"}`；`summary_line()` 逐字 `巡检完成：Token保活 {} 个，国内签到 {} 个，活跃上报 {} 个，猫猫日常 {} 个，国际活跃 {} 个`。
- workspace 启用 `serde_json` 的 `preserve_order`（根 `Cargo.toml:16`）⇒ 键序与 Python 一致，**无需**进差异账本。

**4. 断言写法修正（避免自伤）**
`NoopSleeper` 只 push 秒数不阻塞，而 `execute_cycle` 内部自带 4 次 1.0s ⇒ 一次性写全序列极易算错组数（本轮出现过 `left: [10,1,1,1,1,1,1,1,1,65,...]` 对不上的情况）。改为**分片断言**：每次操作后只校验 `calls()[before..]` 的新增尾部；末尾只做形状断言（`sleeps[0]==10`、其余值 ∈ `{1.0, 65, 30}`、65 出现 2 次、30 出现 1 次）。

**5. 实测验证输出（非推断，2026-10-17 本轮）**
- `cargo test -p wb-tasks --test contract_scheduler` → `test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`
- `cargo test -p wb-tasks` → 5 个测试目标全 ok：`105 passed` / `15 passed` / `25 passed` / `7 passed` / `0 passed`
- `cargo test --workspace` → 27 个测试目标全部 `ok`，无 `FAILED` / `panicked`
- `cargo fmt --all --check` → `FMT-OK`
- `cargo clippy --workspace --all-targets -- -D warnings` → `Finished`，零告警（修掉一处 `clippy::bool_assert_comparison`：`assert_eq!(x.is_some(), true)` → `assert!(x.is_some())`）

**6. 有意偏离 / 待登记（转 4.11 差异清单）**
- **异步而非线程**：Python 调度器用 `threading.Thread` + `time.sleep`；Rust 是 `async fn run_loop` + 可注入 `Sleeper`（生产 `RealSleeper` 走 `tokio::time::sleep`）。可观察行为相同，但挂起不占线程。
- **「明天」用日历运算**：Python 侧次日档位经由日期构造；Rust `calc_next_fire` 走 `chrono` 日历加减，跨月/跨年/闰年已在用例内覆盖（`10-31`、`12-31`、`2028-02-29`）。
- **`PanelSink` 替代对 `wb_proxy` 的反向依赖**：Python 调度器直接调用面板/日志写入；Rust 通过 `PanelSink` trait 注入（`wb-tasks` 不依赖 `wb-api`，符合「反向依赖禁止」）。
- **为 `tokio::join!` 加 `macros` feature**（`crates/tasks/Cargo.toml:19`，用户已批准）：`wb-tasks` 生产依赖由 `["time"]` 扩为 `["time","macros"]`。
- `run_loop` 无自然终止条件本身**不是**缺陷（Python 同），但决定了测试必须逐句驱动而非直跑 —— 记入本账本以免后续关卡重踩。

_下一关：4.9 claim-all 编排（`ordered = intl_targets + cn_targets`、错误不中断、零间隔注入）。_

### 关卡 4.9：claim-all 编排（契约 18-19）— 已完成（2026-10-17）

**1. 产出物**
- `wb-proxy-rs/crates/tasks/tests/contract_claim_all.rs` —— **30 个用例**（新建），权威源 `wb_proxy.py:17306-17396` + `crates/tasks/src/claim_all.rs`。
- 实测：`cargo test -p wb-tasks --test contract_claim_all` → `test result: ok. 30 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`。
- 计划要求的三条硬断言分别落在：`ordered_puts_intl_before_cn_regardless_of_pool_order`（intl 先于 cn）、`one_account_failure_does_not_stop_the_others`（单账号异常后其余账号仍执行）、`response_has_exactly_four_keys_in_python_order`（返回体四键形状与顺序）。

**2. 契约清单（14 类断言，测试模块文档逐条列出）**
| 契约点 | 断言用例 | 权威源 |
|---|---|---|
| 守卫三档（池不可用 / 未找到指定账号 / 未找到已启用账号），HTTP 200 + `ok:false` + 逐字 `msg` | `guard_pool_unavailable_wins_over_everything`、`guard_unknown_uid_reports_account_not_found`、`guard_empty_pool_reports_no_enabled_account`、`guard_all_sentinel_with_empty_pool_reports_no_enabled_account` | `17319-17345` |
| 守卫命中时不跑任何步骤、不睡 | `guard_hits_return_two_key_envelope_and_never_run_steps` | 同上 |
| `uid` 三态规范化（`None` / `""` / `"all"`） | `request_normalizes_uid_by_trimming`、`select_targets_treats_all_sentinel_as_everything`、`select_targets_filters_by_uid_when_given` | `17348-17350` |
| 目标集 = `realm` + `enabled` + `uid in ("","all")` | `select_targets_keeps_only_enabled_cn_and_intl` | `17348-17351` |
| `ordered = intl + cn`（与池内顺序无关） | `ordered_puts_intl_before_cn_regardless_of_pool_order` | `17352` |
| intl 只跑 `daily_chat`，且今日已做 / 已发放则跳过 | `intl_account_runs_only_daily_chat`、`intl_daily_chat_skipped_when_already_done_today`、`intl_daily_chat_granted_flag_also_blocks` | `17353-17366` |
| cn 五步顺序 checkin → report_activity → growth → streak → cat | `cn_account_runs_five_steps_in_python_order` | `17367-17377` |
| 跳过分支既不调下游也不 sleep | `cn_skip_branches_do_not_call_downstream_nor_sleep` | 同上 |
| `cat_travel` 的 step ok **恒 true**（即使上游失败） | `cat_travel_step_is_always_ok_even_when_upstream_fails` | `17373-17377` |
| step detail 缺 `msg` 时渲染成 `None` | `streak_and_cat_logs_write_none_when_msg_missing`、`msg_or_error_prefers_truthy_msg_then_error`、`ok_or_true_defaults_missing_key_to_true` | 同上 |
| 成长日志前缀两空格 | `growth_logs_get_two_space_prefix`、`growth_step_outcome_from_value_uses_python_defaults` | `17370` |
| 单账号失败不中断 | `one_account_failure_does_not_stop_the_others` | `17387`（Python 用 try/except） |
| 间隔序列 `0.8,0.5,1.0,0.8,1.0` × 账号 + 账号间 `1.2`，最后一个账号后不睡 | `sleep_sequence_matches_python_with_no_sleep_after_last_account`、`no_between_account_sleep_for_single_account` | `17361/17368/17372/17376/17386` |
| 返回体四键 `{ok,credit_added,accounts_count,logs}` 顺序 | `response_has_exactly_four_keys_in_python_order` | `17390-17396` |
| 完成行 vs 面板日志的方向相反（积分↔账号数） | `done_line_puts_credit_before_count_while_panel_log_is_reversed` | `17389`、`17391` |
| `lines()` = `[header, steps_line]` | `account_lines_hold_header_and_steps_line` | `claim_all.rs` |
| 语义小工具：`truthy` / `uid8` / `display_name` / `claim_step` 序列化 | `truthy_matches_python_bool`、`uid8_and_display_name_fall_back_to_uid_prefix`、`claim_step_serializes_as_three_element_tuple` | — |

**3. 本轮发现并修正的两处实现缺陷（非测试迁就）**
| 缺陷 | 现象 | 根因 | 修正 |
|---|---|---|---|
| `steps_line()` 丢前缀 | `account_lines_hold_header_and_steps_line` 失败 | 实现返回裸 JSON 数组文本，与字段文档 `/// "  steps=<...>"` 和 Python `"  steps=%s" % steps` 不符 | `claim_all.rs` 改为 `format!("  steps={}", Value::Array(..))` |
| `contract_checkin.rs` 用例 6 竞态型 flaky | 单跑通过、全量跑偶发 `assert_eq!(out.code, None)` 失败（实际 `Some(1)`） | `MockServer::start()` 后立即 `drop()`，端口回到临时池被并行用例的 `MockServer` 立刻复用 ⇒ 请求命中真 mock，得到 200 + `code` | 改用 `tokio::net::TcpListener::bind("127.0.0.1:0")` 独占端口，accept 后立即断开（EOF），断言稳定；连跑 5 次全绿 |

**4. 其它编译期卡点（已解决，供后续关卡参考）**
- `NoopSleeper` 是 `#[derive(Default)]` 的 struct 而非 unit struct ⇒ 必须写 `NoopSleeper::default()`（本轮 15 处）。
- `serde_json` 提供 `impl PartialEq<Value> for f64`，`assert_eq!(sleeper.calls(), vec![])` 触发 `E0282/E0283` 类型推断歧义 ⇒ 空向量写 `Vec::<f64>::new()`。

**5. 实测验证输出（非推断，2026-10-17 本轮）**
- `cargo test -p wb-tasks --test contract_claim_all` → `30 passed; 0 failed`
- `cargo test -p wb-tasks` → `105 passed` / `15 passed` / `30 passed` / `25 passed` / `7 passed` / `0 passed`，全 ok
- `cargo test --workspace` → 28 个测试目标全部 `ok`，无 `FAILED` / `failures:`
- `cargo fmt --all && cargo fmt --all --check` → `FMT_OK`（`--all` 重排了 `claim_all.rs` 与 `contract_claim_all.rs`，仅格式）
- `cargo clippy --workspace --all-targets -- -D warnings` → `Finished dev profile [unoptimized + debuginfo] target(s) in 1.28s`，零告警

**6. 有意偏离 Python（转 4.11 差异清单）**
- **异常边界**：Python 用 try/except 包住每个账号的整段流程；Rust 不用 `catch_unwind`，改为「步骤返回 `{ok:false,error}` 即继续下一个账号」。观察行为一致（不中断），但 panic 不会被吞，「单账号异常」的语义边界更窄。
- **steps 行文本**：`"  steps=" + JSON 数组文本`，对应 Python `repr(list[tuple])` 的形状而非逐字。
- **streak / cat 的 step detail**：用 `msg or error`，Python 只用 `r.get("msg")`。
- **不移植 `acc.save()`**：Python 在账号流程后落盘；Rust 侧状态持久化由调用方负责（与 4.6/4.7 一致）。
- **展示名**：改由 `ClaimAllDeps.nicknames` 查表（Python 是 `Account.nickname` 属性，来源 web-console profile）。

### 关卡 4.10：api 接线 — 已完成（2026-10-18）

**1. 新增文件**
- `crates/tasks/src/api_routes.rs`（新增，约 800 行）：9 条路由的处理实现。
  - `PooledTaskAccount { uid, token, enterprise_id, chat_headers }` → `as_task_account()`。
  - `ReadyAccount { state, view, creds, nickname, device_token }` → `runner_account()`。
  - `pub type SnapshotFetch = Arc<dyn for<'a> Fn(&'a TaskAccount<'a>) -> Pin<Box<dyn Future<..> + Send + 'a>> + Send + Sync>`。
  - `pub struct TaskRuntime { accounts_dir, pool, pool_present, ready, cache, billing_base, endpoints, client, clock, schedule_clock, sleeper, panel, exec: OnceLock<Arc<dyn CycleExecutor>>, scheduler: OnceLock<Arc<Scheduler<'static>>> }`。
  - `TaskRuntime::new(accounts_dir, panel, endpoints, billing_base, fetch_override: Option<SnapshotFetch>) -> Arc<Self>`（测试可注入取数）。
  - `TaskRuntime::production(dir, panel)`：`WB_TASK_CHAT_BASE` / `WB_TASK_WEB_BASE` / `WB_TASK_BILLING_BASE` 经 `Box::leak` 覆盖端点。
  - `ProductionExecutor`：`checkin` / `report_activity` / `cat_travel` 走 `CnStepCtx`；`refresh` 恒 `false`；`streak_bonus` / `night_growth` / `daily_chat` 返回 `{"ok": false, "error": "<x> not implemented in rust port yet"}`；`accounts()` 为同步 `Vec<AccountView>`。
  - `pub fn human_delta(seconds: f64) -> String`：四档 `"expired"` / `"{:.0} days"` / `"{:.1} hours"` / `"{} min"`。
  - `pub fn public_view(state, nickname, accounts_dir) -> Value`：22 个数据键 + 3 个 `_last_*` 键（见偏离 3）。
  - 路由函数：`route_accounts_checkin`、`route_tasks_get`、`route_tasks_run`、`route_tasks_travel`、`route_tasks_streak_bonus`、`route_tasks_claim_all`、`route_scheduler_get`、`route_scheduler_trigger`、`route_scheduler_toggle`。
  - 守卫文案逐字：`"账号池不可用"`、`"未找到指定的国内版账号"`、`"未找到已启用的国内版账号"`、`"未找到可用的国内版账号"`、`"未运行"`、`"调度器未初始化"`。
- `crates/api/tests/contract_task_routes.rs`（新增，5 条用例，全走 tempdir 账号池 + `127.0.0.1:1` 不可达上游）：`unauthorized_returns_401`、`invalid_body_returns_400`、`tasks_get_empty_pool_returns_msg`（断言 `msg == "未找到可用的国内版账号"`）、`scheduler_get_returns_enabled_field`、`claim_all_empty_pool_returns_ok`。

**2. 修改文件**
- `crates/api/src/routes.rs`：`AppState` 增加 `task: Arc<TaskRuntime>`；`pub fn router(cfg)` 委托 `app_with(cfg, PanelSessions::new(SESSION_TTL), TaskRuntime::production(&cfg.accounts_dir, Arc::new(ApiPanelSink)))`；新增 `pub fn app_with(cfg, panels, task)`；9 条路由均注册 `.options(preflight)`；9 个 async handler 统一「`unauthorized` → `openai_error(401, "invalid api key - ...", "invalid_request_error")`；`parse_body` 失败 → `openai_error(400, "expected a JSON object", "invalid_request_error")`」；新增 `pub struct ApiPanelSink` 实现 `PanelSink`（**空实现**，见偏离 2）。
- `crates/api/src/lib.rs`：导出 `pub fn app_with(cfg, panels, task) -> axum::Router`。
- `crates/api/Cargo.toml`：`wb-tasks = { path = "../tasks" }`；`axum` 加 `features = ["macros"]`（见偏离 8）。
- `crates/gateway/src/outbound.rs`：`UpstreamClient` 加 `#[derive(Clone)]`。
- `crates/tasks/src/snapshot_cache.rs`：新增 `with_fetch_arc`（接收已装箱的 `SnapshotFetcher`）；`#[allow(clippy::type_complexity)]`。
- `crates/tasks/src/growth_runner.rs`：`RunnerDeps.credits_refresh` 类型加 `+ Send`（否则 handler future 非 `Send`，axum `Handler` 不成立）。
- `crates/tasks/src/lib.rs`：`pub mod api_routes;`。

**3. 编译期卡点与根因（已解决）**
| 卡点 | 根因 | 修正 |
|---|---|---|
| axum `Handler` 不满足（`tasks_run` / `tasks_claim_all`） | `RunnerDeps.credits_refresh: Box<dyn FnOnce() -> Option<Value>>` 缺 `Send`，使 async handler 的 future 非 `Send` | 字段类型加 `+ Send`；调用处 `Arc::new(|| None) as crate::claim_all::CreditRefreshFn` |
| `OnceLock::set(..).unwrap()` 要求 `Debug` | `Arc<dyn CycleExecutor>` / `Arc<Scheduler>` 无 `Debug` | 改为 `let _ = task.exec.set(..)` |
| `SnapshotCache::with_fetch` 要求具体泛型 `F` | 无法直接传 `Arc<dyn Fn>` | 新增 `with_fetch_arc` |
| `Scheduler<'static>` 持有 `&'static TaskRuntime` 字段 | 运行时对象生命周期短于调度器类型参数 | `&dyn Trait` → `&'static dyn Trait` 的 `transmute`，依据：`Arc<TaskRuntime>` 活到进程结束、地址稳定（计划原文允许，无需 `Box::leak`） |
| `router()` 中 `cfg` 先 move 后借用 | 参数求值顺序 | `cfg.clone()` |
| `ClaimAllDeps.client` 非 `Option` | 误按 `Option<UpstreamClient>` 传参 | 去 `Some(...)` |
| `CycleExecutor::accounts` 是同步 `fn` | 误按 async 写 | 改直接返回 |

**4. 实测验证输出（非推断，2026-10-18 本轮，`contract_task_routes.rs` 定稿后重跑）**
- `cargo test --workspace --all-targets` → `TOTAL_PASSED=501`、`FAIL_COUNT=0`，无 `FAILED` / `panicked`。
  各 target 实测条数：`105 / 30 / 25 / 18 / 15 / 13 / 11 / 10 / 10 / 7×3 / 6 / 5×3 / 4 / 3 / 1×3 / 0×2`。
  基线 496 → 501，增量 5 条即本关契约测试。
- `cargo test -p wb-api --test contract_task_routes` → `test result: ok. 5 passed; 0 failed`。
- `cargo fmt --all --check` → `FMT_OK`（此前 `--check` 曾报 1 文件 diff：`contract_task_routes.rs` 的 import 顺序与 `app_with(...)` 换行；`cargo fmt --all` 后复检通过，仅格式）。
- `cargo clippy --workspace --all-targets -- -D warnings` → `Finished 'dev' profile [unoptimized + debuginfo] target(s) in 0.14s`，零告警。

**5. 有意偏离 Python（差异清单，转 4.11 汇总）**
1. **`/tasks/streak-bonus` 为显式占位**：返回 `{"ok": false, "error": "streak_bonus not implemented in rust port yet"}`。`run_streak_bonus`（4 步内部逻辑）属 M4-b。
2. **`ApiPanelSink::add_log_entry` 为空实现**：面板日志存储尚未接线，9 条路由的面板日志副作用当前不产出。推迟到 M5（面板/设置/用量端点）。
3. **`public_view` 25 键 vs Python 44 键**：`public_view` 返回 22 个数据键（uid、nickname、realm、product、enterpriseId、enabled、expiresAt、expiresIn、lastError、lastErrorDetail、inCooldown、cooldownFor、modelCooldowns、softStreak、unscopedStreak、breakerFor、degradeFor、canCheckin、canReportActivity、canDailyChat、machineId、sessionId）加 3 个 `_last_*` 键；Python `Account.public()` 为 44 键。缺的是与本轮路由无关的展示字段；补全属 M5。
4. **`lastError` / `lastErrorDetail` 固定为 `""` / `null`**：账号状态里尚无错误记录字段，未接线。
5. **`/tasks/claim-all` 成功路径未在本关重验**：本关契约测试只覆盖空池（`ok` 字段存在）。claim-all 自身行为已由关卡 4.9 的 30 条测试覆盖，路由层只做参数解析与状态注入。
6. **`Scheduler<'static>` 经 transmute 获得**：见 §3 表格；前提是 `Arc<TaskRuntime>` 地址稳定且进程期存活。
7. **`RunnerDeps.credits_refresh` 加 `+ Send`、新增 `SnapshotCache::with_fetch_arc`**：均为本关装配的连带最小改动，非计划外功能。
8. **`axum` 启用 `macros` feature**：诊断期短暂用于 `#[axum::debug_handler]`，定位完成后已移除属性，feature 保留（不影响运行时行为）。

**6. 未覆盖（M4-b / M5，显式记录）**
- `run_streak_bonus` 内部 4 步、`do_cat_travel`、`run_night_growth`、intl `daily_chat` 网页腿。
- `CreditsRefresher` / `PriceRefresher` / `UpdateChecker`。
- 面板 / 设置 / 用量端点。
- 账号状态落盘（`acc.save()`）由调用方负责，与 4.6/4.7/4.9 口径一致。

### 关卡 4.11：账本 + 全 workspace 验证 — 已完成（2026-10-18）

**1. 本关产出物**
- 本节即产出物：4.10 差异清单已落盘（上文 §5、§6）。
- 三项验证实测输出已写入 §4：`cargo test --workspace --all-targets` = 501 passed / 0 failed；`fmt --check` = `FMT_OK`；`clippy -D warnings` = `Finished`，零告警。

**2. 差异清单汇总（M4-a 全局，跨关卡）**
| # | 差异 | 来源关卡 | 影响 | 处理 |
|---|---|---|---|---|
| D1 | `streak_bonus` 路由占位 | 4.10 | 该路由恒返回 not-implemented | M4-b 补 `run_streak_bonus` |
| D2 | `ApiPanelSink` 空实现 | 4.10 | 路由不写面板日志 | M5 接线 |
| D3 | `public_view` 25 键 vs Python 44 键 | 4.10 | 账号展示字段不全 | M5 补齐 |
| D4 | `lastError` / `lastErrorDetail` 未接线 | 4.10 | 恒 `""` / `null` | M5 或状态模型补字段 |
| D5 | claim-all 成功路径本关未重验 | 4.10 | 覆盖依赖 4.9 的 30 条用例 | 可接受 |
| D6 | `Scheduler<'static>` transmute | 4.10 | 依赖 Arc 地址稳定 | 已论证，无需 leak |
| D7 | claim-all 异常边界不用 `catch_unwind` | 4.9 | panic 不被吞 | 已记录 |
| D8 | steps 行文本为 JSON 数组而非 Python repr | 4.9 | 形状一致、非逐字 | 已记录 |
| D9 | streak / cat step detail 用 `msg or error` | 4.9 | 比 Python 多一路回退 | 已记录 |
| D10 | 不移植 `acc.save()` | 4.6/4.7/4.9/4.10 | 持久化由调用方负责 | 已记录 |
| D11 | 展示名改查 `ClaimAllDeps.nicknames` | 4.9 | 数据来源不同 | 已记录 |
| D12 | `parse_ttl` 非法值回落缺省 | 4.7 | Python 会抛异常 | 已记录 |
| D13 | `RunnerDeps.credits_refresh` 加 `+ Send` | 4.10 | 仅类型放宽 | 无行为变化 |
| D14 | `SnapshotCache::with_fetch_arc` | 4.10 | 新增装配入口 | 无行为变化 |
| D15 | `axum` 启用 `macros` feature | 4.10 | 编译特性 | 保留 |

**3. 结论**
- M4-a 范围内的关卡 4.1–4.11 全部完成，三项验证全绿。
- 未提交；按计划约束「commit 与推送逐次确认」，由用户逐次批准后再提交。

_下一关：M4-b（`run_streak_bonus` 内部 4 步、`do_cat_travel`、`run_night_growth`、intl `daily_chat` 网页腿；`CreditsRefresher` / `PriceRefresher` / `UpdateChecker`、面板/设置/用量端点归 M5 或后续里程碑）。_

### 关卡 4.12 / 4.13 / 4.14 / 4.15：M4-b 国际版每日活跃（daily_chat）— 已完成（2026-10-20）

**授权**：用户 2026-10-20 选定「收口 M4-b 语义缺口（推荐）」+「移植网页腿 ACP 会话（推荐）」。范围三条：①端点按 realm/product 选（不再用 CN 常量打国际版）②移植 Python 网页腿 ACP 会话（`wb_webagent.py`）③更新 M4 账本。

**1. 本关产出物**

| 产出物 | 位置 | 复核方式（实测） |
|---|---|---|
| ACP 会话客户端 | `crates/gateway/src/acp.rs`（新增） | `cargo test -p wb-gateway acp` = 10 passed / 0 failed |
| 每日活跃两腿编排 | `crates/tasks/src/daily_chat.rs`（新增） | `cargo test -p wb-tasks --lib daily_chat` = 7 passed / 0 failed |
| 端点按身份解析 | `daily_chat.rs` → `DailyChatConfig::resolved_chat_base` | 单元测试：国际版 → `www.workbuddy.ai`；国内版 → `copilot.tencent.com` |
| 执行器接线（替换臆造端点） | `crates/tasks/src/api_routes.rs` → `ProductionExecutor::daily_chat` | `cargo test -p wb-tasks --test contract_daily_chat` = 10 passed / 0 failed |
| 契约测试（重写） | `crates/tasks/tests/contract_daily_chat.rs` | 10 条用例全绿 |

**2. 三项验证实测输出（原文，非推断）**

```
$ cargo fmt --all -- --check
FMT_OK

$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.08s

$ cargo test --workspace
passed=544 failed=0
```

分项：`cargo test -p wb-gateway acp` = 10 passed；`cargo test -p wb-tasks --lib daily_chat` = 7 passed；`cargo test -p wb-tasks --test contract_daily_chat` = 10 passed。

**3. 修复前的三处缺陷（逐条对照 Python）**

| # | 缺陷 | 证据 | 现状 |
|---|---|---|---|
| B1 | 臆造端点 `POST {chat_base}/v2/activity/daily-chat` | Python 全仓 0 命中 | 已换成 `POST {chat_base}/v2/chat/completions` |
| B2 | 端点用国内版常量 | `crates/tasks/src/events.rs:25 CHAT_BASE` | 已改成 `wb_gateway::endpoints::chat_base_url(realm, product)` 按账号身份解析 |
| B3 | 没有网页腿（ACP 会话） | Python `wb_webagent.py` 258 行 | 已移植到 `wb_gateway::acp` 并接入 `daily_chat` 两腿编排 |

**4. 差异清单（本关新增）**

| # | 差异 | 来源 | 影响 | 处理 |
|---|---|---|---|---|
| D16 | 桌面腿成功后不刷新积分快照 | Python `_daily_chat_upstream` 成功后调 `fetch_credits()` | 面板积分仍是旧快照 | **M5**（Rust 侧无 credits 抓取） |
| D17 | 不写活动历史 | Python `wb_activity.record_attempt` | 面板缺「每日活跃」流水行 | **M5**（Rust 侧无 activity store） |
| D18 | 429/6004 不记模型 cap | Python `_note_daily_chat_cap` | 同模型窗口满时仍反复重试 | **M5**（`notes::note_model_cap` 已有，接线未做） |
| D19 | 每账号 HTTP 代理未支持 | Python `Account.proxy` 逐请求走代理 | Rust 全部直连 | 后续里程碑（Rust 侧尚无代理配置项） |
| D20 | `accounts()` 每轮重读幂等字段 | 本关新加 | 修正「写盘后内存视图仍是旧值 → 同一账号当天反复跑每日活跃」 | 有意修复，已加注释 |
| D21 | `acp::run_turn` 加 `#[allow(clippy::too_many_arguments)]` | 本关新加 | 10 个入参照抄 Python 签名 | 与 `crates/gateway/src/notes.rs:105` 一致 |
| D22 | `is_channel_blocked` 未锚定 | 与 Python 正则一致（`111280` 也会命中） | 极少数误判成渠道拦截 | 与 Python 一致，**故意不**「修正」 |
| D23 | 失败返回体多一个 `persist_error` 键 | 与 `checkin::CheckinOutcome` 一致 | 落盘失败可见 | 有意差异 |

**5. 与 Python 语义对齐的关键点（易踩坑，务必保留）**

- **`lastDailyChat` 只在网页腿成功时写**：桌面腿成功 + 网页腿失败 → **不写**（Python :1650-1656 明确注释：否则当天 30/50 积分静默丢掉且不再重试，面板还显示成功）。契约用例 `desktop_success_web_failure_does_not_write_last_daily_chat` 钉住这一点。
- **`code: 11128` 是降级不是失败**（Python :1620-1656）：命中就把桌面腿记成「这一腿没走成」，网页腿单独跑。
- **`daily_chat_web` 默认开**（`wb_settings.py:1200`：`value is None → True`；非 JSON `true` 的值一律算关）。
- **奖励包跨日发放（D+1）**：判「今天已完成」只依据 `lastDailyChat`，**不**依据积分快照里有没有今天的包（那是**昨天**的结算，拿它拦截会让账号隔天才拿一包）。Rust 侧 `daily_chat_granted_today` 恒 `false`，正好符合 D+1 口径。
- **网页腿必须接上沙箱**：只建会话不算数（issue #90：会话会永远停在 CREATING）。Rust 侧对应「建会话 → 取 `link`/`token` → ACP 三步 → 轮询到 `completed`」四步，缺一不可。

**6. 结论**

- M4-b 的 `daily_chat` 部分（桌面腿 + 网页腿 ACP + `lastDailyChat` 落盘 + 契约测试 + 账本）已完成，三项验证全绿。
- M4-b 剩余项**未开始**：`run_streak_bonus` 内部 4 步、`do_cat_travel`、`run_night_growth`。
- 未提交；按计划约束「commit 与推送逐次确认」，由用户逐次批准后再提交。

_下一关：M4-b 剩余项（`run_streak_bonus` 内部 4 步 / `do_cat_travel` / `run_night_growth`），或按用户指定切到 M5（面板 / 设置 / 用量端点 / `CreditsRefresher` / `PriceRefresher` / `UpdateChecker`）。M3-b（`/v1/responses` 翻译层 + web-tools 回放）、M6 影子回放、M7 切换均未开始。_

### 关卡 4.16：M4-b 代码评审修复（出站超时 + S1/S2/S3/S5）— 已完成（2026-10-20）

**背景**：关卡 4.12–4.15 完成后跑了一轮代码评审，结论是「必须修 1 项 + 5 条建议 + 6 处逐字核对通过」。本关把必须修项与 4 条建议就地修掉（改动仍在 M4-b 范围内：daily_chat + ACP 这条链路的代码就是本里程碑引入的）。

**1. 本关产出物**

| 产出物 | 位置 | 复核方式（实测） |
|---|---|---|
| 带时限的出站变体 | `crates/gateway/src/outbound.rs` → `post_with_timeout` / `get_with_timeout` / `describe_error` | `cargo test -p wb-gateway outbound` = 8 passed / 0 failed（含「慢上游必须超时」「不带时限的 `post` 仍不超时」） |
| ACP 建链严格 200 | `crates/gateway/src/acp.rs` → `AcpChannel::open` | `cargo test -p wb-gateway acp` = 13 passed / 0 failed（含「201 也必须拒」） |
| ACP 收尾补查状态 | `crates/gateway/src/acp.rs` → `run_turn` | 同上（含「JSON-RPC 失败后仍要补查一次状态」） |
| daily_chat 全链路上时限 | `crates/tasks/src/daily_chat.rs` → `DESKTOP_TIMEOUT`(20s) / `WEB_TIMEOUT`(30s) | `cargo test -p wb-tasks --lib daily_chat` = 7 passed / 0 failed |
| 数字型会话 id | `crates/tasks/src/daily_chat.rs` → `py_truthy` + `conversation_raw` | `cargo test -p wb-tasks --test contract_daily_chat` = 12 passed / 0 failed |

**2. 三项验证实测输出（原文，非推断）**

```
$ cargo fmt --all -- --check
FMT_OK

$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.74s

$ cargo test --workspace
passed=552 failed=0
```

（上一关为 `passed=544`；本关新增 8 条用例：outbound 3 条、acp 3 条、contract 2 条。）

**3. 修复前的缺陷（逐条对照 Python）**

| # | 缺陷 | 证据 | 现状 |
|---|---|---|---|
| M1 | 出站 HTTP 完全无限时：`UpstreamClient::post/get` 用 `reqwest::Client::builder().build()`，既无 `timeout` 也无 `connect_timeout` | Python 侧每次 `urlopen` 都带 `timeout=20/30`；`wb_webagent.py:102,109` 用 `http.client(..., timeout=self.timeout)` | daily_chat + ACP 链路的缓冲型调用全部带上时限；**M3 流式路径刻意保持无限时** |
| S1 | 会话 id 只认字符串 | Python `wb_accounts.py:1503` `(payload.get("data") or {}).get("id")` 不限定类型，URL 里 `quote(str(conversation))` | 接受数字/布尔等任意标量，URL 用 `str()` 形态，结果里保留上游原值 |
| S2 | ACP 轮询循环退出后不再查状态 | Python `wb_webagent.py:249-253` 有补查 | 补查（只更新 `status`，不改 `finished`——与 Python 一致） |
| S3 | ACP 建链接受任意 2xx | Python `wb_webagent.py:123` 是 `if resp.status != 200` | 改成「恰好 200」 |
| S5 | 缺数字 id / 损坏 `settings.json` 的用例 | — | 补 2 条契约用例 |

**4. 差异清单（本关新增）**

| # | 差异 | 来源 | 影响 | 处理 |
|---|---|---|---|---|
| D24 | **其余任务调用的出站同样无限时**：`crates/tasks/src/growth_api.rs:82/193/290/315`、`checkin.rs:280/341`、`events.rs:297/316/342`、`summary.rs:131`、`growth_runner.rs:432/472`，以及 M3 流式路径 `crates/gateway/src/retry.rs:364` | 本关只修了 daily_chat + ACP 这条链路的调用点 | 同样存在「上游挂起 ⇒ 巡检停摆」的风险 | **范围外，未改**：按「只动被要求动的地方」不扩散。**M7 切流前必须处理**（流式路径只能用「只加连接/读超时、不加总时限」的办法） |
| D25 | 超时语义：Python 是**逐次读**的 socket 超时，Rust 用的是 reqwest 的**总时限**（从开始连接到响应体读完） | `urlopen(timeout=N)` vs `RequestBuilder::timeout` | 对「几 KB 的 JSON 答复」没有实质差别；总时限更严格（绝不会无限挂住） | 有意为之，代码注释已写明 |
| D26 | ACP 的 SSE **只给「等响应头」这段加时限**，正文流不设总时限 | 长连接 SSE 要活过 120s 的等待窗口 | 上游长时间静默时 Rust 会继续读（Python 的 socket 超时会让读线程死掉） | 有意为之：判定「跑完」靠状态轮询，SSE 只统计事件数 |
| D27 | `web.conversation` 现在保留上游**原值**（上游给数字就是数字） | Python `result["conversation"] = conversation` 塞的是原值 | 字符串场景逐字不变 | 与 Python 对齐 |
| D28 | 出站错误文案统一过 `describe_error` | — | 超时/连接失败不再显示「error sending request for url」 | 有意改进 |

**5. 留给 M5 的待办（评审 S4）**

- Python `wb_proxy.py:17549` `_route_accounts_daily_chat`（手动跑每日活跃的端点）——Rust 侧**无**对应入口。
- Python `wb_proxy.py:17568` `_route_accounts_daily_chat_web`（**只**跑网页腿、且**刻意不写** `lastDailyChat` 的端点）——Rust 侧**无**独立 `daily_chat_web` 入口。

**6. 结论**

- M1 + S1/S2/S3/S5 已修并有实测用例钉住；`passed=552 failed=0`，fmt 与 clippy 全绿。
- **S4 与 D24 明确留到 M5 / M7 前处理**，不在本关扩散改动。
- 未提交；按计划约束「commit 与推送逐次确认」，由用户逐次批准后再提交。

### 关卡 4.17：M4-b 剩余 + M5（并行四条实现线）— 已完成（2026-10-20，集成收口见本节末）

**授权**：用户 2026-10-20 指示「并行做」——在推送 M4-b 收口提交（`a05e57bc`，已推 `origin/feat/rust-m2`）的同时，四条实现线并行推进。

| 线 | 范围 | 独占文件 |
|---|---|---|
| A | M4-b 剩余①：`run_streak_bonus` 内部 4 步 | 新建 `tasks/src/streak_bonus.rs`、重写 `tests/contract_streak_bonus.rs` |
| B | M4-b 剩余②：`do_cat_travel` + `run_night_growth` | 新建 `tasks/src/night_growth.rs`、改 `tasks/src/growth_runner.rs` |
| C | M5 数据侧：积分(D16) / 活动历史(D17) / 模型 cap(D18) / 价格 / 更新检查 | 新建 `tasks/src/{credits,activity,cap,price,update}.rs` |
| D | M5 端点侧：S4 两个手动 daily_chat 端点 + 面板/设置/用量路由 | 改 `tasks/src/api_routes.rs`、`api/src/routes.rs` |

防冲突设计：`lib.rs` 的模块声明各用**不同锚点行**；`api_routes.rs` 只有 D 可写，A/B/C 只读；C 的新模块 D **不调用**（避免编译期依赖）；各线禁止 `cargo fmt --all`（会格式化他人半成品），只许 `rustfmt` 自己的文件。

#### 线 A：连登管家四步移植 — 已完成（待集成接线）

**1. 产出物**

| 产出物 | 位置 | 复核方式 |
|---|---|---|
| 连登管家四步实现 | `crates/tasks/src/streak_bonus.rs`（新建，约 640 行含单测） | 8 passed / 0 failed（**隔离 crate** 内，见下） |
| 契约测试 | `crates/tasks/tests/contract_streak_bonus.rs`（整份重写，13 条） | 13 passed / 0 failed（隔离 crate） |
| 模块声明 | `crates/tasks/src/lib.rs`：`pub mod summary;` 之后插入 `pub mod streak_bonus;` | 锚点唯一，未碰他人锚点 |

⚠️ **验证方式的例外（必须如实记）**：A 线收工时仓库内 `cargo test -p wb-tasks` 被并行代理 C 的半成品编译错误挡住（`tasks/src/{activity,credits,price}.rs`），A 线按分工不代劳修别人的文件，改在 `~/scratch/streak-bonus-verify/` 建隔离 crate（`#[path]` 直接引用仓库里的 `streak_bonus.rs` 原文）验证：`FMT_OK`、8 passed、13 passed、clippy 零警告。**仓库内复跑是集成关卡 4.17 的必过项**（隔离 crate 的数字不能替代仓库内实测）。

**2. 重要发现：旧实现的端点上游根本不存在**

旧 Rust 实现在 `api_routes::streak_bonus` 里打 `POST {chat_base}/activity/growth/streak/claim`；`grep -rn "streak/claim" --include=*.py .` **0 命中** —— Python 的连登管家是四步，从不调该端点。**⇒ Rust 侧连登管家从来没真正工作过**（不是语义差距，是功能从未生效）。

**3. 四步与 Python 的逐条对应**（Python `wb_tasks.py:771-879`）

| 步 | 端点 | 关键语义 |
|---|---|---|
| 0 补签 | `GET /activity/growth/heatmap` → `GET /activity/growth/streak` → `POST /activity/growth/makeup-cards/use` | 昨天格子 `score==0` 才查卡；`balance>0` 才用卡；body `{"target_date":"YYYY-MM-DD"}`；**2xx 即记日志，不看 `code`** |
| 1 礼包/补偿 | `POST {bill base}/billing/meter/claim-gift` / `claim-compensation` | body 字面量 `{}`；`code==0` 才记 `领取{name} +{cr}积分` |
| 2 档位兑换 | `GET /activity/growth/streak` → `POST /activity/growth/redeem` | body `{"tier":…,"client_token":…}`；只跳过 `locked`/`claimed`；单档失败**不**中断整步 |
| 3 抽奖 | `GET /activity/growth/lottery/summary` → `POST /activity/growth/lottery/draw` | `chances` 次全抽；**每次迭代末尾都 sleep 1.0（含最后一次）**；抽失败 `break` 且不再 sleep |

门禁逐字一致：`realm != "cn"` → `{"ok":false,"msg":"连登管家仅限国内版账号"}`；`enterprise_id` 非空 → `{"ok":true,"msg":"企业版账号无成长体系，跳过"}`——**这两个返回体没有 `logs` 键**（故 `logs: Option<Vec<String>>`）。

**4. 差异清单（线 A 新增）**

| # | 差异 | 影响 | 处理 |
|---|---|---|---|
| D29 | 异常可见性：Python 各步异常写 `_log`（应用日志），返回体看不到；Rust 收进 `StreakBonusOutcome.warnings`，**不进 `to_json()`** | 返回体形状与 Python 逐字相同 | 有意差异 |
| D30 | 补签的「昨天」：Python 是 `localtime(time.time()-86400)`（夏令时切换日可能差一天）；Rust 是「今天本地日期减一天」 | 极端日期边界 | 有意：Rust 更稳 |
| D31 | 类型宽容：上游把 `score`/`balance`/`chances` 给成非预期类型时，Python 抛异常中止该步，Rust 降级为「跳过 + warning」 | 不崩、不误判 | 有意 |
| D32 | `client_token`：Python 用 `uuid.uuid4()`；Rust 未新增生产依赖，用「时钟+计数器+哈希播种」拼 uuid4 形状并做成注入点 | 令牌形态 | 有意（测试可确定性） |

**5. 线 A 留给集成的待办（主会话负责）**

1. `api_routes::streak_bonus` 仍是打空端点的旧实现 → 改接 `tasks::streak_bonus`（A 线重写测试时移除了旧端点的 5 条 api_routes 层用例，接线后要补回）。
2. **集成缝隙**：`PooledTaskAccount` 只有 `chat_headers`、**没有 billing 头**；第 1 步需要 `billing_request_headers(&BillingHeadersParams{uid,token,realm,domain,enterprise_id,device_token})` 现算（`ReadyAccount` 里有 `device_token`）。
3. 跨代理 API 名漂移风险：B 线一度 import 不存在的 `post_with_timeout_call`/`get_with_timeout_call`（真实名是 `post_with_timeout`/`get_with_timeout`），B 已自行修掉；集成时**全仓再核一遍**这类名字漂移。

#### 线 B：猫猫旅行 + 夜猫子移植 — 已完成（待集成接线）

**1. 产出物**

| 产出物 | 位置 | 复核方式 |
|---|---|---|
| 夜猫子实现 | `crates/tasks/src/night_growth.rs`（新建） | `contract_night_growth_task.rs` 14 passed / 0 failed |
| 旅行路径修正 + 超时 | `crates/tasks/src/growth_runner.rs`（改） | `cargo test -p wb-tasks --lib growth_runner` 40 passed |
| 契约测试 | `crates/tasks/tests/contract_night_growth_task.rs`（新建，14 条） | 14 passed / 0 failed |

⚠️ 文件名偏离原定：原定的 `contract_night_growth.rs` **已被关卡 4.10 占用**（那是 `api_routes::ProductionExecutor::night_growth` 路由层的契约测试），故新建 `contract_night_growth_task.rs`，未覆盖既有文件。

**2. 关键发现：R1 路径 bug（真实功能失效）**

Rust 原实现打 `GET /activity/growth/travel/status`，Python 权威是 `GET /activity/growth/buddy/travel/status`（`wb_tasks.py:412`，**无** `/v2`）——**漏了 `/buddy/`**。上游不存在该路径 ⇒ 猫猫旅行整个功能在 Rust 侧从未生效。更严重的是：旧单测把错误路径当期望值（9 处断言），**测试跟着实现一起错，等于没测**。B 线已改正断言并新增一条「断言 wiremock 实际收到的路径」的测试。

**3. 出站超时**：`growth_runner.rs` 新增 `TRAVEL_TIMEOUT = 10s`（Python 四处 `timeout=10`），状态 GET / config GET / depart POST / claim POST 全部改为 `get_with_timeout` / `post_with_timeout`。

**4. 差异清单（线 B 新增）**

| # | 差异 | 影响 | 处理 |
|---|---|---|---|
| D33 | 积分刷新：Python 领奖成功后调 `account.fetch_credits()`（`wb_tasks.py:478`）；Rust 原实现完全不刷 | 面板积分余额不更新 | B 线加了 `build_cat_travel_refreshing`（闭包注入）；**路由层尚未改接**（归主会话） |
| D34 | `msg` 键只出现在「国际版 / 企业版」两档（Python 字面量如此），其余档没有 | 返回体键序 | Rust `to_json` 做**两套键序**，与 Python 逐字一致 |
| D35 | `earned_credit` 反推：Python 逐行 `re.search(r"\+(\d+) 积分", line)`，**后一行覆盖前一行** | 取最终累计值 | Rust 手写同语义解析器（取最后一条），未新增 `regex` 依赖 |
| D36 | `credits_refresh` 是**同步**闭包（`FnMut() -> Option<Value>`），而 Rust 侧无持久化积分快照存储点（M5 的 `CreditsRefresher` 未落地） | 接线时只能回填 `None` | **已知缺口**，接线时显式记入本账本，不假装等价 |

**5. 线 B 留给集成的待办（主会话负责）**

1. `api_routes::night_growth` 仍是占位实现 → 改接 `night_growth::run_night_growth`；**返回键要从 `credit` 改成 Python 的 `earned_credit`**。改完后关卡 4.10 的 `contract_night_growth.rs`（4 条，断言旧占位形状）需同步更新。
2. `api_routes::cat_travel` 仍调不刷积分的 5 参版 → 改调 `build_cat_travel_refreshing`。

#### 线 C：M5 数据侧（积分/活动历史/cap/价格/更新检查）— 已完成

**1. 产出物**

| 产出物 | 位置 | 行数 | 复核方式 |
|---|---|---|---|
| 积分包解析 + 12 键快照聚合 | `crates/tasks/src/credits.rs`（新建） | 838 | `contract_credits.rs` 9 passed |
| 活动历史 JSONL 投影/筛选/脱敏/压实 | `crates/tasks/src/activity.rs`（新建） | 694 | 单测（lib 内） |
| 模型 cap 判定层 | `crates/tasks/src/cap.rs`（新建） | 198 | 单测 |
| 定价快照 + 等价成本估算 | `crates/tasks/src/price.rs`（新建） | 1039 | 单测 |
| 更新发现（只发现不安装） | `crates/tasks/src/update.rs`（新建） | 423 | 单测 |
| 积分抓取的 **HTTP 那一半** | `crates/gateway/src/credits.rs`（新建） | 459 | 单测 |
| 契约测试 | `crates/tasks/tests/contract_credits.rs`（新建，9 条） | 389 | 9 passed / 0 failed |

**2. 架构决策：为什么必须新增 wb-gateway 模块**

`crates/tasks/Cargo.toml` 明文规定「契约类型全为 `serde_json::Value` / 纯数据；无任何 HTTP 依赖（HTTP 由 wb-gateway 提供）」，而积分抓取必须发 HTTP。故拆成两半：`wb_gateway::credits` 只取回上游原文（不塑形），`wb_tasks::credits` 负责解析与聚合（21 键 Package / 12 键 Credits / 排序 / 临期 / 百分比），由 `packages_from_accounts` + `aggregate_detailed` / `aggregate_fallback` 衔接。

**3. 差异清单（线 C 新增）**

| # | 差异 | 影响 | 处理 |
|---|---|---|---|
| D37 | Python `round(x,2)` 对精确二进制值做 round-half-even；Rust `f64::round` 是五入 | 百分比尾数 | 改为 `format!("{x:.2}").parse()`，用 7 个探针值对齐 CPython |
| D38 | Python `http_json` 重试 3 次，触发条件是 SSL 错 / URLError / HTTP ≥500 | 退避行为 | Rust 对 timeout/connect/5xx 重试，退避同为 `1.0 × attempt` 秒；4xx 两边都立即失败 |
| D39 | Python `range_cutoff` 内部自己读 `time.time()` | 时点 | Rust 把 `now` 作为入参（仅影响可测性，生产等价） |
| D40 | Python `local_midnight` 用 `time.mktime(..., tm_isdst=-1)` | 夏令时 | Rust 用 `and_local_timezone`，`Ambiguous` 取第一个、`None` 回落当前偏移 |
| D41 | `activity::value_text` 复刻 Python `str(x or "")`（0 / False / 空容器 ⇒ 空串） | 文本投影 | 逐条对齐 |
| D42 | `price::compute_row` 的 `cached = min(cached_tokens, prompt)`、输出费率只作用于 completion_tokens | 成本估算 | reasoning token 不重复计费，与 Python 一致 |
| D43 | Python 的 `wb_catalog`（`hub_model_ids()`）与运行时覆盖表（文件支撑）在 Rust 侧不存在 | 价格快照的输入 | `build_snapshot` 改由调用方传 `hub_ids` / `runtime_overrides` |
| D44 | Python band 的 `days` 未设时是 `null` | 形状 | Rust `Option<Vec<String>>` 区分 `None`（未设）与 `Some(vec![])` |
| D45 | Python `http_json` 遇非 JSON body 抛异常（由调用方 catch） | 错误路径 | Rust 返回 `Err("响应不是 JSON")` |

**4. D36 裁决（cap 门控口径）— 主会话裁定**

C 线提出「静态表 vs 运行时目录哪个权威」。**核实 Python 后澄清：这是两套并存机制，不是二选一。**

| | Python | 作用 | Rust 落点（裁决后） |
|---|---|---|---|
| 静态表 | `is_free_cap_model(realm, model)`（`wb_accounts.py:281-287`） | 决定 6004 **要不要写进 `model_caps`**（跨重启持久禁用） | **新建 `crates/gateway/src/cap.rs`**（权威定义）；`wb_tasks::cap` 改为 `pub use` re-export，消除双份定义 |
| 运行时目录 | `account.free_models`（由 `apply_daily_credit_limit()` 从模型目录按 realm 填充） | 决定「这个模型算不算免费」（每日额度守卫 `model_is_free`） | `AccountState::free_models` 字段保留，注释已更正；M5 每日额度守卫未落地 ⇒ 目前**无消费者**、恒空 |

裁决的必要性：Rust 侧 `note_model_cap` 原用 `account.free_models.contains(model)` 当门控，而该字段恒空 ⇒ **6004 永远不落 `model_caps`**（面板显示失败、模型却不被跨重启禁用）。Python 权威是静态表按 `realm` 判定（`wb_accounts.py:953`）。

**产出物**：
- `crates/gateway/src/cap.rs`（新建）：`FREE_CAP_MODELS` + `is_free_cap_model` + 1 条单测。
- `crates/gateway/src/notes.rs`：`note_model_cap` 门控改为 `crate::cap::is_free_cap_model(&account.realm, model)`；原单测的 `free_models.insert` 已失效故删除；新增 1 条「同一次 6004，intl 记 cap、cn 不记」的跨 realm 区分用例。
- `crates/gateway/src/pool.rs`：`free_models` 字段注释更正（不再声称是 `note_model_cap` 门控）。
- `crates/tasks/src/cap.rs`：静态表改为 `pub use wb_gateway::cap::…`（**签名与路径不变**，不构成第二份定义）。
- 验证：`cargo test -p wb-gateway` = **235 passed / 0 failed**（原 233，新增 2）。

**5. 线 C 留给集成的待办（主会话负责）**

1. `CreditsRefresher` / `PriceRefresher` / `UpdateChecker` 的**后台线程与调度**未实现 —— 只有纯判定层；线程模型属主会话/D 线。
2. `price` 无策略库（Python 的 policies / timeline 文件）、`update` 无 GitHub HTTP 抓取 —— 留给主会话。
3. `activity` 的「压实触发」（2 MB 体积阈值 / 6 小时间隔）只暴露为 `compact_rows`，**未接调度**。
4. 企业版积分路径（`_fetch_credits_enterprise`）未移植（C 线任务书只覆盖国内版详细口径 + 兜底口径）。

#### D24：出站 HTTP 超时缺口（主会话，非代理范围）— 已完成

账本早前记的「M7 切流前必须处理」的出站超时缺口，涉及 4 个**无代理拥有**的文件，主会话统一处理完毕。

| 文件 | 常量 | Python 权威 | 转换的调用点 |
|---|---|---|---|
| `crates/tasks/src/growth_api.rs` | `GROWTH_API_TIMEOUT = 15s` | `wb_tasks.py:114`（任务清单 GET）、`:224`（接取）、`:274`/`:305`（领奖） | 4 处 |
| `crates/tasks/src/checkin.rs` | `CHECKIN_TIMEOUT = 15s` / `STREAK_DAYS_TIMEOUT = 10s` | `wb_accounts.py:1683` / `wb_tasks.py:156` | 2 处 |
| `crates/tasks/src/events.rs` | `REPORT_TIMEOUT = 15s` / `TRAVEL_STATUS_TIMEOUT = 10s` | `wb_tasks.py:404` / `:414`、`:181` | 3 处 |
| `crates/tasks/src/summary.rs` | `SUMMARY_TIMEOUT = 10s` | `wb_tasks.py:148`（能量）、`:156`（连登）、`:173`（抽奖）、`:181`（旅行） | `get_json` 辅助函数（4 段共用） |

**超时语义差异（有意）**：Python `urlopen(timeout=N)` 是**单次 socket 读超时**；reqwest 的 `RequestBuilder::timeout(N)` 是**总截止时间**（连接 → 响应体读完），方向更严，对几 KB JSON 无实质差别。

**保留无超时的调用点（有意，共 3 处）**：`outbound.rs:39/56`（plain `post`/`get` 的定义本身）、`acp.rs:139`（SSE 首包等待，已被 `tokio::time::timeout` 包裹；响应体长连不设限）、`retry.rs:364`（**M3 流式路径**——响应体要流给用户，设总时限会截断流）。

**全仓核验**：`grep -rnE "client\s*\.\s*(get|post)\s*\("` 结果与上述 3 处完全一致；API 名漂移扫描（`post_with_timeout_call` / `get_with_timeout_call` 等错误名）**0 命中**。

#### 集成收口（主会话）— 已完成（2026-10-20）

四线合并后的**主会话改动**（A/B/C/D 各自只交付模块与测试，接线归主会话）：

| # | 接线点 | 改了什么 |
|---|---|---|
| 1 | `api_routes::streak_bonus` | 旧实现打的是上游**不存在**的 `POST /activity/growth/streak/claim` ⇒ 改接 `tasks::streak_bonus`（四步移植） |
| 2 | `PooledTaskAccount::billing_headers()` | **集成缝隙**：原结构只有 `chat_headers`，第 1 步（礼包/补偿）需要 billing 域的出站头 ⇒ 新增现算（`billing_request_headers`，带 `device_token`） |
| 3 | `api_routes::night_growth` | 占位实现 ⇒ 改接 `night_growth::run_night_growth`；返回键从 `credit` 改为 Python 的 **`earned_credit`**，`msg` 只在「国际版 / 企业版」两档出现 |
| 4 | `api_routes::cat_travel` | 改调 `build_cat_travel_refreshing`（领奖后刷积分，D33） |
| 5 | D 线端点 | 两个手动 daily_chat 端点 + 面板/设置/用量路由（`api/src/routes.rs`） |

**本关修复的三处测试缺陷**（前两处是旧用例断言了**上游不存在的端点**与**旧占位形状**）：

1. `crates/api/tests/contract_task_routes.rs` 的 3 条 streak_bonus 用例：旧断言打的是 `POST /activity/growth/streak/claim`（Python 全仓 0 命中）⇒ 重写为 Python 四步口径；新增 `mount_streak_bonus_steps` 助手（heatmap / streak / claim-gift / claim-compensation / lottery-summary 全集）；**并修掉一个会打真实上游的隐患**：原 `streak_runtime` 把 `billing_base` 指向 `http://127.0.0.1`（非 mock），礼包/补偿两步会绕过 mock ⇒ 改指 mock base。
2. 「部分失败」用例在 Python 口径下**不可达**：各步失败只跳过该步、账号级仍 `ok:true`；唯一能产出 `ok:false` 的国际版门禁又被路由的 `realm=="cn"` 筛选挡掉 ⇒ 拆成两条真实用例：① 上游整片 500 仍 `ok:true`（Python 语义，且 `warnings` 不泄漏进返回体）；② 聚合分支用 `state.realm` / `creds.realm` 不一致的构造锁定（**生产不可达**，仅锁 `ok:false` + "partially failed" 这条分支）。
3. `crates/tasks/tests/contract_night_growth.rs` 的 4 条用例：断言旧占位形状（`credit` 键）⇒ 重写为 Python 的 `ok` / `earned_credit` / `logs`；且旧用例**依赖真实时间**（夜猫子窗口 23:00-08:00，判定读 `TaskRuntime` 的时钟）⇒ 给 `TaskRuntime` 注入可替换时钟（见 D46），6 条用例与真实时间无关。

**本关验证数字（仓库内实测，非隔离 crate）**

| 检查 | 结果 |
|---|---|
| `cargo test --workspace` | **673 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 零警告 |
| `cargo fmt --all -- --check` | FMT_OK |

**新增差异条目**

| # | 差异 | 影响 | 处理 |
|---|---|---|---|
| D46 | `TaskRuntime.clock` 的类型由 `LocalClock` 改为 `Arc<dyn Clock>`（并新增 `test_with_ready_and_clock` 构造） | `wb-tasks` 的 **pub 字段类型**变了（crate 内部 API，非对外协议） | 有意：夜猫子的窗口判定若不可注入时钟，执行器层契约测试就只在真实夜间成立（白天必挂）。`schedule_clock`（调度器用）未动 |
| D47 | `/tasks/streak-bonus` 返回体形状与 Python 不同：Rust 是 `{"ok": all_ok, "msg": "streak bonus completed"\|"partially failed", "results": [对象], "accounts_count": N}`；Python 是 `{"ok": true, "msg": "<换行拼接的 nick: msg>"}` | 面板上看到的文案不同 | **既有差异**（`git show HEAD:crates/tasks/src/api_routes.rs` 与本关改动后逐字相同 ⇒ 不是本关引入）。`dashboard.html:4863` 的 `triggerStreakBonus` 只消费 `r.msg` / `r.ok` ⇒ 不破坏。**按「只动被要求动的地方」报告而不代劳**；是否对齐 Python 由用户定 |

**本关遗留（未做，需用户决定）**

- M5 的后台刷新线程（`CreditsRefresher` / `PriceRefresher` / `UpdateChecker`）未做——只有纯判定层。
- D36 `credits_refresh` 仍回填 `None`（Rust 侧还没有持久化积分快照的存储点）。
- 线 C 的第 2/3/4 项遗留：`price` 无策略库、`update` 无 GitHub HTTP 抓取、`activity` 的压实未接调度、企业版积分路径未移植。
- M3-b（`/v1/responses` 翻译 + web-tools replay）、M6（影子回放）、M7（切流）未启动。



