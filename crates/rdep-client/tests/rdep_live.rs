//! 真实 rdep-service 的客户端连接诊断（需 127.0.0.1:9443 有 service，admin/admin）。
//! 运行：cargo test -p rdep-client --test rdep_live -- --ignored --nocapture
use rdep_client::client::{Client, ConnectParams, Event};
use std::path::PathBuf;
use std::time::Duration;

#[test]
#[ignore]
fn rdep_live_connect_localhost_9443() {
    // 可用环境变量覆盖目标，便于对真实部署做诊断：
    // RDEP_LIVE_HOST / RDEP_LIVE_PORT / RDEP_LIVE_CA / RDEP_LIVE_USER / RDEP_LIVE_PASS
    let ca = std::env::var("RDEP_LIVE_CA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rdep-service/certs/server.crt")
        });
    let host = std::env::var("RDEP_LIVE_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("RDEP_LIVE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9443);
    let user = std::env::var("RDEP_LIVE_USER").unwrap_or_else(|_| "admin".into());
    let pass = std::env::var("RDEP_LIVE_PASS").unwrap_or_else(|_| "admin".into());
    println!("connecting to {host}:{port} user={user} pass_len={}", pass.len());
    let c = Client::new();
    c.connect(ConnectParams {
        host,
        port,
        user,
        pass,
        ca_cert: Some(ca),
        use_forwarder: false,
        target_service_id: String::new(),
        relay_token: String::new(),
        use_token: false,
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    while std::time::Instant::now() < deadline {
        match c.recv_timeout(Duration::from_millis(300)) {
            Some(ev) => {
                println!("event: {ev:?}");
                events.push(ev);
                if matches!(events.last(), Some(Event::Connected)) {
                    // 可选：上传一个本地文件（RDEP_LIVE_UPLOAD=<本地文件>），
                    // 目标路径用 RDEP_LIVE_UPLOAD_TO 指定（默认 /tmp/rdep-live-upload.bin）。
                    // 用途：验证「部署根不可写（如 RDEP_ROOT=/ 且 service 非 root）」
                    // 时上传是否成功——暂存区必须落在 service 私有 meta 目录而非部署根。
                    if let Ok(local) = std::env::var("RDEP_LIVE_UPLOAD") {
                        let remote = std::env::var("RDEP_LIVE_UPLOAD_TO")
                            .unwrap_or_else(|_| "/tmp/rdep-live-upload.bin".into());
                        c.upload(remote.clone(), local.clone());
                        let deadline3 = std::time::Instant::now() + Duration::from_secs(20);
                        while std::time::Instant::now() < deadline3 {
                            if let Some(ev) = c.recv_timeout(Duration::from_millis(300)) {
                                println!("upload event: {ev:?}");
                                match ev {
                                    Event::TransferDone { ok: true, .. } => {
                                        println!("UPLOAD OK: {local} -> {remote}");
                                        break;
                                    }
                                    Event::TransferDone { ok: false, message, .. } => {
                                        panic!("UPLOAD FAILED: {message}");
                                    }
                                    Event::Error(e) => panic!("UPLOAD ERROR: {e}"),
                                    _ => {}
                                }
                            }
                        }
                    }
                    // 可选：连上后列目录，用于验证真实文件系统浏览（RDEP_LIVE_LS=/path）
                    if let Ok(dir) = std::env::var("RDEP_LIVE_LS") {
                        c.ls(&dir);
                        let deadline2 = std::time::Instant::now() + Duration::from_secs(15);
                        while std::time::Instant::now() < deadline2 {
                            if let Some(Event::DirListed { path, entries }) =
                                c.recv_timeout(Duration::from_millis(300))
                            {
                                println!(
                                    "ls {path}: {} entries",
                                    entries.len()
                                );
                                for e in entries.iter().take(40) {
                                    println!(
                                        "  {} {} {}",
                                        if e.is_dir { "d" } else { "f" },
                                        e.size,
                                        e.name
                                    );
                                }
                                return; // 列目录成功
                            }
                        }
                        panic!("ls {dir} 15s 内未收到 DirListed");
                    }
                    return; // 全链路 OK
                }
                if matches!(events.last(), Some(Event::Disconnected)) {
                    break;
                }
            }
            None => {}
        }
    }
    panic!(
        "30s 内未连上 rdep-service；收到事件: {:?}",
        events
    );
}
