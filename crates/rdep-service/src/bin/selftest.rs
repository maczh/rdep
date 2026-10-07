//! rdep-service 自测客户端（Phase 1）
//!
//! 在 127.0.0.1:18443 启动服务，用 TLS 连上后依次执行：
//! AUTH → MKDIR(多级) → LS → UPLOAD → DOWNLOAD → RENAME → COPY → MOVE → DELETE，
//! 全部通过则打印 `ALL SELFTEST PASSED`。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rdep_protocol::{
    AuthMethod, AuthRequest, CmdRequest, CmdResponse, CmdType, DataChunk, DeleteRequest,
    DownloadRequest, Frame, FrameFlags, FrameType, LsRequest, LsResponse, MkdirRequest,
    MoveRequest, NamePolicy, RenameRequest, CopyPolicy, CopyRequest, UploadCommit, UploadInit,
    sha256, split_file,
};
use rustls::pki_types::ServerName;
use rdep_service::transport::FrameCodec;
use rdep_service::{run_service, ServiceConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;

const ADDR: &str = "127.0.0.1:18443";

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;

    // 准备隔离的测试根目录与数据库
    let root = cwd.join("target/selftest-root");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    let db = cwd.join("target/selftest.db");
    let _ = std::fs::remove_file(&db);

    let certs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("certs");
    let config = ServiceConfig {
        listen_addr: ADDR.to_string(),
        root_dir: root.clone(),
        cert_path: certs_dir.join("server.crt"),
        key_path: certs_dir.join("server.key"),
        db_path: db,
        scripts_dir: cwd.join("target/selftest-scripts"),
        web_listen: None,
        use_forwarder: false,
        forwarder_host: String::new(),
        forwarder_port: 9444,
        forwarder_ca: certs_dir.join("server.crt"),
        relay_token: "rdep-relay-token".into(),
        service_id: String::new(),
        service_label: String::new(),
        max_sessions: 4,
        staging_ttl_hours: 24,
    };

    // 启动服务（后台任务，进程退出即停）
    tokio::spawn(async move {
        let _ = run_service(config).await;
    });
    // 等待监听就绪
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    let (mut codec, _conn) = connect().await?;
    let mut seq: u32 = 0;

    // AUTH
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Auth,
        &AuthRequest {
            user: "admin".into(),
            pass: "admin".into(),
            method: AuthMethod::Password,
        },
    )
    .await?;
    assert!(resp.ok, "AUTH failed: {}", resp.message);
    println!("[ok] AUTH");

    // MKDIR 多级
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Mkdir,
        &MkdirRequest {
            paths: vec!["/a/b/c".into()],
        },
    )
    .await?;
    assert!(resp.ok, "MKDIR failed: {}", resp.message);
    println!("[ok] MKDIR /a/b/c");

    // LS 根目录，应能看到 a/
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Ls,
        &LsRequest {
            path: "/".into(),
            recursive: false,
        },
    )
    .await?;
    assert!(resp.ok, "LS failed: {}", resp.message);
    let lsr: LsResponse = postcard::from_bytes(&resp.body)?;
    assert!(
        lsr.entries.iter().any(|e| e.name == "a" && e.is_dir),
        "LS root missing a/: {:?}",
        lsr.entries
    );
    println!("[ok] LS / -> found a/");

    // UPLOAD 一个小文件（分两片）
    let content: Vec<u8> = b"hello rdep service, this is a test file".to_vec();
    let file_sha = sha256(&content);
    let parts = split_file(&content, 16);
    let transfer_id = 0x100u64;
    let total = parts.len() as u32;

    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Upload,
        &UploadInit {
            transfer_id,
            remote_path: "/a/b/c/hello.txt".into(),
            size: content.len() as u64,
            mtime: 0,
            chunk_size: 16,
            total_chunks: total,
            file_sha256: file_sha,
            backup_first: false,
            mode: 0,
        },
    )
    .await?;
    assert!(resp.ok, "UPLOAD init failed: {}", resp.message);

    for (i, chunk) in &parts {
        let dc = DataChunk::new(transfer_id, *i, chunk.clone());
        let f = Frame::new(
            FrameType::DataChunk,
            FrameFlags::new(),
            postcard::to_allocvec(&dc)?,
        );
        codec.write_frame(&f).await?;
    }

    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Upload,
        &UploadCommit { transfer_id },
    )
    .await?;
    assert!(resp.ok, "UPLOAD commit failed: {}", resp.message);
    println!("[ok] UPLOAD /a/b/c/hello.txt ({} chunks)", total);

    // DOWNLOAD，内容应一致
    seq += 1;
    let got = download(&mut codec, seq, "/a/b/c/hello.txt").await?;
    assert_eq!(got, content, "DOWNLOAD content mismatch");
    println!("[ok] DOWNLOAD content matches");

    // RENAME
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Rename,
        &RenameRequest {
            src: "/a/b/c/hello.txt".into(),
            new_name: "hi.txt".into(),
        },
    )
    .await?;
    assert!(resp.ok, "RENAME failed: {}", resp.message);
    println!("[ok] RENAME -> hi.txt");

    // COPY 到 /a/b/copy.txt
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Copy,
        &CopyRequest {
            src: vec!["/a/b/c/hi.txt".into()],
            dst: "/a/b/copy.txt".into(),
            policy: CopyPolicy::Keep,
        },
    )
    .await?;
    assert!(resp.ok, "COPY failed: {}", resp.message);
    println!("[ok] COPY -> /a/b/copy.txt");

    // MOVE /a/b/copy.txt -> /a/copy.txt
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Move,
        &MoveRequest {
            src: vec!["/a/b/copy.txt".into()],
            dst_dir: "/a".into(),
        },
    )
    .await?;
    assert!(resp.ok, "MOVE failed: {}", resp.message);
    println!("[ok] MOVE -> /a/copy.txt");

    // DELETE 两个文件
    seq += 1;
    let resp = request(
        &mut codec,
        seq,
        CmdType::Delete,
        &DeleteRequest {
            paths: vec!["/a/b/c/hi.txt".into(), "/a/copy.txt".into()],
        },
    )
    .await?;
    assert!(resp.ok, "DELETE failed: {}", resp.message);
    println!("[ok] DELETE hi.txt + copy.txt");

    println!("\nALL SELFTEST PASSED");
    Ok(())
}

/// 发送一个 CmdRequest 并读取其 CmdResponse。
async fn request<S>(
    codec: &mut FrameCodec<S>,
    seq: u32,
    cmd: CmdType,
    body: &impl serde::Serialize,
) -> Result<CmdResponse>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let req = CmdRequest {
        seq,
        cmd,
        body: postcard::to_allocvec(body)?,
    };
    let frame = Frame::new(
        FrameType::CmdRequest,
        FrameFlags::new(),
        req.encode()?,
    );
    codec.write_frame(&frame).await?;
    let resp_frame = codec
        .read_frame()
        .await?
        .context("expected response frame")?;
    Ok(CmdResponse::decode(&resp_frame.payload)?)
}

/// 下载文件：服务端先发 DataChunk 流，最后发 CmdResponse。
async fn download<S>(codec: &mut FrameCodec<S>, seq: u32, remote_path: &str) -> Result<Vec<u8>>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let dl = DownloadRequest {
        remote_path: remote_path.into(),
        policy: NamePolicy::Overwrite,
    };
    let req = CmdRequest {
        seq,
        cmd: CmdType::Download,
        body: postcard::to_allocvec(&dl)?,
    };
    codec
        .write_frame(&Frame::new(
            FrameType::CmdRequest,
            FrameFlags::new(),
            req.encode()?,
        ))
        .await?;

    let mut got = Vec::new();
    loop {
        let f = codec.read_frame().await?.context("eof during download")?;
        match f.frame_type {
            FrameType::DataChunk => {
                let dc: DataChunk = postcard::from_bytes(&f.payload)?;
                got.extend_from_slice(&dc.data);
            }
            FrameType::CmdResponse => {
                let r = CmdResponse::decode(&f.payload)?;
                assert!(r.ok, "DOWNLOAD failed: {}", r.message);
                break;
            }
            _ => {}
        }
    }
    Ok(got)
}

/// 建立到 127.0.0.1:18443 的 TLS 连接。
/// 自测使用本地自签证书，将其作为根 CA 信任（等价于信任该服务端证书）。
async fn connect(
) -> Result<(FrameCodec<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, ())> {
    let cert_pem = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("certs/server.crt"))?;
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = std::io::Cursor::new(cert_pem);
    for c in rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        roots.add(c)?;
    }

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(roots))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let server_name = ServerName::try_from("localhost").unwrap();
    let tcp = tokio::net::TcpStream::connect(ADDR).await?;
    let tls = connector.connect(server_name, tcp).await?;
    Ok((FrameCodec::new(tls), ()))
}
