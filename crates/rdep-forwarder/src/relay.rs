use std::sync::Arc;

use anyhow::Result;
use rdep_protocol::relay::{RelayConnect, RelayConnectResp, RelayHello, RelayHelloAck};
use rdep_protocol::{ErrorCode, Frame, FrameFlags, FrameType};
use rdep_protocol::FrameCodec;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::config::ForwarderConfig;
use crate::db::Db;
use crate::registry::Registry;
use crate::tls::load_server_config;

/// 启动 rdep-forwarder：同时监听 service 端口（注册）与 client 端口（会话）。
/// 运行到 `shutdown` 被触发后退出（停止两个 accept 循环）。
///
/// 与 service 一样支持优雅关闭：systemd `restart` 时不再把进行中的中转会话硬打断。
/// 关闭信号以 `watch` 广播——两个 accept 循环各持一份 receiver，且主流程也要等待。
pub async fn run_forwarder_until(
    config: ForwarderConfig,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let tls_cfg = load_server_config(&config.cert_path, &config.key_path)?;
    let acceptor = TlsAcceptor::from(tls_cfg);
    let registry = Registry::new(config.relay_token.clone());
    let db = Arc::new(Db::open(&config.db_path)?);

    // oneshot（单消费者）→ watch（可广播给两个 accept 循环 + 主流程）
    let (shut_tx, shut_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = shutdown.await;
        let _ = shut_tx.send(true);
    });

    let svc_listener = TcpListener::bind(&config.service_listen).await?;
    let cli_listener = TcpListener::bind(&config.client_listen).await?;
    tracing::info!(
        "rdep-forwarder listening: service={} client={}",
        config.service_listen,
        config.client_listen
    );

    // Web 管理后台（可选）
    if let Some(web_addr) = config.web_listen.clone() {
        let web_state = crate::web::WebState::new(db.clone(), registry.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::web::serve(web_state, &web_addr).await {
                tracing::warn!("web server ended: {e:#}");
            }
        });
    }

    // service 注册 accept 循环
    {
        let acceptor = acceptor.clone();
        let registry = registry.clone();
        let db = db.clone();
        let mut shut = shut_rx.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = shut.changed() => {
                        tracing::info!("service accept loop stopping");
                        break;
                    }
                    res = svc_listener.accept() => res,
                };
                let (sock, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("service accept error: {e}");
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let registry = registry.clone();
                let db = db.clone();
                tokio::spawn(async move {
                    match acceptor.accept(sock).await {
                        Ok(tls) => {
                            if let Err(e) = handle_service_conn(tls, registry, db).await {
                                tracing::debug!("service conn ({peer:?}) ended: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("service tls accept failed: {e}"),
                    }
                });
            }
        });
    }

    // client 会话 accept 循环
    {
        let acceptor = acceptor.clone();
        let registry = registry.clone();
        let db = db.clone();
        let mut shut = shut_rx.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = shut.changed() => {
                        tracing::info!("client accept loop stopping");
                        break;
                    }
                    res = cli_listener.accept() => res,
                };
                let (sock, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("client accept error: {e}");
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let registry = registry.clone();
                let db = db.clone();
                tokio::spawn(async move {
                    match acceptor.accept(sock).await {
                        Ok(tls) => {
                            if let Err(e) = handle_client_conn(tls, registry, db).await {
                                tracing::debug!("client conn ({peer:?}) ended: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("client tls accept failed: {e}"),
                    }
                });
            }
        });
    }

    // 等待关闭信号（原先是 `pending()`，导致 forwarder 永远无法优雅退出）
    let mut shut = shut_rx;
    let _ = shut.changed().await;
    tracing::info!("rdep-forwarder shutting down");
    Ok(())
}

/// 一直运行（不响应关闭信号）。
pub async fn run_forwarder(config: ForwarderConfig) -> Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _keep = Box::leak(Box::new(tx));
    run_forwarder_until(config, rx).await
}

/// 处理一条 service 注册连接：读 `RelayHello` → 回 Ack → 把隧道停放进注册表。
async fn handle_service_conn(
    tls: TlsStream<TcpStream>,
    registry: Arc<Registry>,
    db: Arc<Db>,
) -> Result<()> {
    let mut codec = FrameCodec::new(tls);

    // 第一帧必须是 RelayHello
    let frame = codec
        .read_frame()
        .await?
        .ok_or_else(|| anyhow::anyhow!("service closed before hello"))?;
    let hello: RelayHello = postcard::from_bytes(&frame.payload)
        .map_err(|e| anyhow::anyhow!("bad RelayHello: {e}"))?;

    if !registry.check_token(&hello.token) {
        let ack = RelayHelloAck {
            ok: false,
            message: "invalid relay token".into(),
        };
        codec
            .write_frame(&frame_of(FrameType::CmdResponse, &ack)?)
            .await?;
        return Ok(());
    }

    let ack = RelayHelloAck {
        ok: true,
        message: "registered".into(),
    };
    codec
        .write_frame(&frame_of(FrameType::CmdResponse, &ack)?)
        .await?;

    db.touch_service(&hello.service_id, &hello.label)?;
    tracing::info!("service registered: {} ({})", hello.service_id, hello.label);

    // 把隧道停放，等待某个 client 会话配对。
    // 注意：service 侧在收到 Ack 后会阻塞等待第一个 rdep 帧（由本连接转发过去）。
    let stream = codec.into_inner();
    registry.register(hello.service_id, stream).await;
    Ok(())
}

/// 处理一条 client 会话连接：读 `RelayConnect` → 路由 → 零解析双向透传。
async fn handle_client_conn(
    tls: TlsStream<TcpStream>,
    registry: Arc<Registry>,
    db: Arc<Db>,
) -> Result<()> {
    let mut codec = FrameCodec::new(tls);

    // 第一帧必须是 RelayConnect
    let frame = codec
        .read_frame()
        .await?
        .ok_or_else(|| anyhow::anyhow!("client closed before connect"))?;
    let rc: RelayConnect = postcard::from_bytes(&frame.payload)
        .map_err(|e| anyhow::anyhow!("bad RelayConnect: {e}"))?;

    if !registry.check_token(&rc.token) {
        let resp = RelayConnectResp {
            ok: false,
            code: ErrorCode::AuthFailed.code(),
            message: "invalid relay token".into(),
        };
        codec
            .write_frame(&frame_of(FrameType::CmdResponse, &resp)?)
            .await?;
        return Ok(());
    }

    // 审计：配对成功即记一次「该 service 服务了一个客户端会话」。
    // 放在 take 之前，保证即使后续隧道立刻断开也已被计数。
    if let Err(e) = db.record_client_session(&rc.target_service_id) {
        // 用 warn 而非 debug：审计写入失败不应被静默吞掉
        // （曾因记在 debug 导致一个 SQL 语法错误完全不可见）。
        tracing::warn!("record client session failed for {}: {e:#}", rc.target_service_id);
    }

    let Some(mut svc_stream) = registry.take(&rc.target_service_id).await else {
        let resp = RelayConnectResp {
            ok: false,
            code: ErrorCode::ForwarderNoService.code(),
            message: format!("service not online: {}", rc.target_service_id),
        };
        codec
            .write_frame(&frame_of(FrameType::CmdResponse, &resp)?)
            .await?;
        return Ok(());
    };

    // 路由成功：回 ok，之后连接转为零解析透传
    let resp = RelayConnectResp {
        ok: true,
        code: ErrorCode::Ok.code(),
        message: "connected".into(),
    };
    codec
        .write_frame(&frame_of(FrameType::CmdResponse, &resp)?)
        .await?;

    let mut client_stream = codec.into_inner();
    tracing::info!("relay session -> service {}", rc.target_service_id);

    // 零解析字节透传：client ⇄ forwarder ⇄ service
    let _ = tokio::io::copy_bidirectional(&mut client_stream, &mut svc_stream).await;
    tracing::info!("relay session ended for {}", rc.target_service_id);
    Ok(())
}

/// 构造一个承载握手响应的 Frame。
fn frame_of<T: serde::Serialize>(ft: FrameType, body: &T) -> Result<Frame> {
    let payload = postcard::to_allocvec(body)?;
    Ok(Frame::new(ft, FrameFlags::new(), payload))
}
