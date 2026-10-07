use rdep_service::{run_service_until, ServiceConfig};
use tracing_subscriber::EnvFilter;

/// 日志级别：`RUST_LOG` 优先，缺省 `debug`
/// —— 按运维要求，每条指令的入参/返回值/故障点都留痕。
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn main() -> anyhow::Result<()> {
    init_logging();
    let config = ServiceConfig::load()?;
    tracing::info!(
        listen = %config.listen_addr,
        root = %config.root_dir.display(),
        "starting rdep-service",
    );
    // 显式构建 runtime 而非 `#[tokio::main]`：口令散列走 `spawn_blocking`，
    // 而 tokio 默认允许 512 个阻塞线程，认证洪泛时会无界增长。
    // 这里显式封顶，把资源占用变成可预期的常数。
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
        run_service_until(config, rx).await
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
