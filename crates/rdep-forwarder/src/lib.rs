//! # rdep-forwarder
//!
//! 公网中转代理（Phase 5）：在 NAT/内网的 rdep-service 与公网 rdep-client 之间
//! 做**零解析字节透传**。
//!
//! 两个监听端口：
//! - **service 端口**：rdep-service 主动拨号注册（`RelayHello`），注册后其隧道连接
//!   停放在注册表，等待被 client 会话配对；
//! - **client 端口**：rdep-client 送来 `RelayConnect{target_service_id}`，
//!   forwarder 取出对应 service 的隧道连接，用 `copy_bidirectional` 双向透传。
//!
//! 握手只发生在连接第一帧（复用 rdep Frame 编解码）；握手成功后 forwarder 不再解析
//! rdep 协议内容，只按字节转发。

pub mod config;
pub mod password;
pub mod db;
pub mod registry;
pub mod relay;
pub mod tls;
pub mod web;

pub use config::ForwarderConfig;
pub use relay::{run_forwarder, run_forwarder_until};
