//! wb-identity：账号身份、realm 归属、JWT 解码与导入导出（纯函数，无网络）。
//!
//! 对齐 spec `:39`「identity/ # 账号、realm、JWT、导入导出、OAuth」的读路径；
//! OAuth 写路径（M2-b）不在本 crate。

pub mod jwt;
pub mod realm;
