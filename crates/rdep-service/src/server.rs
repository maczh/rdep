use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::config::ServiceConfig;
use crate::db::Db;
use crate::storage::Storage;
use crate::tls::load_server_config;

/// 启动 rdep-service：加载 TLS 配置，监听并接受连接，每个连接交给 `session::handle`。
///
/// 该函数在成功绑定后会进入无限接受循环，不会返回（由调用方在独立任务中 spawn）。
/// 一直运行到 `shutdown` 被触发（收到信号即停止 accept 并返回）。
///
/// 优雅关闭的意义：systemd `restart`/`stop` 会发 SIGTERM，此时应停止接受新连接
/// 并让在途会话自然结束，而不是直接被杀掉。
pub async fn run_service_until(
    config: ServiceConfig,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let tls_cfg = load_server_config(&config.cert_path, &config.key_path)?;
    let acceptor = TlsAcceptor::from(tls_cfg);

    // 确保重启脚本目录存在
    std::fs::create_dir_all(&config.scripts_dir).context("create scripts dir")?;

    let backup_keep = std::env::var("RDEP_BACKUP_KEEP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10);
    let storage = Arc::new(Storage::new(config.root_dir.clone(), backup_keep)?);
    let db = Arc::new(Db::open(&config.db_path)?);

    // 生效配置一览：排查「看到的目录不对」类问题时，第一行要看的就是 RDEP_ROOT
    tracing::info!(
        listen = %config.listen_addr,
        root = %storage.root_display(),
        db = %config.db_path.display(),
        scripts = %config.scripts_dir.display(),
        cert = %config.cert_path.display(),
        backup_keep,
        forwarder = config.use_forwarder,
        "rdep-service configuration resolved",
    );

    // 断点续传暂存目录的垃圾回收：定期清理超过 TTL 未活动的暂存（中断上传）。
    {
        let storage = storage.clone();
        let ttl = std::time::Duration::from_secs(
            config.staging_ttl_hours.saturating_mul(3600).max(60),
        );
        tokio::spawn(async move {
            // 启动后稍等再首次清理，随后按 TTL/2（至少 5 分钟）周期执行
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let period = (ttl / 2).max(std::time::Duration::from_secs(300));
            loop {
                match storage.cleanup_stale_staging(ttl) {
                    Ok(n) if n > 0 => {
                        tracing::info!("reclaimed {n} stale staging dir(s)")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("staging cleanup failed: {e:#}"),
                }
                tokio::time::sleep(period).await;
            }
        });
    }

    let listener = TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("rdep-service listening on {}", config.listen_addr);

    // Web 管理后台（可选）
    if let Some(web_addr) = config.web_listen.clone() {
        let web_state = crate::web::WebState::new(db.clone(), storage.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::web::serve(web_state, &web_addr).await {
                tracing::warn!("web server ended: {e:#}");
            }
        });
    }

    // 中转模式：另起一个长连接向 forwarder 注册（不占用本监听端口）
    if config.use_forwarder {
        let reg_cfg = config.clone();
        let reg_storage = storage.clone();
        let reg_db = db.clone();
        let reg_scripts = config.scripts_dir.clone();
        tokio::spawn(async move {
            crate::registry::register_loop(reg_cfg, reg_storage, reg_db, reg_scripts).await;
        });
    }

    loop {
        // 关闭信号与 accept 竞争：先收到哪个就走哪个
        let accepted = tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received, stopping accept loop");
                break;
            }
            res = listener.accept() => res,
        };
        let (sock, peer) = match accepted {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        let peer = peer.to_string();
        tracing::debug!(peer, "accepted tcp connection");
        let acceptor = acceptor.clone();
        let storage = storage.clone();
        let db = db.clone();
        let scripts_dir = config.scripts_dir.clone();
        tokio::spawn(async move {
            match acceptor.accept(sock).await {
                Ok(tls) => {
                    tracing::debug!(peer, "tls handshake ok");
                    if let Err(e) =
                        crate::session::handle(tls, storage, db, &scripts_dir, &peer).await
                    {
                        tracing::warn!(peer, "session ended: {e:#}");
                    }
                    tracing::debug!(peer, "session closed");
                }
                Err(e) => tracing::warn!(peer, "tls accept failed: {e}"),
            }
        });
    }
    Ok(())
}

/// 一直运行（不响应关闭信号）。生产用 `run_service_until` + 信号处理。
pub async fn run_service(config: ServiceConfig) -> Result<()> {
    let (_tx, rx) = tokio::sync::oneshot::channel();
    // 保持 sender 存活，使 rx 永不返回（等价于永不关闭）
    let _keep = Box::leak(Box::new(_tx));
    run_service_until(config, rx).await
}
