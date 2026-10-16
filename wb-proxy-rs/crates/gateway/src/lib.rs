//! 上游代理与调度（M3）：端点表、出站头构造、body 变形管线、池与槽位、
//! 流式转发与聚合。
//!
//! 依赖链 identity → gateway → api（spec :50）。Task 1 端点表/出站头（纯函数）、
//! Task 2 body 变形管线（纯函数）；池/槽位/转发随 Task 3/4 补齐。

pub mod anthropic;
pub mod body;
pub mod egress;
pub mod endpoints;
pub mod forward;
pub mod headers;
pub mod leaked;
pub mod notes;
pub mod outbound;
pub mod pool;
pub mod prompt;
pub mod retry;
pub mod sanitize;
pub mod stream;
pub mod upstream;
