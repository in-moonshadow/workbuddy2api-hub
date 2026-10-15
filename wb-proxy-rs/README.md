# wb-proxy-rs

WorkBuddy 网关的 Rust 重写（影子实例，默认 127.0.0.1:8789）。

设计与里程碑见 `../docs/superpowers/specs/2026-10-15-rust-refactor-design.md`。
运行中的 Python 服务（8788）在切换前保持不动；本目录只读同一份
`accounts/`、`usage/` legacy 数据。

    cargo fmt --check
    cargo clippy -- -D warnings
    cargo test
    PORT=8789 cargo run -p wb-api
