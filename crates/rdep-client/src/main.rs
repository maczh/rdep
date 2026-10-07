//! rdep 客户端桌面入口（egui/eframe）。需 `gui` feature（默认开启）。

fn main() -> eframe::Result<()> {
    // 全局 debug 日志：stderr + 日志文件（路径打印到 stderr，便于用户反馈问题时定位）
    let log_path = rdep_client::logging::init();
    eprintln!("rdep-client logging to {}", log_path.display());
    tracing::debug!("rdep-client starting, log file = {}", log_path.display());

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "rdep 客户端",
        options,
        Box::new(|cc| Ok(Box::new(rdep_client::RdepApp::new(cc)))),
    )
}
