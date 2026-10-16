# M2 Rust 里程碑实施计划（identity + storage）

日期：2026-10-16
基分支：feat/rust-m2（从 feat/rust-m1 切出）
前置：M1（feat/rust-m1，已推送）——workspace 骨架、storage 只读快照、api /health 基础字段 + GET / 面板 + 契约测试框架 + CI
spec：docs/superpowers/specs/2026-10-15-rust-refactor-design.md（:75「M2 identity+storage」，:39「identity/ # 账号、realm、JWT、导入导出、OAuth」）

## 范围

### M2-a（本次实施）——identity 读路径 + storage 扩展

只做「读路径 / 纯函数」，**不含任何凭据落盘写路径**（符合 spec :55「影子默认只读或 dry-run」）：

| 模块 | 内容 | 权威参照 |
|---|---|---|
| identity: jwt | `jwt_claims`/`jwt_exp`/`jwt_uid`/`jwt_issuer`，base64 url-safe 解码、padding 补齐 | wb_accounts.py:220-247 |
| identity: realm | `realm_evidence`/`detect_realm_from_token`/`domain_for_realm`/`desktop_effective_realm`，CN/INTL markers | wb_accounts.py:249-254, 305-349 |
| identity: import | `normalise_import_row`：三形状（扁平/嵌套/cockpit snake_case）、uid 净化、realm 优先级、volatile 重置 | wb_accounts.py:3575-3650 |
| identity: export | `account_to_export`/`build_export_document`：format/version/exportedAt/count/accounts、include_secrets 剥离 | wb_accounts.py:3466-3534 |
| storage: Account | 加载**全字段**（M1 只取 uid/realm/enabled/accessToken/cooldownUntil/lastError） | wb_accounts.py Account.to_dict |
| storage: ready | `count_ready()` 对齐 accounts_ready | wb_accounts.py is_ready/count_ready |
| storage: realm | `active_realm.json` 容忍额外时间字段（updated_at/updated_iso） | wb_proxy.py:5653-5690 |
| api: /health | 授权时补 5 身份字段：uid/domain/issuer/credential_file/expires_at；另抓授权后 fixture | wb_proxy.py:15794 |

### M2-b（推迟，不实施）——OAuth 写路径

`start_login`/`poll_login`/`cancel_login`/`import_desktop_credential` 及 `save()` 落盘。
理由：网络写路径 + 真实凭据落盘 + realm=cn 自动 checkin 副作用，与「不扰动运行中服务」冲突。
验证需隔离环境（独立 accounts 目录 + 测试账号），不能用生产凭据。**在计划中登记，不在本次实施。**

## 关键行为契约（从 Python 提炼）

1. JWT **只解码、不验签、不判过期**；`sub`→uid、`iss`→issuer、`exp`→expiry；解析失败一律 `0`/`""`（宽容，不拒绝账号）。Rust 侧同。
2. `normalize_epoch`：>1e11 的毫秒值 `/1000`；异常返回 `0`。
3. realm 优先级：强制 > token/issuer 证据 > domain 证据 > 行内 `realm` > `"intl"` 兜底；`realm_evidence` 看不出返回 `None`（不瞎猜），`detect_realm_from_token` 兜底 `"intl"`。
4. `uid` 净化：`re.sub(r"[^A-Za-z0-9_-]", "_", raw).strip("_ ")`，空则报错。
5. 导入：三种形状；缺 token 抛 `ValueError("no accessToken")`；`token.count(".") != 2` 抛 `ValueError("accessToken is not a JWT")`。
6. 导出文档：`format="workbuddy-accounts"`、`version=1`、`exportedAt="%Y-%m-%d %H:%M:%S"`（**本地时间**，非 ISO）、`count`、`accounts[]`。
7. `PORTABLE_EXPORT_FIELDS` = 15 项（uid…note）；`RUNTIME_ONLY_FIELDS` = (lastError, cooldownUntil)；`VOLATILE_FIELDS` = 7 项。
8. `active_realm.json` 读取端只取 `realm` 并校验 ∈ {intl,cn}，**容忍** `updated_at`/`updated_iso` 额外字段。
9. `credential_file` 只取 `os.path.basename(account.path)`。
10. 导入报告形状：`{added[], updated[], skipped[{uid,reason}], invalid[{index,reason}]}`，一行坏不打断其余，`index` 1-based。

## 验收

- `cargo test` 全绿（M1 11 个 + M2 新增）
- M2 新增契约测试**对照 Python 测试提炼的行为**（spec :64），用例来源：`tests/_test_cockpit_import.py`、`tests/_test_desktop_realm.py`、`tests/_test_error_persistence.py`
- `/health` 授权后 fixture（另抓）：**字段集 + 每字段类型** 与 Python 一致
- 不触碰运行中 Python 8788、不写真实 accounts/（只读副本验证）

## 风险

- JWT base64 的 `"=" * (-len(segment) % 4)` padding 在 Rust 侧要等价实现（base64 engine 的 padding 行为）
- `exportedAt` 本地时间：Rust 侧 `chrono::Local`，测试里避开时段边界
- `normalise_import_row` 的三形状 + cockpit snake_case 映射细节

## 任务分解

### Task 1：identity crate 骨架（jwt + realm）
- 新建 `crates/identity/`（lib，纯函数，无网络）
- `jwt.rs`：`jwt_claims`/`jwt_exp`/`jwt_uid`/`jwt_issuer` + `normalize_epoch`
- `realm.rs`：`realm_evidence`/`detect_realm_from_token`/`domain_for_realm`/`desktop_effective_realm` + `REALM_MARKERS`
- 对照 `_test_desktop_realm.py` 写测试
- 产出物：`cargo test -p wb-identity` 全绿

### Task 2：identity 导入导出
- `import.rs`：`normalise_import_row`（三形状、uid 净化、realm 优先级、volatile 重置）+ `Account` 字段集合
- `export.rs`：`account_to_export`/`build_export_document` + 三常量
- 对照 `_test_cockpit_import.py` 写测试
- 产出物：`cargo test -p wb-identity` 全绿 + 导出文档 JSON 形状对照

### Task 3：storage 扩展（Account 全字段 + ready + active_realm 容忍）
- 扩展 `load_snapshot`：Account 全字段、`count_ready()` 对齐
- `active_realm.json` 容忍额外时间字段
- 对照真实 accounts/（只读副本）验证
- 产出物：`cargo test -p wb-storage` 全绿

### Task 4：api /health 授权身份字段
- `/health` 授权时补 5 身份字段
- 另抓授权后 fixture（需 Python 侧以已授权 key 请求）
- 对照新增 fixture 写契约测试
- 产出物：`cargo test -p wb-api` 全绿 + 授权后 fixture 形状一致

### Task 5：ledger + CI 确认
- 更新 `docs/superpowers/plans/2026-10-16-rust-m2-identity-storage.md` 状态
- 确认 `rust.yml` 对新 crate 生效（paths 已覆盖 wb-proxy-rs/**）
- 产出物：ledger 更新 + `cargo fmt/clippy/test` 全绿

## 约束

- 不触碰 wb_proxy.py、dashboard.html、运行中 8788
- 凭据明文不落地/不日志
- OAuth（M2-b）不在本次实施

## 进度状态（2026-10-16 15:05 北京时间）

| Task | 状态 | 产出物 |
|---|---|---|
| 1 identity 骨架（jwt + realm） | ✅ 完成 | `cargo test -p wb-identity` 全绿（jwt/realm 用例） |
| 2 identity 导入导出 | ✅ 完成 | import/export 纯函数两件套 + 测试；**写池部分超出 M2-a 只读范围，登记留给 M2-b/M5** |
| 3 storage 扩展 | ✅ 完成 | `cargo test -p wb-storage` 全绿（7 unit + 5 active_realm + 13 api_keys = 25） |
| 4 api /health 授权身份字段 | ✅ 完成 | 授权契约 3 测试全绿；授权后 fixture（`health_python_auth.json`）形状一致；**key 三来源 `effective_api_key`（env > 面板 > launcher）接线 main.rs + 测试 config()**，修复 launcher 集成缝隙 |
| 5 ledger + CI 确认 | ✅ 完成 | `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` 全绿 |

全 workspace 汇总：**77 用例、18 套件全绿零警告**（wb-identity 28 + wb-storage 25 + wb-api 24）。
CI：`.github/workflows/rust.yml` paths 已覆盖 `wb-proxy-rs/**`，新 crate（identity/storage）变更自动触发；CI 的 clippy `-D warnings` 与 fmt 检查已本地复现通过。

M2-b（OAuth 写路径：start_login/poll_login/cancel_login/import_desktop_credential + save() 落盘）保持推迟，理由不变：网络写路径 + 真实凭据落盘 + realm=cn 自动 checkin 副作用，与「不扰动运行中 8788」冲突，验证需隔离环境。