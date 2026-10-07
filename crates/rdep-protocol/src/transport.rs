use tokio::io::{AsyncReadExt, AsyncWriteExt};

use anyhow::{Context, Result};
use crate::{parse_frame, Frame, HEADER_LEN};

/// 在任意 `AsyncRead + AsyncWrite` 流上做 rdep 帧的读写。
///
/// 内部维护读缓冲，自动处理 TCP 粘包/拆包：缓冲区不足时继续读取，
/// 凑齐一个完整帧（`HEADER_LEN + payload_len`）后再解析。
pub struct FrameCodec<S> {
    stream: S,
    buf: Vec<u8>,
}

impl<S> FrameCodec<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(8192),
        }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: AsyncReadExt + Unpin> FrameCodec<S> {
    /// 读取并返回下一帧；连接关闭且无可读数据时返回 `None`。
    pub async fn read_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            if let Some(frame) = try_parse(&self.buf)? {
                let consumed = frame.1;
                self.buf.drain(..consumed);
                return Ok(Some(frame.0));
            }

            let mut tmp = [0u8; 8192];
            let n = self.stream.read(&mut tmp).await.context("read stream")?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                anyhow::bail!("connection closed in the middle of a frame");
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
}

impl<S: AsyncWriteExt + Unpin> FrameCodec<S> {
    /// 写入一帧（自动做 postcard→zstd→base64 编码）。
    pub async fn write_frame(&mut self, frame: &Frame) -> Result<()> {
        let bytes = frame.to_bytes().context("encode frame")?;
        self.stream.write_all(&bytes).await.context("write frame")?;
        self.stream.flush().await.context("flush")?;
        Ok(())
    }
}

/// 若缓冲区已包含一个完整帧则解析返回，否则 `Ok(None)` 表示需要更多数据。
fn try_parse(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    // 防御异常大的长度（避免无限等待）
    if len > 256 * 1024 * 1024 {
        anyhow::bail!("frame payload too large: {} bytes", len);
    }
    if buf.len() < HEADER_LEN + len {
        return Ok(None);
    }
    Ok(Some(parse_frame(buf)?))
}
