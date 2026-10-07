use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rdep_protocol::relay::{RelayHello, RelayHelloAck};
use rdep_protocol::FrameCodec;
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::config::ServiceConfig;
use crate::db::Db;
use crate::storage::Storage;

/// 向 forwarder 注册的长连接池：维持 `cfg.max_sessions` 条常驻隧道连接。
///
/// 每条连接独立循环：拨号 → TLS → RelayHello → 把连接交给 rdep 会话处理
/// （停放等待某个 client 经 forwarder 转发过来的流量）；该 client 会话结束后
/// 连接断开，本循环重连补回池中。池中同时有 N 条可用隧道 ⇒ 同一 service 可并发
/// 服务最多 N 个 client（N = `RDEP_MAX_SESSIONS`，默认 4）。
pub async fn register_loop(
    cfg: ServiceConfig,
    storage: Arc<Storage>,
    db: Arc<Db>,
    scripts_dir: PathBuf,
) {
    let retry = Duration::from_secs(3);
    let n = cfg.max_sessions.max(1);
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let cfg = cfg.clone();
        let storage = storage.clone();
        let db = db.clone();
        let scripts_dir = scripts_dir.clone();
        handles.push(tokio::spawn(async move {
            loop {
                if let Err(e) = register_once(&cfg, &storage, &db, &scripts_dir).await {
                    tracing::debug!("forwarder session ended: {e:#}");
                }
                tokio::time::sleep(retry).await;
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

/// 单次注册：建立到 forwarder 的隧道并阻塞服务，直到该会话结束。
async fn register_once(
    cfg: &ServiceConfig,
    storage: &Arc<Storage>,
    db: &Arc<Db>,
    scripts_dir: &PathBuf,
) -> Result<()> {
    let server_name = ServerName::try_from(cfg.forwarder_host.clone())
        .map_err(|_| anyhow::anyhow!("invalid forwarder host: {}", cfg.forwarder_host))?;

    let mut roots = RootCertStore::empty();
    let pem = std::fs::read(&cfg.forwarder_ca).context("read forwarder CA")?;
    let certs = rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse forwarder CA")?;
    for c in certs {
        let _ = roots.add(c);
    }
    let tls_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(roots))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_cfg));

    let tcp = TcpStream::connect((cfg.forwarder_host.as_str(), cfg.forwarder_port))
        .await
        .context("dial forwarder")?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .context("forwarder tls handshake")?;
    let mut codec = FrameCodec::new(tls);

    // 发送 RelayHello 并等 Ack
    let hello = RelayHello {
        service_id: cfg.service_id.clone(),
        label: cfg.service_label.clone(),
        token: cfg.relay_token.clone(),
    };
    let body = postcard::to_allocvec(&hello)?;
    let frame = rdep_protocol::Frame::new(
        rdep_protocol::FrameType::CmdRequest,
        rdep_protocol::FrameFlags::new(),
        body,
    );
    tracing::debug!(forwarder = %cfg.forwarder_host, port = cfg.forwarder_port, service_id = %cfg.service_id, "relay: sending hello");
    codec.write_frame(&frame).await?;
    let resp = codec
        .read_frame()
        .await?
        .context("forwarder closed before ack")?;
    let ack: RelayHelloAck = postcard::from_bytes(&resp.payload)?;
    tracing::debug!(ok = ack.ok, message = %ack.message, "relay: hello ack");
    if !ack.ok {
        anyhow::bail!("forwarder rejected registration: {}", ack.message);
    }
    tracing::info!("registered to forwarder as {}", cfg.service_id);

    // 注册成功：这条连接现在就是一条等待 client 的隧道。
    // 交给 rdep 会话处理器（阻塞，直到某个 client 经 forwarder 完成一次会话）。
    let stream = codec.into_inner();
    let peer = format!("forwarder/{}", cfg.service_id);
    crate::session::handle(stream, storage.clone(), db.clone(), scripts_dir, &peer).await?;
    Ok(())
}
