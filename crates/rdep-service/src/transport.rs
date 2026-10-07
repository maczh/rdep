//! 历史位置：rdep-service 曾自带帧编解码实现，现统一收敛到 `rdep-protocol::transport`，
//! 三端共用。此处仅做转发，避免改动 service 内部 `crate::transport::FrameCodec` 的引用。
pub use rdep_protocol::transport::FrameCodec;
