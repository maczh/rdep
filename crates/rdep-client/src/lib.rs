//! # rdep-client
//!
//! rdep 桌面客户端（Phase 2：基础 UI + 连接 + 文件操作）。
//! 通过 `rdep-protocol` 与 rdep-service 通信，提供双栏文件浏览、传输列表、站点管理。
//!
//! 网络层采用「GUI 主线程 + 后台 Tokio 线程」桥接：
//! - 指令从 GUI 经 `tokio::sync::mpsc` 发往后台；
//! - 事件经 `std::sync::mpsc` 回传 GUI，GUI 每帧 `try_recv` 后刷新界面。
//!
//! `gui` feature（默认开启）引入 egui/eframe 提供桌面窗口；关闭后仅保留
//! 无界面的网络核心 `Client`，便于在 CI/无显示环境下做集成测试。
//!
//! 协议后端三种（`sites::Protocol`）：rdep（自有，全功能）/ SFTP（russh）/
//! FTP（suppaftp，仅基础操作）。多语言（en / zh-CN / zh-TW，默认英文）见
//! `i18n`；CJK 字体加载见 `fonts`。

pub mod client;
pub mod ftp;
pub mod i18n;
pub mod logging;
pub mod sftp;
pub mod sites;

#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub mod fonts;

pub use client::{Client, Command, ConnectParams, Event, PublishFile};
pub use ftp::{FtpClient, FtpParams};
pub use rdep_protocol::Direction;
pub use sftp::{SftpClient, SftpParams};
pub use sites::{Protocol, Site, SiteStore};

#[cfg(feature = "gui")]
pub use app::RdepApp;
