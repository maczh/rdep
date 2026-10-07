//! # rdep-protocol
//!
//! rdep 私有协议的纯类型与编解码层，**不依赖任何异步运行时**，供
//! rdep-client / rdep-service / rdep-forwarder 三端共用。
//!
//! 编码管线（发送方向）：`业务结构体 → postcard → zstd 压缩 → （可选）base64`
//! 解码方向逆序还原，flag 位声明启用了哪些步骤，保证「可还原」。
//!
//! 一个逻辑会话内划分三类通道，复用同一条 TLS 连接：
//! - 控制通道（CmdRequest / CmdResponse）
//! - 数据通道（DataChunk，可并发分片）
//! - 流通道（StreamPush，如 TAIL 主动推送）

pub mod relay;
pub mod transport;

pub use transport::FrameCodec;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

// ===========================================================================
// 常量
// ===========================================================================

/// 帧魔数 `"RDEP"`（大端）。
pub const MAGIC: u32 = 0x5244_4550;
/// 当前协议版本。
pub const PROTOCOL_VERSION: u8 = 0x01;
/// 默认分片大小（1 MiB），用于下载/上传切分。
pub const CHUNK_SIZE_DEFAULT: usize = 1 << 20;
/// 协议头固定长度：magic(4) + version(1) + type(1) + flags(1) + reserved(1) + len(4)。
pub const HEADER_LEN: usize = 12;

/// 传输方向（上传 / 下载），用于 UI 展示与事件标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Upload,
    Download,
}

// ===========================================================================
// 错误
// ===========================================================================

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("帧数据不完整")]
    Incomplete,
    #[error("魔数校验失败")]
    BadMagic,
    #[error("不支持的协议版本 {0}")]
    UnsupportedVersion(u8),
    #[error("未知的帧类型 {0}")]
    UnknownFrameType(u8),
    #[error("未知的指令 {0}")]
    UnknownCommand(u8),
    #[error("序列化失败: {0}")]
    Serialize(#[from] postcard::Error),
    #[error("base64 解码失败: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("压缩失败: {0}")]
    Compress(std::io::Error),
    #[error("解压失败: {0}")]
    Decompress(std::io::Error),
    #[error("分片校验失败 (index={0})")]
    ChunkChecksumFailed(u32),
    #[error("整文件 SHA-256 不匹配")]
    FileChecksumMismatch,
}

pub type Result<T> = std::result::Result<T, Error>;

/// 业务错误码（在 CmdResponse.code 中返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    Ok = 0,
    AuthFailed = 1001,
    SessionExpired = 1002,
    PathNotFound = 2001,
    PermissionDenied = 2002,
    NameConflict = 2003,
    ChunkChecksumFailed = 3001,
    FileChecksumMismatch = 3002,
    RestartFailed = 4001,
    ForwarderNoService = 5001,
    NotImplemented = 6001,
}

impl ErrorCode {
    pub fn code(self) -> u16 {
        self as u16
    }
}

// ===========================================================================
// 帧类型 / 标志位
// ===========================================================================

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameType {
    CmdRequest = 0x01,
    CmdResponse = 0x02,
    DataChunk = 0x03,
    StreamPush = 0x04,
    Ctrl = 0x05,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0x01 => FrameType::CmdRequest,
            0x02 => FrameType::CmdResponse,
            0x03 => FrameType::DataChunk,
            0x04 => FrameType::StreamPush,
            0x05 => FrameType::Ctrl,
            _ => return Err(Error::UnknownFrameType(v)),
        })
    }
}

/// 帧标志位（按位组合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameFlags(pub u8);

impl FrameFlags {
    pub const COMPRESSED: u8 = 0x01;
    pub const BASE64: u8 = 0x02;
    pub const CHUNK_CHECKSUM: u8 = 0x04;

    pub fn new() -> Self {
        FrameFlags(0)
    }
    /// 链式开启某标志位。
    pub fn with(mut self, bit: u8) -> Self {
        self.0 |= bit;
        self
    }
    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

// ===========================================================================
// 帧
// ===========================================================================

#[derive(Debug, Clone)]
pub struct Frame {
    pub version: u8,
    pub frame_type: FrameType,
    pub flags: FrameFlags,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(frame_type: FrameType, flags: FrameFlags, payload: Vec<u8>) -> Self {
        Frame {
            version: PROTOCOL_VERSION,
            frame_type,
            flags,
            payload,
        }
    }

    /// 序列化：写入帧头 + 编码后的 payload。
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let payload = encode_body(&self.payload, self.flags)?;
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.push(self.version);
        buf.push(self.frame_type as u8);
        buf.push(self.flags.0);
        buf.push(0); // reserved
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        Ok(buf)
    }
}

/// 从缓冲区解析一帧，返回解析出的帧与**已消费字节数**（便于流式分帧）。
pub fn parse_frame(buf: &[u8]) -> Result<(Frame, usize)> {
    if buf.len() < HEADER_LEN {
        return Err(Error::Incomplete);
    }
    let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != MAGIC {
        return Err(Error::BadMagic);
    }
    let version = buf[4];
    if version != PROTOCOL_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    let frame_type = FrameType::from_u8(buf[5])?;
    let flags = FrameFlags(buf[6]);
    // buf[7] reserved
    let len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    if buf.len() < HEADER_LEN + len {
        return Err(Error::Incomplete);
    }
    let payload_raw = &buf[HEADER_LEN..HEADER_LEN + len];
    let payload = decode_body(payload_raw, flags)?;
    let frame = Frame {
        version,
        frame_type,
        flags,
        payload,
    };
    Ok((frame, HEADER_LEN + len))
}

/// 编码管线：postcard 字节 → (可选) zstd → (可选) base64。
fn encode_body(body: &[u8], flags: FrameFlags) -> Result<Vec<u8>> {
    let mut p = body.to_vec();
    if flags.has(FrameFlags::COMPRESSED) {
        p = zstd::encode_all(&p[..], 3).map_err(Error::Compress)?;
    }
    if flags.has(FrameFlags::BASE64) {
        p = base64::engine::general_purpose::STANDARD.encode(&p).into_bytes();
    }
    Ok(p)
}

/// 解码管线：base64 → zstd → 还原为 postcard 字节。
fn decode_body(payload: &[u8], flags: FrameFlags) -> Result<Vec<u8>> {
    let mut p = payload.to_vec();
    if flags.has(FrameFlags::BASE64) {
        p = base64::engine::general_purpose::STANDARD
            .decode(&p)
            .map_err(Error::Base64)?;
    }
    if flags.has(FrameFlags::COMPRESSED) {
        p = zstd::decode_all(&p[..]).map_err(Error::Decompress)?;
    }
    Ok(p)
}

// ===========================================================================
// 通用哈希工具
// ===========================================================================

/// 计算 SHA-256，返回 `[u8;32]`。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

// ===========================================================================
// 指令集
// ===========================================================================

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmdType {
    Auth = 1,
    Ls,
    Download,
    Upload,
    Publish,
    Tail,
    Grep,
    Edit,
    Rollback,
    Delete,
    Copy,
    Move,
    Rename,
    Mkdir,
    Ping,
    PublishCommit,
}

impl CmdType {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => CmdType::Auth,
            2 => CmdType::Ls,
            3 => CmdType::Download,
            4 => CmdType::Upload,
            5 => CmdType::Publish,
            6 => CmdType::Tail,
            7 => CmdType::Grep,
            8 => CmdType::Edit,
            9 => CmdType::Rollback,
            10 => CmdType::Delete,
            11 => CmdType::Copy,
            12 => CmdType::Move,
            13 => CmdType::Rename,
            14 => CmdType::Mkdir,
            15 => CmdType::Ping,
            16 => CmdType::PublishCommit,
            _ => return Err(Error::UnknownCommand(v)),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdRequest {
    pub seq: u32,
    pub cmd: CmdType,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdResponse {
    pub seq: u32,
    pub ok: bool,
    pub code: u16,
    pub message: String,
    pub body: Vec<u8>,
}

impl CmdRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(self).map_err(Error::Serialize)
    }
    pub fn decode(buf: &[u8]) -> Result<Self> {
        postcard::from_bytes(buf).map_err(Error::Serialize)
    }
}

impl CmdResponse {
    pub fn encode(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(self).map_err(Error::Serialize)
    }
    pub fn decode(buf: &[u8]) -> Result<Self> {
        postcard::from_bytes(buf).map_err(Error::Serialize)
    }
    pub fn success(seq: u32, body: Vec<u8>) -> Self {
        CmdResponse {
            seq,
            ok: true,
            code: ErrorCode::Ok.code(),
            message: String::new(),
            body,
        }
    }
    pub fn failure(seq: u32, code: ErrorCode, message: impl Into<String>) -> Self {
        CmdResponse {
            seq,
            ok: false,
            code: code.code(),
            message: message.into(),
            body: Vec::new(),
        }
    }
}

// ===========================================================================
// 控制帧（CTRL）
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Ctrl {
    Ping,
    Pong,
    /// 停止跟随（tail follow）。
    Stop,
    // 说明：早期设计曾有 Resend/AckBitmap/Query 三个变体用于断点续传，
    // 但**从未实现**（service 只认 Ping，其余静默丢弃），而续传需求已由
    // `UploadInitAck { received }` 完整满足。保留它们会误导第二实现者去实现
    // 永远无效的逻辑，故移除。未知变体应被服务端**忽略而非断连**（见 session.rs）。
}

// ===========================================================================
// 指令载荷定义
// ===========================================================================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuthRequest {
    pub user: String,
    pub pass: String,
    pub method: AuthMethod,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum AuthMethod {
    Password,
    Token,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuthResponse {
    pub token: String,
    pub expires: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LsRequest {
    pub path: String,
    pub recursive: bool,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LsResponse {
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MkdirRequest {
    pub paths: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UploadInit {
    pub transfer_id: u64,
    pub remote_path: String,
    pub size: u64,
    pub mtime: i64,
    pub chunk_size: u32,
    pub total_chunks: u32,
    pub file_sha256: [u8; 32],
    /// Publish 时为 true，service 先备份再覆盖。
    pub backup_first: bool,
    /// 权限位（unix `st_mode & 0o777`；0 / 非 unix 平台表示不设置，service 用默认权限）。
    /// 用于保留可执行位等，避免部署后脚本丢失 `+x`。
    pub mode: u32,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UploadCommit {
    pub transfer_id: u64,
}
/// `UploadInit` 的响应：服务端已暂存的分片序号（升序），客户端据此只补传缺失片。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UploadInitAck {
    pub received: Vec<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PublishItem {
    pub remote_path: String,
    pub size: u64,
    pub chunks: u32,
    pub sha256: [u8; 32],
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PublishRequest {
    /// 远端部署目录。`project` 为 Some 时**以项目记录为准**，本字段被忽略。
    pub remote_dir: String,
    /// 重启脚本 ID。`project` 为 Some 时**以项目记录为准**，本字段被忽略。
    pub restart_script_id: String,
    /// 项目名：非空时 service 从 projects 表解析出 remote_dir / restart_script，
    /// 使「项目」成为部署配置的单一来源（而非仅 Web 端的展示记录）。
    pub project: Option<String>,
    pub items: Vec<PublishItem>,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PublishCommitRequest {
    pub remote_dir: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum NamePolicy {
    Overwrite,
    Rename,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DownloadRequest {
    pub remote_path: String,
    pub policy: NamePolicy,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TailRequest {
    pub path: String,
    pub lines: u32,
    pub follow: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GrepRequest {
    pub path: String,
    pub pattern: String,
    pub flags: String,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GrepResponse {
    pub lines: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EditRequest {
    pub remote_path: String,
    /// `None` = 读取文件内容（响应 body 为文件内容）；`Some` = 保存（覆盖，先备份）。
    pub content: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RollbackRequest {
    pub remote_dir: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeleteRequest {
    pub paths: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum CopyPolicy {
    Rename,
    Keep,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CopyRequest {
    pub src: Vec<String>,
    pub dst: String,
    pub policy: CopyPolicy,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MoveRequest {
    pub src: Vec<String>,
    pub dst_dir: String,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RenameRequest {
    pub src: String,
    pub new_name: String,
}

// ===========================================================================
// 数据分片（DataChunk）
// ===========================================================================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DataChunk {
    pub transfer_id: u64,
    pub index: u32,
    pub data: Vec<u8>,
    pub chunk_sha256: [u8; 32],
}

impl DataChunk {
    pub fn new(transfer_id: u64, index: u32, data: Vec<u8>) -> Self {
        let chunk_sha256 = sha256(&data);
        DataChunk {
            transfer_id,
            index,
            data,
            chunk_sha256,
        }
    }
}

/// 流推送（如 TAIL 实时行）。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StreamPush {
    pub transfer_id: u64,
    pub line: String,
    pub eof: bool,
}

// ===========================================================================
// 分片组装器 / 切分器（client 下载、service 上传合并共用）
// ===========================================================================

/// 按 `chunk_size` 切分数据为 `(index, data)` 列表。
pub fn split_file(data: &[u8], chunk_size: usize) -> Vec<(u32, Vec<u8>)> {
    data.chunks(chunk_size.max(1))
        .enumerate()
        .map(|(i, c)| (i as u32, c.to_vec()))
        .collect()
}

/// 分片收集器：按 index 暂存，commit 时重排合并并校验收到的整文件 SHA。
pub struct ChunkAssembler {
    total: u32,
    file_sha256: [u8; 32],
    chunks: HashMap<u32, Vec<u8>>,
}

impl ChunkAssembler {
    pub fn new(total: u32, file_sha256: [u8; 32]) -> Self {
        ChunkAssembler {
            total,
            file_sha256,
            chunks: HashMap::with_capacity(total as usize),
        }
    }

    /// 加入一片；`expected` 非空时校验分片 SHA。
    pub fn add(&mut self, index: u32, data: Vec<u8>, expected: Option<[u8; 32]>) -> Result<()> {
        if let Some(exp) = expected {
            if sha256(&data) != exp {
                return Err(Error::ChunkChecksumFailed(index));
            }
        }
        self.chunks.insert(index, data);
        Ok(())
    }

    /// 尚未收到的分片序号（升序），用于触发 Resend。
    pub fn missing(&self) -> Vec<u32> {
        (0..self.total)
            .filter(|i| !self.chunks.contains_key(i))
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.chunks.len() as u32 == self.total
    }

    /// 重排合并并校验整文件 SHA。
    pub fn assemble(&self) -> Result<Vec<u8>> {
        if !self.is_complete() {
            return Err(Error::Incomplete);
        }
        let mut out = Vec::with_capacity(self.total as usize * 1024);
        for i in 0..self.total {
            out.extend_from_slice(self.chunks.get(&i).expect("chunk present"));
        }
        if sha256(&out) != self.file_sha256 {
            return Err(Error::FileChecksumMismatch);
        }
        Ok(out)
    }
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_compressed_b64() {
        let chunk = DataChunk::new(1, 0, b"hello world, this is rdep".to_vec());
        let body = postcard::to_allocvec(&chunk).unwrap();

        let flags = FrameFlags::new()
            .with(FrameFlags::COMPRESSED)
            .with(FrameFlags::BASE64);
        let frame = Frame::new(FrameType::DataChunk, flags, body.clone());
        let bytes = frame.to_bytes().unwrap();

        let (parsed, consumed) = parse_frame(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed.frame_type, FrameType::DataChunk);
        assert_eq!(parsed.flags, flags);
        assert_eq!(parsed.payload, body);

        let back: DataChunk = postcard::from_bytes(&parsed.payload).unwrap();
        assert_eq!(back.data, b"hello world, this is rdep");
        assert_eq!(back.chunk_sha256, chunk.chunk_sha256);
    }

    #[test]
    fn frame_roundtrip_plain() {
        let req = CmdRequest {
            seq: 42,
            cmd: CmdType::Ls,
            body: postcard::to_allocvec(&LsRequest {
                path: "/opt/app".into(),
                recursive: false,
            })
            .unwrap(),
        };
        let body = req.encode().unwrap();
        let frame = Frame::new(FrameType::CmdRequest, FrameFlags::new(), body);
        let bytes = frame.to_bytes().unwrap();
        let (parsed, _) = parse_frame(&bytes).unwrap();
        let back = CmdRequest::decode(&parsed.payload).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.cmd, CmdType::Ls);
    }

    #[test]
    fn chunk_assembler_full_and_verify() {
        let data: Vec<u8> = (0..5000u32).map(|x| (x % 251) as u8).collect();
        let file_sha = sha256(&data);
        let parts = split_file(&data, 1000);
        let total = parts.len() as u32;

        let mut asm = ChunkAssembler::new(total, file_sha);
        assert_eq!(asm.missing().len() as u32, total);

        for (i, p) in &parts {
            let cs = sha256(p);
            asm.add(*i, p.clone(), Some(cs)).unwrap();
        }
        assert!(asm.is_complete());
        assert!(asm.missing().is_empty());

        let out = asm.assemble().unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn chunk_assembler_missing_and_bad_sha() {
        let data: Vec<u8> = vec![7u8; 2500];
        let file_sha = sha256(&data);
        let parts = split_file(&data, 1000); // 3 片
        let mut asm = ChunkAssembler::new(parts.len() as u32, file_sha);

        // 只加第 0、2 片
        asm.add(0, parts[0].1.clone(), Some(sha256(&parts[0].1))).unwrap();
        asm.add(2, parts[2].1.clone(), Some(sha256(&parts[2].1))).unwrap();
        assert!(!asm.is_complete());
        assert_eq!(asm.missing(), vec![1]);

        // 错误分片 SHA 应被拒绝
        let bad = vec![0u8; 100];
        let res = asm.add(1, bad, Some([9u8; 32]));
        assert!(matches!(res, Err(Error::ChunkChecksumFailed(1))));
    }

    #[test]
    fn response_builders() {
        let ok = CmdResponse::success(1, vec![1, 2, 3]);
        assert!(ok.ok && ok.code == 0);
        let fail = CmdResponse::failure(2, ErrorCode::AuthFailed, "bad creds");
        assert!(!fail.ok && fail.code == 1001);
    }
}
