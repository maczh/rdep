//! 真实 sshd 的 SFTP 连接诊断（需本机 127.0.0.1:22 有 sshd）。
//! 运行：cargo test -p rdep-client --no-default-features --test sftp_live -- --ignored --nocapture
#![cfg(feature = "gui")]
use rdep_client::client::Event;
use rdep_client::sftp::{SftpClient, SftpParams};
use std::time::Duration;

fn drain(c: &SftpClient, secs: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match c.recv_timeout(Duration::from_millis(200)) {
            Some(ev) => {
                println!("event: {ev:?}");
                out.push(ev);
            }
            None => {}
        }
    }
    out
}

#[test]
#[ignore]
fn sftp_live_connect_localhost() {
    let c = SftpClient::new();
    c.connect(SftpParams {
        host: "127.0.0.1".into(),
        port: 22,
        user: std::env::var("LIVE_SFTP_USER").unwrap_or_else(|_| "admin".into()),
        pass: std::env::var("LIVE_SFTP_PASS").unwrap_or_else(|_| "wrongpass".into()),
        initial_dir: String::new(),
    });
    let evs = drain(&c, 30);
    assert!(
        !evs.is_empty(),
        "30s 内未收到任何事件——连接静默挂起（这就是 GUI 无响应的复现）"
    );
    assert!(evs.iter().any(|e| matches!(e, Event::Connected | Event::Error(_))));

    // 连通后顺带验证 chmod（SFTP setstat）往返：发出 OpDone 即代表协议链路通
    if evs.iter().any(|e| matches!(e, Event::Connected)) {
        c.chmod("/tmp/rdep-chmod-probe".into(), 0o600);
        let chmod_evs = drain(&c, 5);
        assert!(
            chmod_evs.iter().any(|e| matches!(e, Event::OpDone { .. })),
            "chmod 后未收到 OpDone 事件（setstat 链路未通）"
        );
    }
}
