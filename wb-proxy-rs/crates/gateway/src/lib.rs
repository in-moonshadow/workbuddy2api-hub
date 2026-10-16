//! 上游代理与调度（M3）：端点表、出站头构造、池与槽位、流式转发与聚合。
//!
//! 依赖链 identity → gateway → api（spec :50）。Task 1 只做端点表与出站头
//! 构造（纯函数）；池/槽位/转发随 Task 3/4 补齐。

pub mod endpoints;
pub mod headers;