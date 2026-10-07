//! rdep-forwarder 可执行入口。

use anyhow::Result;
use rdep_forwarder::{run_forwarder_until, ForwarderConfig};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cfg = ForwarderConfig::load()?;
    tracing::info!("starting rdep-forwarder");
    // 与 service 一致：显式封顶阻塞线程数（Web 登录同样走 PBKDF2 散列）。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(16)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            if wait_for_signal().await {
                tracing::info!("received termination signal, shutting down gracefully");
                let _ = tx.send(());
            }
        });
        run_forwarder_until(cfg, rx).await
    })
}

/// 等待 SIGTERM / SIGINT（systemd stop、Ctrl-C）。
#[cfg(unix)]
async fn wait_for_signal() -> bool {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("cannot listen SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return true;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    true
}

#[cfg(not(unix))]
async fn wait_for_signal() -> bool {
    let _ = tokio::signal::ctrl_c().await;
    true
}
