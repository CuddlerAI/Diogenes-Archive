//! 独立 depth-ingest 服务适配层。
//!
//! 这里把 `depth-lab::archive_contract::ingest` 的纯状态机接到 HTTP、独立认证、
//! durable depth index 与 Cloudflare R2。它没有、也不允许依赖生产 `apps/api`、
//! `artifact_objects`、生产 `R2_BUCKET` 或 production uploader credential。

pub mod auth;
pub mod boundary;
pub mod config;
pub mod http;
pub mod r2;
pub mod repository;

pub use boundary::{BoundaryError, DurableDepthIngest};
pub use http::{HttpState, build_router};
