# M1 (Rust baseline) 执行进度

plan: docs/superpowers/plans/2026-10-15-rust-m1-baseline.md
spec: docs/superpowers/specs/2026-10-15-rust-refactor-design.md
branch: feat/rust-m1
started: 2026-10-15

- Task 1: complete (commits 9726f7b4..52e1cb8e, review clean; 首轮 1 Important[MSRV] + 1 波修复后复审 Approved)
  - 遗留 Minor（交最终整分支审查裁决）：
    - M1: main.rs 的 ACCOUNTS_DIR/WB_ACCOUNTS_DIR 空串会静默取空路径（Python 语义为空串=未设置）→ 已并入 Task 2 开工前修
    - M2: 计划文档 :636/:813 仍写 WB_ACCOUNTS_DIR（回退仍有效，非 bug）→ 已并入 Task 2 开工前改
    - M3: HOST/WB_DASHBOARD/WB_LAN 的非 UTF-8 仍静默回落（用户仅裁定 PORT）
    - M4: 报告记账：diff 新增/删除分列勿用 stat 合计列
    - M5: Cargo.lock 为 format v4（cargo>=1.83），与 MSRV 1.85 自洽；CI 须用 stable
  - 用户裁定（2026-10-15）：MSRV=1.85；PORT 非法即 exit(2)；tower 统一 0.5；ACCOUNTS_DIR 优先
  - 控制器裁决：fmt/clippy/test 的"全绿"按改动 crate 计量（wb-api 在 Task 3 前不可编译，E0583 属预期）
- Task 2: complete (commit c5c25c7e + fix 2e61abe6)
  - 审查发现 1 Important：api_key_configured 漏判 settings.json 的 launcher_key，
    会把「要求 key」的真实实例误报为 api_key_required=false。证据：真实
    settings.json 只有 launcher_key；8788 /health 返回 true，而 Python
    auth_required() 读启动时灌入的全局 API_KEY（launcher mode 会填充）。
  - 已修 + 增 2 条回归（launcher_key_requires_a_key / auth_disabled_wins_over_launcher_key）
  - 验证：cargo test -p wb-storage 7 passed、fmt --check、clippy -D warnings 零警告
  - 控制器的 Task 2 复审结论：Approved（其余为既有 Minor，见 Task 1 遗留项）
- Task 3: complete (commit ad30e4b9)
  - 新建 config.rs / routes.rs / tests/health.rs，lib.rs 增 app()
  - TDD：先确认 RED（E0583 缺模块）→ GREEN（health_reports_pool_shape ok）
  - 验证：cargo test -p wb-api 1 passed、fmt --check、clippy -D warnings 零警告
  - 手动对照真实实例 GET 127.0.0.1:8788/health：字段名一致，
    api_key_required=true（正是 Task 2 修复所防的误报口径）
  - 说明：M1 只暴露 5 个基础字段；identity 字段（uid/domain/issuer/
    credential_file/expires_at）待 M2。accounts_ready 为保守近似，差异记入 Task 5 fixture
- Task 4: complete (commit 3961396f)
  - routes.rs 增 GET /：语言占位替换 + ETag(mtime_ns/size/language) + no-cache，
    If-None-Match 命中回 304 空体；新增 tests/dashboard.rs
  - TDD：先确认 RED（404）→ GREEN
  - 验证：cargo test -p wb-api 两文件全 PASS、fmt --check、clippy -D warnings 零警告
  - 真实数据只读探针（绝对路径、不监听端口）：200 / text/html; charset=utf-8 /
    no-cache / ETag 存在 / 占位替换为 zh / 条件请求 304 空体 / 637976 bytes
  - 探针副产物：dashboard.html 缺失时正确回 500 error envelope；
    注意集成测试进程 cwd 是 crates/api，相对路径 ../dashboard.html 会解析失败
    （main.rs 的默认值只在真正运行时 cwd=wb-proxy-rs 才成立）
- Task 5: co​mplete (commit f0fc52fd)
  - 新增 crates/api/fixtures/health_python.json（8788 实测抓取）+ 
    crates/api/tests/contract_health.rs；README 追加「契约 fixture」节
  - assert_same_shape：字段名集合 + 每字段 discriminant 类型；值允许不同
  - **计划缺陷（已绕开）**：计划 :833/:924 写 wb-proxy-rs/tests/ 与
    `cargo test --test contract_health`，但 wb-proxy-rs/Cargo.toml 是
    [workspace] 虚拟清单（无 [package]），workspace 根不承载测试 target；
    实测报 `no test target named contract_health`。改为与既有
    health.rs/dashboard.rs 同构的 crates/api/tests/，fixture 落
    crates/api/fixtures/（include_str! 基准是 crates/api，已用探针验证）
  - 可证伪校验：fixture 变异「多字段」→ field sets differ 失败；
    「bool 改 number」→ type of api_key_required differs 失败；还原转绿
  - 验证：cargo test -p wb-api 三文件全 PASS、fmt --check、clippy -D warnings 零警告
  - 遗留：M1 fixture 为未授权响应，只含 5 基础字段；identity 字段属 M2，
    届时须另抓授权后的 fixture（已写入 README 与测试注释）
- Task 6: co​mplete (commit 83423c69)
  - 新增 .github/workflows/rust.yml：paths 过滤 wb-proxy-rs/**，跑 fmt/clippy/test
  - 与 tests.yml（Python 3.9/3.12 矩阵）无重叠；加 concurrency 取消旧运行
  - 工具链 stable（Cargo.lock format v4 需 cargo>=1.83；MSRV 1.85，见 M5）
  - 三步拆独立 step（不 && 串联，否则只暴露最后一个退出码）
  - 验证：本地复现三条 CI 命令全 exit=0（11 测试全 PASS）；
    PyYAML 解析通过；全仓检索确认既有 5 个 workflow 无 rust/cargo 覆盖
  - 核实：Swatinem/rust-cache 的 workspaces 相对仓库根（action 文档确认），
    与 job 级 defaults.run.working-directory 并存正确

## M1 全部任务完成（Task 1-6）

commits: 9726f7b4..52e1cb8e (T1) / c5c25c7e+2e61abe6 (T2) / ad30e4b9 (T3)
         / 3961396f (T4) / f0fc52fd (T5) / 83423c69 (T6)
未推送：git push origin feat/rust-m1 仍受本网络 TLS 失败阻塞
