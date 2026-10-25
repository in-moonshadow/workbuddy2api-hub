//! 上游代理与调度（M3）：端点表、出站头构造、body 变形管线、池与槽位、
//! 流式转发与聚合。
//!
//! 依赖链 identity → gateway → api（spec :50）。Task 1 端点表/出站头（纯函数）、
//! Task 2 body 变形管线（纯函数）；池/槽位/转发随 Task 3/4 补齐。

pub mod acp;
pub mod anthropic;
pub mod body;
pub mod cap;
pub mod credits;
pub mod egress;
pub mod endpoints;
pub mod forward;
pub mod headers;
pub mod leaked;
pub mod notes;
// 关卡 4.24（线 A）：OAuth 登录的上游 HTTP 那一半（Python `start_login`/
// `poll_login` 的网络请求；状态机与账号落地在 tasks 侧 login 路径）。
pub mod oauth;
pub mod outbound;
pub mod pool;
pub mod prompt;
pub mod responses;
// 关卡 4.19：两个后台线程的 **HTTP 那一半**——`pricing` 取 OpenRouter 价目
// （Python `wb_pricing.fetch_openrouter`），`github` 取 releases（Python
// `wb_updates.fetch_releases`）。本 crate 只取回原文，不塑形。
pub mod github;
pub mod pricing;
// 关卡 4.21（线 C）：**远端产品目录**的 HTTP 那一半——拉 `<chat_upstream>/v3/config`
// 并解析成 (ids, credits)（Python `wb_proxy.fetch_remote_product_config`）。
pub mod product_catalog;
pub mod retry;
pub mod sanitize;
pub mod stream;
pub mod upstream;
