//! # rdep-service
//!
//! rdep 服务端（Phase 1：核心文件能力）。
//! 提供 TLS 监听、rdep 协议分发、文件管理（LS/MKDIR/UPLOAD/DOWNLOAD/DELETE/COPY/MOVE/RENAME）、
//! 基于 SQLite 的账户认证。备份/回滚/发布等高级能力在后续阶段接入。

pub mod config;
pub mod password;
pub mod db;
pub mod registry;
pub mod server;
pub mod session;
pub mod storage;
pub mod tls;
pub mod transport;
pub mod web;

pub use config::ServiceConfig;
pub use registry::register_loop;
pub use server::{run_service, run_service_until};
