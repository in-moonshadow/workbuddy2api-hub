# Rust 全量重构设计（workbuddy2api-hub）

日期：2026-10-15
状态：已批准（shadow instance on new port + Strangler 包壳；期间不扰动运行中的 Python 容器）
作者：用户 + Claude

## 1. 背景与目标

当前系统是 Python 实现的多账号 WorkBuddy 反向代理网关（wb_proxy ~19k 行、wb_accounts ~3.6k 行 + 20 余子模块 + dashboard.html ~11.7k 行）。
目标：性能/资源、类型安全、单二进制部署；排除桌面 VM 自动化。

## 2. 约束（已与用户确认）

1. 向后兼容 100%：REST endpoints、JSON 字段名、workbuddy-accounts v1 导入/导出、Docker Compose 行为均保持不变。
2. 不扰动运行中服务；除非要求，不动 Python 容器 8788。
3. 重构完成后才切换；Rust 先影子运行。
4. 每步可运行、可验证、可回滚。

## 3. 总体架构

Strangler Fig + Shadow：

- Rust 影子实例监听 8789，完全复刻 API 合约；Python 8788 继续真实流量。
- 对照回放 equivalence；用户决定切换后下线 Python。
- 前端先原样静态托管，重写可选。

```
:8788  Python（保持不动）
:8789  Rust  api/gateway/tasks/identity
        |
        legacy JSON（accounts/usage）
```

## 4. Rust workspace

```
wb-proxy-rs/
  crates/
    identity/   # 账号、realm、JWT、导入导出、OAuth
    gateway/    # 上游代理、槽位调度、限流、健康探测
    tasks/      # growth、签到、打卡、claim-all
    storage/    # legacy JSON 读写、原子写、usage 聚合
    api/        # HTTP、鉴权、合约、静态前端托管
    shadow/     # 回放/对等测试夹具
  tests/       # contract tests
```

## 5. 组件职责

identity/gateway/tasks/storage/api/shadow，按依赖链 identity → gateway/tasks → api；每个 crate 单职责、可独立测。

## 6. 数据与持久化

- 保持 legacy JSON 格式，原子写 + 单实例；影子默认只读或 dry-run。
- RUNTIME_ONLY_FIELDS / VOLATILE_FIELDS 语义严格复刻；modelCaps 重启语义显式建模。

## 7. API 兼容契约

- 鉴权、端点清单、字段名、错误结构、ETag（mtime_ns,size,language）全部复刻；
- contract tests 覆盖每个端点。

## 8. 等价性与测试策略

1. 契约测试：从 Python 测试提炼 behavior。
2. Shadow 回放：8789 vs 8788 同请求比 diff（白名单噪声）。
3. 数据往返：真实导出文件 fixture 等价（volatile 除外）。
4. 里程碑全绿，截图/输出入 archive。

## 9. 错误处理与安全

分层错误，不吞错；token 明文不落地/不日志；masked settings；不引入运行时代理 env。

## 10. 里程碑与关卡

M1 基线/脚手架；M2 identity+storage；M3 gateway；M4 tasks；M5 面板/设置/用量；M6 影子上线验收；M7 切换/下线。
各里程碑产出物为实际 cargo test 与回放 diff。

## 11. Out of scope

前端重写、VM 自动化、DB 迁移、API breaking changes。

## 12. 验收标准

契约/回放/数据往返/health/资源占用全部达标；用户确认后切换。

## 13. 风险

上游 checkin 多变导致 rebase drift；单文件隐性耦合；并发 legacy 数据写入。

## 14. 第一步（已批准）

M1：Rust workspace + 最小 /health 与静态面板 + contract 测试框架 + CI 命令， prove baseline。
