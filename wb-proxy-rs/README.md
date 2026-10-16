# wb-proxy-rs

WorkBuddy 网关的 Rust 重写（影子实例，默认 127.0.0.1:8789）。

设计与里程碑见 `../docs/superpowers/specs/2026-10-15-rust-refactor-design.md`。
运行中的 Python 服务（8788）在切换前保持不动；本目录只读同一份
`accounts/`、`usage/` legacy 数据。

    cargo fmt --check
    cargo clippy -- -D warnings
    cargo test
    PORT=8789 cargo run -p wb-api

## 契约 fixture

`crates/api/fixtures/health_python.json` 是从运行中的 Python 实例抓取的真实响应：

    curl -s http://127.0.0.1:8788/health > wb-proxy-rs/crates/api/fixtures/health_python.json

`crates/api/tests/contract_health.rs` 拿影子实例的 `/health` 与该 fixture 比对：
**字段集必须一致、每个字段的 JSON 类型必须一致**（值本身允许不同，账号池与开关
因机器而异）。改动 Python 侧 `/health` 字段或类型时，必须重新抓取并同步该 fixture，
否则契约测试会失败。

注意：M1 阶段 fixture 是**未授权**请求的响应，只含 5 个基础字段；身份字段
（`uid`/`domain`/`issuer`/`credential_file`/`expires_at`）属 M2，届时需另抓一份
授权后的 fixture。
