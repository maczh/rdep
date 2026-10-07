//! rdep-client 无界面集成测试（Phase 2/3）
//!
//! 每个用例启动一个真实的 rdep-service 实例（TLS + SQLite），用客户端 `Client` 走完整链路。
//! 不依赖任何 GUI/显示环境。
//!
//! 运行方式（避免引入 egui 原生后端）：
//! `cargo test -p rdep-client --no-default-features --test integration`

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdep_client::{Client, ConnectParams, Event, PublishFile};
use rdep_service::{run_service_until, ServiceConfig};
// 断点续传测试需要直接操作协议（原始 TLS 会话）
use rdep_protocol::{
    AuthMethod, AuthRequest, CmdRequest, CmdResponse, CmdType, DataChunk, DownloadRequest,
    Frame, FrameFlags, FrameType, LsRequest, MkdirRequest, NamePolicy, PublishCommitRequest,
    UploadCommit, UploadInit, UploadInitAck, sha256, split_file,
};
use rdep_protocol::FrameCodec;
use tokio::io::{AsyncRead, AsyncWrite};

fn certs_dir() -> PathBuf {
    // 集成测试 crate 的 CARGO_MANIFEST_DIR 是 rdep-client，证书在 rdep-service/certs。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../rdep-service/certs")
}

fn wait_for<F>(client: &Client, timeout: Duration, pred: F) -> Option<Event>
where
    F: Fn(&Event) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(ev) = client.recv_timeout(Duration::from_millis(250)) {
            if pred(&ev) {
                return Some(ev);
            }
        }
    }
    None
}

/// 等待某个端口真正开始监听（而不是固定 sleep）。
///
/// 固定 sleep 是脆弱的：service 启动时要先打开 SQLite 并为默认账户派生
/// 口令散列（PBKDF2 是刻意慢的），耗时随构建模式差异巨大
/// （release ~0.1s，debug 可达数秒）。用「实际探测端口」替代固定等待。
fn wait_port_ready(port: u16, within: Duration) {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().expect("addr"),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("port {port} not ready within {within:?}");
}

/// 测试持有的 service 生命周期守卫：Drop 时发关闭信号，让 service 线程真正退出。
///
/// 此前 helper 启动 service 后**永不关闭**，其 runtime / 线程 / 内存在整个测试进程
/// 生命周期内累积——用例数一多就会 `memory allocation failed`（SIGABRT）。
/// 这不只是测试问题：生产上 systemd `stop`/`restart` 发 SIGTERM 时同样需要优雅关闭。
#[must_use = "guard 未绑定会在语句结束时立即 drop，导致服务被立刻关闭"]
struct ServiceGuard {
    tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}

/// 启动一个隔离的 rdep-service（独立端口/根目录/数据库/脚本目录），等待监听就绪。
/// 返回守卫，**必须**用 `let _svc = ...` 持有到用例结束。
/// `web_port` 为 `Some` 时同时启动 Web 管理后台。
fn start_service(
    port: u16,
    web_port: Option<u16>,
    root: &Path,
    db: &Path,
    scripts: &Path,
) -> ServiceGuard {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).expect("create root");
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_dir_all(scripts);
    std::fs::create_dir_all(scripts).expect("create scripts");

    let config = ServiceConfig {
        listen_addr: format!("127.0.0.1:{port}"),
        root_dir: root.to_path_buf(),
        cert_path: certs_dir().join("server.crt"),
        key_path: certs_dir().join("server.key"),
        db_path: db.to_path_buf(),
        scripts_dir: scripts.to_path_buf(),
        meta_dir: db.with_extension("meta"),
        web_listen: web_port.map(|p| format!("127.0.0.1:{p}")),
        use_forwarder: false,
        forwarder_host: String::new(),
        forwarder_port: 9444,
        forwarder_ca: certs_dir().join("server.crt"),
        relay_token: "rdep-relay-token".into(),
        service_id: String::new(),
        service_label: String::new(),
        max_sessions: 4,
        staging_ttl_hours: 24,
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("service runtime");
        rt.block_on(async move {
            let _ = run_service_until(config, rx).await;
        });
    });
    wait_port_ready(port, Duration::from_secs(30));
    ServiceGuard { tx: Some(tx) }
}

fn connect(client: &Client, port: u16) {
    client.connect(ConnectParams {
        host: "localhost".into(),
        port,
        user: "admin".into(),
        pass: "admin".into(),
        ca_cert: Some(certs_dir().join("server.crt")),
        use_forwarder: false,
        target_service_id: String::new(),
        relay_token: "rdep-relay-token".into(),
            use_token: false,
    });
}

#[test]
fn client_to_service_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 18444u16;
    let root = cwd.join("target/it-root");
    let db = cwd.join("target/it.db");
    let scripts = cwd.join("target/it-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    let client = Client::new();
    connect(&client, port);
    // AUTH 成功
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::Connected)
    })
    .expect("should receive Connected");

    // MKDIR 多级
    client.mkdir(vec!["/a/b/c".into()]);
    let ev = wait_for(&client, Duration::from_secs(10), |e| match e {
        Event::OpDone { ok, .. } => *ok,
        _ => false,
    })
    .expect("mkdir op done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));

    // LS 根目录应包含 a/
    client.ls("/");
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::DirListed { .. })
    })
    .expect("dir listed");
    if let Event::DirListed { entries, .. } = ev {
        assert!(
            entries.iter().any(|x| x.name == "a" && x.is_dir),
            "root missing a/: {:?}",
            entries
        );
    }

    // 准备本地源文件
    let src = cwd.join("target/it-upload.bin");
    let payload: Vec<u8> = (0..4096u32).map(|x| (x % 251) as u8).collect();
    std::fs::write(&src, &payload).unwrap();
    let remote_path = "/a/b/c/hello.bin".to_string();

    // UPLOAD
    client.upload(remote_path.clone(), src.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("upload done");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));

    // DOWNLOAD 并比对内容
    let dst = cwd.join("target/it-download.bin");
    let _ = std::fs::remove_file(&dst);
    client.download(remote_path.clone(), dst.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("download done");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));

    let got = std::fs::read(&dst).expect("read downloaded file");
    assert_eq!(got, payload, "downloaded content mismatch");

    // DELETE
    client.delete(vec![remote_path.clone()]);
    let ev = wait_for(&client, Duration::from_secs(10), |e| match e {
        Event::OpDone { ok, .. } => *ok,
        _ => false,
    })
    .expect("delete done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));

    // 删除后 LS 该目录不应再含 hello.bin
    client.ls("/a/b/c");
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::DirListed { .. })
    })
    .expect("dir listed after delete");
    if let Event::DirListed { entries, .. } = ev {
        assert!(
            !entries.iter().any(|x| x.name == "hello.bin"),
            "hello.bin should be deleted: {:?}",
            entries
        );
    }

    // 断开
    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    println!("INTEGRATION TEST PASSED");
}

#[test]
fn publish_and_rollback_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 18445u16;
    let root = cwd.join("target/it-pub-root");
    let db = cwd.join("target/it-pub.db");
    let scripts = cwd.join("target/it-pub-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);
    // 放一个「成功」的重启脚本（服务会在发布收尾时用 sh 执行它）
    std::fs::write(scripts.join("restart"), "#!/bin/sh\nexit 0\n").unwrap();

    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::Connected)
    })
    .expect("connected");

    // 建远端目录
    client.mkdir(vec!["/a/b".into()]);
    wait_for(&client, Duration::from_secs(10), |e| match e {
        Event::OpDone { ok, .. } => *ok,
        _ => false,
    })
    .expect("mkdir");

    // 先写入 v0（普通上传）
    let v0 = b"version-zero-content".to_vec();
    let src0 = cwd.join("target/it-pub-v0.bin");
    std::fs::write(&src0, &v0).unwrap();
    client.upload("/a/b/hello.txt".into(), src0.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("upload v0");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));

    // 发布 v1（覆盖 /a/b/hello.txt，先备份 v0，收尾执行 restart 脚本）
    let v1 = b"version-one-content-new".to_vec();
    let src1 = cwd.join("target/it-pub-v1.bin");
    std::fs::write(&src1, &v1).unwrap();
    client.publish(
        "/a/b".into(),
        "restart".into(),
        vec![PublishFile {
            remote_path: "/a/b/hello.txt".into(),
            local_path: src1.to_string_lossy().to_string(),
        }],
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::PublishDone { .. })
    })
    .expect("publish done");
    assert!(
        matches!(ev, Event::PublishDone { ok: true, .. }),
        "publish should succeed (restart script exit 0)"
    );

    // 验证远端已是 v1
    let dl1 = cwd.join("target/it-pub-dl1.bin");
    let _ = std::fs::remove_file(&dl1);
    client.download("/a/b/hello.txt".into(), dl1.to_string_lossy().to_string());
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("download v1");
    assert_eq!(std::fs::read(&dl1).unwrap(), v1, "after publish should be v1");

    // 列出备份版本（备份库在 service 私有工作目录 meta 下，走专用 Backups 指令）
    client.list_backups();
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::BackupVersions { .. })
    })
    .expect("backup listed");
    let version = match ev {
        Event::BackupVersions { versions } => versions.first().cloned(),
        _ => None,
    }
    .expect("should have a backup version");

    // 回滚到该版本（应把 v0 恢复回来）
    client.rollback("/a/b".into(), version.clone());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("rollback done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));

    // 验证远端已恢复为 v0
    let dl2 = cwd.join("target/it-pub-dl2.bin");
    let _ = std::fs::remove_file(&dl2);
    client.download("/a/b/hello.txt".into(), dl2.to_string_lossy().to_string());
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("download v0");
    assert_eq!(
        std::fs::read(&dl2).unwrap(),
        v0,
        "after rollback should be v0"
    );

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src0);
    let _ = std::fs::remove_file(&src1);
    let _ = std::fs::remove_file(&dl1);
    let _ = std::fs::remove_file(&dl2);

    println!("PUBLISH/ROLLBACK TEST PASSED");
}

#[test]
fn tail_grep_edit_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 18446u16;
    let root = cwd.join("target/it-log-root");
    let db = cwd.join("target/it-log.db");
    let scripts = cwd.join("target/it-log-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::Connected)
    })
    .expect("connected");

    // 建 /logs 并上传一个 5 行日志文件
    client.mkdir(vec!["/logs".into()]);
    wait_for(&client, Duration::from_secs(10), |e| match e {
        Event::OpDone { ok, .. } => *ok,
        _ => false,
    })
    .expect("mkdir");

    let log = "line1\nline2\nERROR something bad\nline4\nline5\n".to_string();
    let src = cwd.join("target/it-log-src.txt");
    std::fs::write(&src, &log).unwrap();
    client.upload("/logs/app.log".into(), src.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("upload log");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));

    // ---- TAIL（非跟随，取最后 3 行）----
    client.tail("/logs/app.log".into(), 3, false);
    let mut tail_lines: Vec<String> = Vec::new();
    let start = Instant::now();
    let mut tail_done = false;
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(ev) = client.recv_timeout(Duration::from_millis(200)) {
            match ev {
                Event::TailLine { line } => tail_lines.push(line),
                Event::TailDone { .. } => {
                    tail_done = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(tail_done, "non-follow tail should finish");
    assert_eq!(tail_lines.len(), 3, "expected last 3 lines: {:?}", tail_lines);
    assert_eq!(tail_lines[2], "line5", "last line should be line5");
    assert!(tail_lines[0].contains("ERROR"), "3rd-from-last is the ERROR line: {:?}", tail_lines);

    // ---- GREP（目录 + 行号）----
    client.grep("/logs".into(), "ERROR".into(), "n".into());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::GrepResult { .. })
    })
    .expect("grep result");
    let hits = match ev {
        Event::GrepResult { lines } => lines,
        _ => Vec::new(),
    };
    assert!(
        hits.iter().any(|l| l.contains("ERROR something bad")),
        "grep should find the ERROR line: {:?}",
        hits
    );
    assert!(
        hits[0].contains(":3:"),
        "grep -n should include line number 3: {:?}",
        hits
    );

    // ---- EDIT 读取 ----
    client.edit_get("/logs/app.log".into());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::EditLoaded { .. })
    })
    .expect("edit loaded");
    let content = match ev {
        Event::EditLoaded { content } => content,
        _ => String::new(),
    };
    assert!(
        content.contains("ERROR something bad"),
        "loaded content should contain original line"
    );

    // ---- EDIT 保存（先备份再覆盖）----
    let new_content = format!("{}appended-by-edit\n", content);
    client.edit_save("/logs/app.log".into(), new_content);
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("edit save done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));

    // 重新读取确认持久化
    client.edit_get("/logs/app.log".into());
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::EditLoaded { .. })
    })
    .expect("reload after save");
    let content2 = match ev {
        Event::EditLoaded { content } => content,
        _ => String::new(),
    };
    assert!(
        content2.contains("appended-by-edit"),
        "saved content should persist"
    );

    // ---- TAIL 跟随 + 主动停止（并验证会话仍可继续用）----
    client.tail("/logs/app.log".into(), 2, true);
    let start = Instant::now();
    let mut got = 0usize;
    let mut stopped = false;
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(ev) = client.recv_timeout(Duration::from_millis(200)) {
            match ev {
                Event::TailLine { .. } => {
                    got += 1;
                    if got >= 2 {
                        client.request_stop_tail();
                    }
                }
                Event::TailDone { ok, .. } => {
                    assert!(ok, "follow tail should stop cleanly");
                    stopped = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(stopped, "follow tail should stop on request");

    // 停止后会话必须仍然同步（没有残留帧）
    client.ls("/logs");
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::DirListed { .. })
    })
    .expect("ls after tail stop (session in sync)");
    if let Event::DirListed { entries, .. } = ev {
        assert!(entries.iter().any(|e| e.name == "app.log"));
    }

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src);

    println!("TAIL/GREP/EDIT TEST PASSED");
}

// ===========================================================================
// Phase 5：经 forwarder 中转的端到端验证
// ===========================================================================

fn start_forwarder(client_port: u16, service_port: u16, db: &Path) -> ServiceGuard {
    use rdep_forwarder::ForwarderConfig;
    let _ = std::fs::remove_file(db);
    let cfg = ForwarderConfig {
        service_listen: format!("127.0.0.1:{service_port}"),
        client_listen: format!("127.0.0.1:{client_port}"),
        cert_path: certs_dir().join("server.crt"),
        key_path: certs_dir().join("server.key"),
        db_path: db.to_path_buf(),
        relay_token: "rdep-relay-token".into(),
        web_listen: None,
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("forwarder runtime");
        rt.block_on(async move {
            let _ = rdep_forwarder::run_forwarder_until(cfg, rx).await;
        });
    });
    // forwarder 同样要先建库（口令散列）才能监听，等真实就绪而非固定 sleep
    wait_port_ready(client_port, Duration::from_secs(30));
    ServiceGuard { tx: Some(tx) }
}

/// 启动一个「已注册到 forwarder」的 service（也开一个直连端口，但本测试走中转）。
fn start_registered_service(
    listen_port: u16,
    root: &Path,
    db: &Path,
    scripts: &Path,
    fwd_service_port: u16,
    service_id: &str,
) -> ServiceGuard {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).expect("create root");
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_dir_all(scripts);
    std::fs::create_dir_all(scripts).expect("create scripts");
    // 发布用的重启脚本
    std::fs::write(scripts.join("restart"), "#!/bin/sh\nexit 0\n").unwrap();

    let config = ServiceConfig {
        listen_addr: format!("127.0.0.1:{listen_port}"),
        root_dir: root.to_path_buf(),
        cert_path: certs_dir().join("server.crt"),
        key_path: certs_dir().join("server.key"),
        db_path: db.to_path_buf(),
        scripts_dir: scripts.to_path_buf(),
        meta_dir: db.with_extension("meta"),
        web_listen: None,
        use_forwarder: true,
        forwarder_host: "localhost".into(),
        forwarder_port: fwd_service_port,
        forwarder_ca: certs_dir().join("server.crt"),
        relay_token: "rdep-relay-token".into(),
        service_id: service_id.into(),
        service_label: "relay-test-svc".into(),
        max_sessions: 4,
        staging_ttl_hours: 24,
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("service runtime");
        rt.block_on(async move {
            let _ = run_service_until(config, rx).await;
        });
    });
    // 等 service 监听就绪并完成向 forwarder 注册（真实探测而非固定等待）
    wait_port_ready(listen_port, Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(500));
    ServiceGuard { tx: Some(tx) }
}

#[test]
fn relay_via_forwarder_publish_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let fwd_client_port = 19543u16;
    let fwd_service_port = 19544u16;
    let svc_listen = 19545u16;
    let service_id = "relay-svc-1";

    let fwd_db = cwd.join("target/it-fwd.db");
    let root = cwd.join("target/it-fwd-root");
    let db = cwd.join("target/it-fwd-svc.db");
    let scripts = cwd.join("target/it-fwd-scripts");

    // 先起 forwarder，再起注册到它的 service
    let _fwd = start_forwarder(fwd_client_port, fwd_service_port, &fwd_db);
    let _svc2 = start_registered_service(
        svc_listen,
        &root,
        &db,
        &scripts,
        fwd_service_port,
        service_id,
    );

    // client 连 forwarder（use_forwarder），由 forwarder 路由到目标 service
    let client = Client::new();
    client.connect(ConnectParams {
        host: "localhost".into(),
        port: fwd_client_port,
        user: "admin".into(),
        pass: "admin".into(),
        ca_cert: Some(certs_dir().join("server.crt")),
        use_forwarder: true,
        target_service_id: service_id.into(),
        relay_token: "rdep-relay-token".into(),
            use_token: false,
    });

    // 经中转建立 rdep 会话（forwarder 配对成功 → client 收到 Connected）
    wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::Connected)
    })
    .expect("connected via forwarder");

    // 经中转执行一次完整发布（Phase 5 验收：client 经 forwarder 连内网 service 完成发布）
    client.mkdir(vec!["/pub".into()]);
    let ev = wait_for(&client, Duration::from_secs(15), |e| match e {
        Event::OpDone { ok, .. } => *ok,
        _ => false,
    })
    .expect("mkdir via relay");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));

    let v0 = b"relay-v0".to_vec();
    let src0 = cwd.join("target/it-fwd-v0.bin");
    std::fs::write(&src0, &v0).unwrap();
    client.upload("/pub/hello.bin".into(), src0.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("upload v0 via relay");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));

    // 发布 v1（经中转：备份 v0 + 重启脚本）
    let v1 = b"relay-v1-new".to_vec();
    let src1 = cwd.join("target/it-fwd-v1.bin");
    std::fs::write(&src1, &v1).unwrap();
    client.publish(
        "/pub".into(),
        "restart".into(),
        vec![PublishFile {
            remote_path: "/pub/hello.bin".into(),
            local_path: src1.to_string_lossy().to_string(),
        }],
    );
    let ev = wait_for(&client, Duration::from_secs(20), |e| {
        matches!(e, Event::PublishDone { .. })
    })
    .expect("publish via relay");
    assert!(
        matches!(ev, Event::PublishDone { ok: true, .. }),
        "publish via forwarder should succeed"
    );

    // 验证经中转下载回来的内容是 v1
    let dl = cwd.join("target/it-fwd-dl.bin");
    let _ = std::fs::remove_file(&dl);
    client.download("/pub/hello.bin".into(), dl.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("download via relay");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));
    assert_eq!(
        std::fs::read(&dl).unwrap(),
        v1,
        "relayed download should be v1"
    );

    client.disconnect();
    wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&fwd_db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src0);
    let _ = std::fs::remove_file(&src1);
    let _ = std::fs::remove_file(&dl);

    println!("RELAY VIA FORWARDER TEST PASSED");
}

// ===========================================================================
// Phase 6：Web 管理后台端到端验证
// ===========================================================================

/// 极简 HTTP/1.1 客户端（单请求-响应，Connection: close），避免引入 reqwest。
/// 返回 `(状态码, 响应体)`。
fn http_request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("http connect");
    let mut req = format!("{} {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n", method, path);
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    match body {
        Some(b) => {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            req.push_str(b);
        }
        None => req.push_str("\r\n"),
    }
    s.write_all(req.as_bytes()).expect("http write");
    let mut buf = String::new();
    s.read_to_string(&mut buf).expect("http read");
    let status = buf
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// 从 `{"token":"..."}` 中抠出 token。
fn extract_token(body: &str) -> String {
    let key = "\"token\":\"";
    let start = body.find(key).map(|i| i + key.len()).expect("token field");
    let rest = &body[start..];
    let end = rest.find('"').expect("token end");
    rest[..end].to_string()
}

#[test]
fn service_web_admin_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 19643u16;
    let web_port = 19644u16;
    let root = cwd.join("target/it-web-root");
    let db = cwd.join("target/it-web.db");
    let scripts = cwd.join("target/it-web-scripts");
    let _svc = start_service(port, Some(web_port), &root, &db, &scripts);

    // 未带 token 访问受保护接口 → 401
    let (st, _) = http_request(web_port, "GET", "/api/status", None, None);
    assert_eq!(st, 401, "unauthenticated /api/status should be 401");

    // 错误密码 → 401
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/login",
        None,
        Some(r#"{"username":"admin","password":"wrong"}"#),
    );
    assert_eq!(st, 401, "bad password should be 401");

    // admin/admin 登录成功
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/login",
        None,
        Some(r#"{"username":"admin","password":"admin"}"#),
    );
    assert_eq!(st, 200, "admin/admin login should be 200: {}", body);
    let token = extract_token(&body);
    assert!(!token.is_empty());

    // 首页可访问
    let (st, page) = http_request(web_port, "GET", "/", None, None);
    assert_eq!(st, 200);
    assert!(page.contains("rdep-service"), "index html should contain title");

    // 概览
    let (st, body) = http_request(web_port, "GET", "/api/status", Some(&token), None);
    assert_eq!(st, 200, "status: {}", body);
    assert!(body.contains("\"users\"") && body.contains("\"projects\""));

    // 用户 CRUD
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/users",
        Some(&token),
        Some(r#"{"username":"deployer","password":"pw12345"}"#),
    );
    assert_eq!(st, 200, "create user");
    let (st, body) = http_request(web_port, "GET", "/api/users", Some(&token), None);
    assert_eq!(st, 200);
    assert!(body.contains("deployer"), "user list should contain deployer: {}", body);
    // 删除刚建的用户（从列表里拿 id）
    let uid = body
        .split("deployer")
        .next()
        .and_then(|s| s.rfind("\"id\":"))
        .map(|i| {
            let r = &body[i + 5..];
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            r[..end].to_string()
        })
        .expect("user id");
    let (st, _) = http_request(web_port, "DELETE", &format!("/api/users/{}", uid), Some(&token), None);
    assert_eq!(st, 200, "delete user");

    // 项目 CRUD
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/projects",
        Some(&token),
        Some(r#"{"name":"web","remote_dir":"/opt/app","restart_script":"restart"}"#),
    );
    assert_eq!(st, 200, "create project");
    let (st, body) = http_request(web_port, "GET", "/api/projects", Some(&token), None);
    assert_eq!(st, 200);
    assert!(body.contains("/opt/app"), "project list: {}", body);

    // 备份列表（此时应为空数组）
    let (st, body) = http_request(web_port, "GET", "/api/backups", Some(&token), None);
    assert_eq!(st, 200);
    assert!(body.contains("versions"), "backups: {}", body);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);

    println!("SERVICE WEB TEST PASSED");
}

/// 启动一个带 Web 的 forwarder（供 forwarder web 测试）。
fn start_forwarder_with_web(
    client_port: u16,
    service_port: u16,
    web_port: u16,
    db: &Path,
) -> ServiceGuard {
    use rdep_forwarder::ForwarderConfig;
    let _ = std::fs::remove_file(db);
    let cfg = ForwarderConfig {
        service_listen: format!("127.0.0.1:{service_port}"),
        client_listen: format!("127.0.0.1:{client_port}"),
        cert_path: certs_dir().join("server.crt"),
        key_path: certs_dir().join("server.key"),
        db_path: db.to_path_buf(),
        relay_token: "rdep-relay-token".into(),
        web_listen: Some(format!("127.0.0.1:{web_port}")),
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("forwarder runtime");
        rt.block_on(async move {
            let _ = rdep_forwarder::run_forwarder_until(cfg, rx).await;
        });
    });
    // forwarder 同样要先建库（口令散列）才能监听，等真实就绪而非固定 sleep
    wait_port_ready(client_port, Duration::from_secs(30));
    ServiceGuard { tx: Some(tx) }
}

#[test]
fn forwarder_web_services_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let fwd_client = 19743u16;
    let fwd_service = 19744u16;
    let fwd_web = 19745u16;
    let fwd_db = cwd.join("target/it-fwdweb.db");
    let root = cwd.join("target/it-fwdweb-root");
    let db = cwd.join("target/it-fwdweb-svc.db");
    let scripts = cwd.join("target/it-fwdweb-scripts");

    // forwarder（带 web）+ 注册到它的 service
    let _fwd = start_forwarder_with_web(fwd_client, fwd_service, fwd_web, &fwd_db);
    let _svc2 = start_registered_service(19845, &root, &db, &scripts, fwd_service, "web-relay-svc");

    // 未认证 → 401
    let (st, _) = http_request(fwd_web, "GET", "/api/services", None, None);
    assert_eq!(st, 401, "unauthenticated forwarder /api/services should be 401");

    // 登录
    let (st, body) = http_request(
        fwd_web,
        "POST",
        "/api/login",
        None,
        Some(r#"{"username":"admin","password":"admin"}"#),
    );
    assert_eq!(st, 200, "forwarder admin/admin login: {}", body);
    let token = extract_token(&body);

    // service 列表：应包含已注册且在线的 service
    let (st, body) = http_request(fwd_web, "GET", "/api/services", Some(&token), None);
    assert_eq!(st, 200, "forwarder services: {}", body);
    assert!(
        body.contains("web-relay-svc"),
        "services list should contain registered service: {}",
        body
    );
    assert!(body.contains("\"online\":true"), "service should be online: {}", body);

    // ---- 中继审计：注册了但还没人用过 → 会话数应为 0 ----
    assert!(
        body.contains("\"client_sessions\":0"),
        "刚注册、尚无客户端会话时计数应为 0: {}",
        body
    );

    // ---- 让一个 client 真的经中转连一次，审计计数应 +1 ----
    let relay_client = Client::new();
    connect_via_fwd(&relay_client, fwd_client, "web-relay-svc");
    wait_for(&relay_client, Duration::from_secs(15), |e| {
        matches!(e, Event::Connected)
    })
    .expect("client connected through forwarder");
    relay_client.disconnect();
    wait_for(&relay_client, Duration::from_secs(10), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("client disconnected");

    // 计数写入是同步的，但留一点余量避免时序抖动
    std::thread::sleep(Duration::from_millis(300));
    let (st, body) = http_request(fwd_web, "GET", "/api/services", Some(&token), None);
    assert_eq!(st, 200);
    assert!(
        body.contains("\"client_sessions\":1"),
        "一次中转会话后审计计数应为 1（证明该字段真实生效，非装饰）: {}",
        body
    );
    assert!(
        !body.contains("\"last_client_at\":0"),
        "服务过客户端后 last_client_at 应被更新: {}",
        body
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&fwd_db);
    let _ = std::fs::remove_dir_all(&scripts);

    println!("FORWARDER WEB TEST PASSED");
}

// ===========================================================================
// 断点续传（resumable upload）端到端验证
// ===========================================================================

/// 建立一条已认证的原始 rdep TLS 会话（绕过高层 Client，直接操作协议）。
async fn raw_authed_session(port: u16) -> FrameCodec<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    use rustls::pki_types::ServerName;
    use rustls::RootCertStore;
    use std::sync::Arc;
    use tokio_rustls::TlsConnector;

    let pem = std::fs::read(certs_dir().join("server.crt")).expect("read server cert");
    let mut roots = RootCertStore::empty();
    let certs = rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("parse cert");
    for c in certs {
        roots.add(c).expect("add root");
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(roots))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(cfg));
    let name = ServerName::try_from("localhost").expect("server name");
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    let tls = connector.connect(name, tcp).await.expect("tls handshake");
    let mut codec = FrameCodec::new(tls);
    let resp = raw_cmd(
        &mut codec,
        1,
        CmdType::Auth,
        &AuthRequest {
            user: "admin".into(),
            pass: "admin".into(),
            method: AuthMethod::Password,
        },
    )
    .await;
    assert!(resp.ok, "raw session auth failed: {}", resp.message);
    codec
}

/// 发送一条控制指令并读取其响应。
async fn raw_cmd<S>(codec: &mut FrameCodec<S>, seq: u32, cmd: CmdType, body: &impl serde::Serialize) -> CmdResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = CmdRequest {
        seq,
        cmd,
        body: postcard::to_allocvec(body).expect("encode body"),
    };
    codec
        .write_frame(&Frame::new(
            FrameType::CmdRequest,
            FrameFlags::new(),
            req.encode().expect("encode req"),
        ))
        .await
        .expect("write frame");
    let f = codec.read_frame().await.expect("read frame").expect("eof");
    CmdResponse::decode(&f.payload).expect("decode resp")
}

#[test]
fn upload_resume_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 19943u16;
    let root = cwd.join("target/it-resume-root");
    let db = cwd.join("target/it-resume.db");
    let scripts = cwd.join("target/it-resume-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    // 3 片 × 16 字节的小文件
    let content: Vec<u8> = (0..48u32).map(|x| (x % 251) as u8).collect();
    let parts = split_file(&content, 16);
    assert_eq!(parts.len(), 3);
    let file_sha = sha256(&content);
    let transfer_id: u64 = 424242; // 固定 id，模拟「同一逻辑上传」的重试
    let remote = "/resumed.bin";

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");

    rt.block_on(async {
        // ---- 第一次连接：init + 只发 0/1 片，然后断开 ----
        let mut c1 = raw_authed_session(port).await;
        let init = UploadInit {
            transfer_id,
            remote_path: remote.into(),
            size: content.len() as u64,
            mtime: 0,
            chunk_size: 16,
            total_chunks: 3,
            file_sha256: file_sha,
            backup_first: false,
            mode: 0,
        };
        let ack = raw_cmd(&mut c1, 2, CmdType::Upload, &init).await;
        assert!(ack.ok, "init1: {}", ack.message);
        let a1: UploadInitAck = postcard::from_bytes(&ack.body).expect("ack1");
        assert!(a1.received.is_empty(), "fresh upload should have no received chunks");

        for i in 0..2usize {
            let dc = DataChunk::new(transfer_id, i as u32, parts[i].1.clone());
            c1.write_frame(&Frame::new(
                FrameType::DataChunk,
                FrameFlags::new(),
                postcard::to_allocvec(&dc).unwrap(),
            ))
            .await
            .expect("send chunk");
        }
        // 给服务端时间把 0/1 片落盘暂存
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(c1); // 模拟断线

        // ---- 第二次连接：同 id init → 服务端应报告已收 0/1；只补发 2 号片 ----
        let mut c2 = raw_authed_session(port).await;
        let ack2 = raw_cmd(&mut c2, 2, CmdType::Upload, &init).await;
        assert!(ack2.ok, "init2: {}", ack2.message);
        let a2: UploadInitAck = postcard::from_bytes(&ack2.body).expect("ack2");
        assert_eq!(
            a2.received,
            vec![0, 1],
            "resume: server should report chunks 0,1 already received"
        );

        // 只补发缺失的 2 号片
        let dc = DataChunk::new(transfer_id, 2, parts[2].1.clone());
        c2.write_frame(&Frame::new(
            FrameType::DataChunk,
            FrameFlags::new(),
            postcard::to_allocvec(&dc).unwrap(),
        ))
        .await
        .expect("send chunk 2");

        let resp = raw_cmd(&mut c2, 3, CmdType::Upload, &UploadCommit { transfer_id }).await;
        assert!(resp.ok, "commit: {}", resp.message);
    });

    // 校验落盘内容完整且与源一致（断点续传合并正确）
    let saved = std::fs::read(root.join("resumed.bin")).expect("read resumed file");
    assert_eq!(saved, content, "resumed file content mismatch");

    // 暂存区应在 commit 后被清理
    let staging = root.join(".rdep-staging").join(transfer_id.to_string());
    assert!(!staging.exists(), "staging dir should be cleaned after commit");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);

    println!("UPLOAD RESUME TEST PASSED");
}

// ===========================================================================
// 并发中转：同一 service 同时服务多个 client
// ===========================================================================

fn connect_via_fwd(client: &Client, fwd_port: u16, service_id: &str) {
    client.connect(ConnectParams {
        host: "localhost".into(),
        port: fwd_port,
        user: "admin".into(),
        pass: "admin".into(),
        ca_cert: Some(certs_dir().join("server.crt")),
        use_forwarder: true,
        target_service_id: service_id.into(),
        relay_token: "rdep-relay-token".into(),
            use_token: false,
    });
}

#[test]
fn relay_concurrent_clients_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let fwd_client = 19953u16;
    let fwd_service = 19954u16;
    let svc_listen = 19955u16;
    let service_id = "conc-svc";

    let fwd_db = cwd.join("target/it-conc-fwd.db");
    let root = cwd.join("target/it-conc-root");
    let db = cwd.join("target/it-conc-svc.db");
    let scripts = cwd.join("target/it-conc-scripts");

    let _fwd = start_forwarder(fwd_client, fwd_service, &fwd_db);
    let _svc2 = start_registered_service(svc_listen, &root, &db, &scripts, fwd_service, service_id);

    // 两个 client 同时（各自保持连接）连到同一个 service
    let c1 = Client::new();
    let c2 = Client::new();
    connect_via_fwd(&c1, fwd_client, service_id);
    connect_via_fwd(&c2, fwd_client, service_id);

    wait_for(&c1, Duration::from_secs(15), |e| matches!(e, Event::Connected))
        .expect("client1 connected via relay");
    wait_for(&c2, Duration::from_secs(15), |e| matches!(e, Event::Connected))
        .expect("client2 connected via relay (concurrency)");

    // 两个会话重叠，各自上传到不同文件
    let p1 = b"concurrent-file-1".to_vec();
    let p2 = b"concurrent-file-2".to_vec();
    let s1 = cwd.join("target/it-conc-1.bin");
    let s2 = cwd.join("target/it-conc-2.bin");
    std::fs::write(&s1, &p1).unwrap();
    std::fs::write(&s2, &p2).unwrap();
    c1.upload("/c1.bin".into(), s1.to_string_lossy().to_string());
    c2.upload("/c2.bin".into(), s2.to_string_lossy().to_string());

    let ev1 = wait_for(&c1, Duration::from_secs(15), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("c1 upload done");
    assert!(matches!(ev1, Event::TransferDone { ok: true, .. }), "c1 upload: {:?}", ev1);
    let ev2 = wait_for(&c2, Duration::from_secs(15), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("c2 upload done");
    assert!(matches!(ev2, Event::TransferDone { ok: true, .. }), "c2 upload: {:?}", ev2);

    // 两个文件都应落在服务端
    assert_eq!(std::fs::read(root.join("c1.bin")).unwrap(), p1, "c1 content");
    assert_eq!(std::fs::read(root.join("c2.bin")).unwrap(), p2, "c2 content");

    c1.disconnect();
    c2.disconnect();

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&fwd_db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&s1);
    let _ = std::fs::remove_file(&s2);

    println!("RELAY CONCURRENT CLIENTS TEST PASSED");
}

// ===========================================================================
// 目录同步（sync_dir）端到端验证
// ===========================================================================

#[test]
fn sync_dir_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 19963u16;
    let root = cwd.join("target/it-sync-root");
    let db = cwd.join("target/it-sync.db");
    let scripts = cwd.join("target/it-sync-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    // 本地目录：site/a.txt、site/sub/b.txt
    let local = cwd.join("target/it-sync-local");
    let _ = std::fs::remove_dir_all(&local);
    std::fs::create_dir_all(local.join("sub")).unwrap();
    std::fs::write(local.join("a.txt"), b"aaa").unwrap();
    std::fs::write(local.join("sub").join("b.txt"), b"bbb").unwrap();

    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Connected))
        .expect("connected");

    // ---- 第一次同步：两个文件都应上传 ----
    client.sync_dir(
        local.to_string_lossy().to_string(),
        "/sync".into(),
        false,
        false,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("sync1 done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }), "sync1: {:?}", ev);
    assert_eq!(std::fs::read(root.join("sync/a.txt")).unwrap(), b"aaa");
    assert_eq!(std::fs::read(root.join("sync/sub/b.txt")).unwrap(), b"bbb");

    // ---- 第二次 dry-run：内容未变，应报告 0 待传 ----
    client.sync_dir(
        local.to_string_lossy().to_string(),
        "/sync".into(),
        false,
        true,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("dry-run done");
    if let Event::OpDone { message, .. } = &ev {
        assert!(
            message.contains("0 to upload"),
            "unchanged files should not re-upload: {}",
            message
        );
    }

    // ---- 修改 a.txt（改大小，确保变更被检出）→ 只重传它 ----
    std::fs::write(local.join("a.txt"), b"aaaaaa-modified").unwrap();
    client.sync_dir(
        local.to_string_lossy().to_string(),
        "/sync".into(),
        false,
        true,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("dry-run2 done");
    if let Event::OpDone { message, .. } = &ev {
        assert!(
            message.contains("1 to upload"),
            "only modified file should be pending: {}",
            message
        );
    }

    // 真正同步
    client.sync_dir(
        local.to_string_lossy().to_string(),
        "/sync".into(),
        false,
        false,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("sync2 done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));
    assert_eq!(
        std::fs::read(root.join("sync/a.txt")).unwrap(),
        b"aaaaaa-modified",
        "modified file updated"
    );
    assert_eq!(std::fs::read(root.join("sync/sub/b.txt")).unwrap(), b"bbb");

    // ---- delete_extra：远端多余文件应被删除 ----
    std::fs::write(root.join("sync").join("extra.txt"), b"stale").unwrap();
    // 先 dry-run 预览：SyncPreview 应把 extra.txt 列入「将删除」
    client.sync_dir(
        local.to_string_lossy().to_string(),
        "/sync".into(),
        true,
        true,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::SyncPreview { .. })
    })
    .expect("sync preview");
    if let Event::SyncPreview { to_delete, .. } = &ev {
        assert!(
            to_delete.iter().any(|d| d == "extra.txt"),
            "preview should list extra.txt for deletion: {:?}",
            to_delete
        );
    }
    // 消费 dry-run 自身的完成事件，避免与后续真实同步的 OpDone 混淆
    wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("dry-run done");
    // 真正同步（delete_extra）
    client.sync_dir(
        local.to_string_lossy().to_string(),
        "/sync".into(),
        true,
        false,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("sync3 done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }));
    assert!(
        !root.join("sync/extra.txt").exists(),
        "extra remote file should be deleted"
    );
    assert!(root.join("sync/a.txt").exists(), "keep local files");

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_dir_all(&local);

    println!("SYNC DIR TEST PASSED");
}

// ===========================================================================
// 下载流式落盘 + 完整性/原子性验证
// ===========================================================================

#[test]
fn download_stream_integrity_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 19973u16;
    let root = cwd.join("target/it-dl-root");
    let db = cwd.join("target/it-dl.db");
    let scripts = cwd.join("target/it-dl-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Connected))
        .expect("connected");

    // 上传一个跨多分片的大文件（3 片 × 小 chunk 触发多 DataChunk），再下载校验
    let payload: Vec<u8> = (0..(300 * 1024u32)).map(|x| (x % 251) as u8).collect();
    let src = cwd.join("target/it-dl-src.bin");
    std::fs::write(&src, &payload).unwrap();
    client.upload("/big.bin".into(), src.to_string_lossy().to_string());
    wait_for(&client, Duration::from_secs(20), |e| {
        matches!(e, Event::TransferDone { ok: true, .. })
    })
    .expect("upload big");

    // 下载到本地，校验内容一致（服务端下发的 sha256 通过）
    let dst = cwd.join("target/it-dl-dst.bin");
    let _ = std::fs::remove_file(&dst);
    let part = format!("{}.rdep-part", dst.to_string_lossy());
    let _ = std::fs::remove_file(&part);
    client.download("/big.bin".into(), dst.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(20), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("download done");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }), "download: {:?}", ev);
    assert_eq!(
        std::fs::read(&dst).expect("read downloaded"),
        payload,
        "downloaded content must match (sha-verified, streamed)"
    );
    // 原子改名后不应残留 .rdep-part 临时文件
    assert!(!std::path::Path::new(&part).exists(), "temp part file should be cleaned up");

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    println!("DOWNLOAD STREAM INTEGRITY TEST PASSED");
}

// ===========================================================================
// 权限位保留（可执行位 +x 不丢失）
// ===========================================================================

#[test]
#[cfg(unix)]
fn mode_preservation_e2e() {
    use std::os::unix::fs::PermissionsExt;
    let cwd = std::env::current_dir().expect("cwd");
    let port = 19983u16;
    let root = cwd.join("target/it-mode-root");
    let db = cwd.join("target/it-mode.db");
    let scripts = cwd.join("target/it-mode-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    // 本地脚本设为 0755
    let src = cwd.join("target/it-mode-start.sh");
    std::fs::write(&src, b"#!/bin/sh\necho ok\n").unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();

    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Connected))
        .expect("connected");

    client.upload("/start.sh".into(), src.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("upload done");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));

    // 远端应保留可执行位
    let remote = root.join("start.sh");
    let m = std::fs::metadata(&remote).expect("remote stat").permissions().mode() & 0o777;
    assert_eq!(m, 0o755, "remote should keep 0755, got {:o}", m);

    // 同步路径同样保留：改本地权限为 0644 再同步
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
    let local_dir = cwd.join("target/it-mode-local");
    let _ = std::fs::remove_dir_all(&local_dir);
    std::fs::create_dir_all(&local_dir).unwrap();
    std::fs::copy(&src, local_dir.join("s.sh")).unwrap();
    client.sync_dir(
        local_dir.to_string_lossy().to_string(),
        "/synced".into(),
        false,
        false,
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::OpDone { .. })
    })
    .expect("sync done");
    assert!(matches!(ev, Event::OpDone { ok: true, .. }), "sync: {:?}", ev);
    let m2 = std::fs::metadata(root.join("synced/s.sh")).expect("synced stat").permissions().mode() & 0o777;
    assert_eq!(m2, 0o644, "synced file should be 0644, got {:o}", m2);

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_dir_all(&local_dir);
}

// ===========================================================================
// 多分片流式上传（真实 1MB 分片，验证 send_file_chunks 跨片逻辑）
// ===========================================================================

#[test]
fn upload_multichunk_streaming_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 19993u16;
    let root = cwd.join("target/it-mc-root");
    let db = cwd.join("target/it-mc.db");
    let scripts = cwd.join("target/it-mc-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    // 2.5MB → 3 个 1MB 分片
    let payload: Vec<u8> = (0..(2_500_000u32)).map(|x| (x % 251) as u8).collect();
    let src = cwd.join("target/it-mc-src.bin");
    std::fs::write(&src, &payload).unwrap();

    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Connected))
        .expect("connected");

    // 流式上传（客户端内存恒定，服务端重组校验 sha）
    client.upload("/big.bin".into(), src.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(30), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("upload done");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }), "multichunk upload: {:?}", ev);

    // 下载回来逐字节比对（跨分片一致）
    let dst = cwd.join("target/it-mc-dst.bin");
    let _ = std::fs::remove_file(&dst);
    client.download("/big.bin".into(), dst.to_string_lossy().to_string());
    let ev = wait_for(&client, Duration::from_secs(30), |e| {
        matches!(e, Event::TransferDone { .. })
    })
    .expect("download done");
    assert!(matches!(ev, Event::TransferDone { ok: true, .. }));
    assert_eq!(
        std::fs::read(&dst).expect("read back"),
        payload,
        "multi-chunk roundtrip must be byte-identical"
    );

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    println!("MULTICHUNK STREAMING TEST PASSED");
}

// ===========================================================================
// 站点管理：保存的站点能真正驱动一次连接
// ===========================================================================

#[test]
fn site_saved_drives_connection_e2e() {
    use rdep_client::{Site, SiteStore};
    use rdep_client::sites::obfuscate_for_storage;

    let cwd = std::env::current_dir().expect("cwd");
    let port = 20003u16;
    let root = cwd.join("target/it-site-root");
    let db = cwd.join("target/it-site.db");
    let scripts = cwd.join("target/it-site-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    // 隔离的站点配置文件
    let sites_path = cwd.join("target/it-site-sites.json");
    let _ = std::fs::remove_file(&sites_path);
    let store = SiteStore::with_path(sites_path.clone());

    // 预先在远端建一个目录，作为「上次所在位置」
    std::fs::create_dir_all(root.join("myproj")).unwrap();
    std::fs::write(root.join("myproj/marker.txt"), b"marker").unwrap();

    // 1) 保存一个站点（密码走混淆存储）
    store
        .upsert(Site {
            name: "test-svc".into(),
            protocol: rdep_client::Protocol::Rdep,
            host: "localhost".into(),
            port,
            user: "admin".into(),
            password: obfuscate_for_storage("admin"),
            ca_cert: certs_dir().join("server.crt").to_string_lossy().to_string(),
            use_forwarder: false,
            target_service_id: String::new(),
            relay_token: String::new(),
            last_remote_dir: "/myproj".into(),
            use_token: false,
        })
        .expect("save site");

    // 2) 新进程视角：重新从磁盘载入（验证真的持久化了）
    let reloaded = SiteStore::with_path(sites_path.clone()).load().expect("reload");
    assert_eq!(reloaded.len(), 1, "site must survive a fresh load");
    let site = &reloaded[0];
    assert_eq!(site.name, "test-svc");
    assert_eq!(site.last_remote_dir, "/myproj");
    assert_eq!(site.password_plain(), "admin", "password must restore");

    // 3) 用站点配置驱动一次真实连接 + 落点校验
    let client = Client::new();
    client.connect(ConnectParams {
        host: site.host.clone(),
        port: site.port,
        user: site.user.clone(),
        pass: site.password_plain(),
        ca_cert: Some(std::path::PathBuf::from(&site.ca_cert)),
        use_forwarder: site.use_forwarder,
        target_service_id: site.target_service_id.clone(),
        relay_token: site.relay_token_plain(),
            use_token: false,
    });
    wait_for(&client, Duration::from_secs(15), |e| matches!(e, Event::Connected))
        .expect("connected using saved site");

    // 站点记录的 last_remote_dir 应能直接 ls 通（验证目录确实存在）
    client.ls(&site.last_remote_dir);
    let ev = wait_for(&client, Duration::from_secs(10), |e| {
        matches!(e, Event::DirListed { .. })
    })
    .expect("ls saved last_remote_dir");
    if let Event::DirListed { path, entries } = ev {
        assert_eq!(path, "/myproj");
        assert!(
            entries.iter().any(|e| e.name == "marker.txt"),
            "saved dir should contain marker.txt: {:?}",
            entries
        );
    }

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    // 4) 删除站点后文件同步移除
    store.remove("test-svc").expect("remove site");
    assert!(
        SiteStore::with_path(sites_path.clone()).load().unwrap().is_empty(),
        "site file should be empty after remove"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&sites_path);

    println!("SITE MANAGED CONNECTION TEST PASSED");
}

// ===========================================================================
// 认证闸门 + 暴力破解节流
// ===========================================================================

/// 建立一条**已认证**的原始 rdep TLS 会话。
async fn raw_session(port: u16) -> FrameCodec<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    use rustls::pki_types::ServerName;
    use rustls::RootCertStore;
    use std::sync::Arc;
    use tokio_rustls::TlsConnector;

    let pem = std::fs::read(certs_dir().join("server.crt")).expect("read server cert");
    let mut roots = RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("parse cert")
    {
        roots.add(c).expect("add root");
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(roots))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(cfg));
    let name = ServerName::try_from("localhost").expect("server name");
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    let tls = connector.connect(name, tcp).await.expect("tls handshake");
    FrameCodec::new(tls)
}

/// 发送一次认证，返回响应。
async fn raw_auth<S>(codec: &mut FrameCodec<S>, seq: u32, user: &str, pass: &str) -> CmdResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    raw_cmd(
        codec,
        seq,
        CmdType::Auth,
        &AuthRequest {
            user: user.into(),
            pass: pass.into(),
            method: AuthMethod::Password,
        },
    )
    .await
}

/// 核心安全测试：**未认证不得执行任何指令**；认证失败有节流。
#[test]
fn auth_gate_and_bruteforce_throttle_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 20013u16;
    let root = cwd.join("target/it-auth-root");
    let db = cwd.join("target/it-auth.db");
    let scripts = cwd.join("target/it-auth-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");

    rt.block_on(async {
        // ---- 1) 未认证：任何指令都必须被拒 ----
        let mut c = raw_session(port).await;

        let r = raw_cmd(&mut c, 1, CmdType::Ls, &LsRequest { path: "/".into(), recursive: false }).await;
        assert!(!r.ok, "未认证 LS 必须被拒绝");
        assert!(
            r.message.contains("not authenticated"),
            "应明确提示未认证，实际: {}",
            r.message
        );

        // 写操作同样必须被拒
        let r = raw_cmd(&mut c, 2, CmdType::Mkdir, &MkdirRequest { paths: vec!["/evil".into()] }).await;
        assert!(!r.ok, "未认证 Mkdir 必须被拒绝");
        assert!(
            !root.join("evil").exists(),
            "未认证的建目录请求绝不能生效"
        );

        // 读文件（下载）也必须被拒
        let r = raw_cmd(
            &mut c,
            3,
            CmdType::Download,
            &DownloadRequest { remote_path: "/x".into(), policy: NamePolicy::Overwrite },
        )
        .await;
        assert!(!r.ok, "未认证 Download 必须被拒绝");

        // 危险指令：重启脚本（发布收尾）
        let r = raw_cmd(
            &mut c,
            4,
            CmdType::PublishCommit,
            &PublishCommitRequest { remote_dir: "/".into() },
        )
        .await;
        assert!(!r.ok, "未认证 PublishCommit 必须被拒绝");

        // ---- 2) 错误口令连续失败 → 触发节流 ----
        for i in 1..=5u32 {
            let r = raw_auth(&mut c, 10 + i, "admin", "wrong").await;
            assert!(!r.ok, "错误口令第 {i} 次必须失败");
        }
        // 超过上限后，即使口令正确也拒绝（要求重连）
        let r = raw_auth(&mut c, 30, "admin", "admin").await;
        assert!(!r.ok, "触发节流后正确口令也应被拒（需重连）");
        assert!(
            r.message.contains("too many failed"),
            "应提示尝试次数过多，实际: {}",
            r.message
        );
        drop(c);

        // ---- 3) 重连后节流重置，正常认证可用 ----
        let mut c2 = raw_session(port).await;
        let r = raw_auth(&mut c2, 1, "admin", "admin").await;
        assert!(r.ok, "重连后正确口令应认证成功: {}", r.message);

        // 认证后可正常执行指令
        let r = raw_cmd(&mut c2, 2, CmdType::Ls, &LsRequest { path: "/".into(), recursive: false }).await;
        assert!(r.ok, "认证后 LS 应成功: {}", r.message);

        // ---- 4) 认证成功后失败计数被重置：再错 5 次仍会节流，说明计数独立于认证状态 ----
        for i in 1..=5u32 {
            let _ = raw_auth(&mut c2, 100 + i, "admin", "nope").await;
        }
        let r = raw_auth(&mut c2, 200, "admin", "admin").await;
        assert!(
            !r.ok,
            "已认证连接上再连续失败也应触发节流（防止认证后被用来爆破其他账号）"
        );
    });

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);

    println!("AUTH GATE / BRUTEFORCE THROTTLE TEST PASSED");
}

// ===========================================================================
// 项目即部署配置单一来源（publish_by_project）
// ===========================================================================

/// 验证「项目」不再是装饰性记录：
/// 1) 通过 Web API 建项目（remote_dir + restart_script 存在 projects 表）；
/// 2) 客户端按项目名发布，且**故意**自报一个不同的 remote_path；
/// 3) 文件必须落在**项目记录的目录**下，而不是客户端自报的路径；
/// 4) 项目不存在时明确报错。
#[test]
fn publish_by_project_uses_db_config_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 20023u16;
    let web_port = 20024u16;
    let root = cwd.join("target/it-proj-root");
    let db = cwd.join("target/it-proj.db");
    let scripts = cwd.join("target/it-proj-scripts");
    let _svc = start_service(port, Some(web_port), &root, &db, &scripts);
    // start_service 只建目录，重启脚本需测试自行写入（项目里引用的就是它）
    std::fs::write(scripts.join("restart"), "#!/bin/sh\nexit 0\n").unwrap();

    // ---- 通过 Web 后台建项目（真实运维路径）----
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/login",
        None,
        Some(r#"{"username":"admin","password":"admin"}"#),
    );
    assert_eq!(st, 200, "login: {body}");
    let token = extract_token(&body);

    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/projects",
        Some(&token),
        Some(r#"{"name":"shop","remote_dir":"/srv/shop","restart_script":"restart"}"#),
    );
    assert_eq!(st, 200, "create project: {body}");

    // 确认项目确实进了 projects 表（不是只返回了个 200）
    let (st, body) = http_request(web_port, "GET", "/api/projects", Some(&token), None);
    assert_eq!(st, 200);
    assert!(body.contains("shop") && body.contains("/srv/shop"), "project list: {body}");

    // ---- 客户端按项目名发布 ----
    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(15), |e| matches!(e, Event::Connected))
        .expect("connected");

    let payload = b"project-scoped-payload".to_vec();
    let src = cwd.join("target/it-proj-src.txt");
    std::fs::write(&src, &payload).unwrap();

    // remote_path 故意写成 /attacker/chosen/path.txt —— 服务端应忽略其目录部分，
    // 只取文件名并落到项目目录 /srv/shop 下。
    client.publish_by_project(
        "shop".into(),
        vec![PublishFile {
            remote_path: "/attacker/chosen/path.txt".into(),
            local_path: src.to_string_lossy().to_string(),
        }],
    );
    let ev = wait_for(&client, Duration::from_secs(20), |e| {
        matches!(e, Event::PublishDone { .. })
    })
    .expect("publish by project done");
    assert!(
        matches!(ev, Event::PublishDone { ok: true, .. }),
        "按项目发布应成功: {ev:?}"
    );

    // 关键断言：落在项目目录，且**没有**落到客户端自报的目录
    assert_eq!(
        std::fs::read(root.join("srv/shop/path.txt")).unwrap_or_default(),
        payload,
        "文件必须落在项目记录的 /srv/shop 下"
    );
    assert!(
        !root.join("attacker").exists(),
        "客户端自报的 /attacker 路径必须被忽略（项目为单一来源）"
    );

    // ---- 项目不存在时必须明确报错 ----
    client.publish_by_project(
        "no-such-project".into(),
        vec![PublishFile {
            remote_path: "/x.txt".into(),
            local_path: src.to_string_lossy().to_string(),
        }],
    );
    let ev = wait_for(&client, Duration::from_secs(15), |e| {
        matches!(e, Event::PublishDone { .. })
    })
    .expect("publish with bad project returns");
    if let Event::PublishDone { ok, message } = &ev {
        assert!(!ok, "不存在的项目必须失败");
        assert!(
            message.contains("project not found"),
            "应明确说明项目不存在，实际: {message}"
        );
    }

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&src);

    println!("PUBLISH BY PROJECT TEST PASSED");
}

// ===========================================================================
// 控制帧向前兼容：无法解码的 Ctrl 帧不得终止会话
// ===========================================================================

/// 回归测试：早期实现对无法解码的 Ctrl 帧使用 `?` 直接返回错误，
/// 会**终止整个会话**。这意味着协议版本错配（新客户端发送服务端不认识的
/// 控制帧）会直接断连。现要求：忽略该帧，会话继续可用。
#[test]
fn undecodable_ctrl_frame_does_not_kill_session_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 20033u16;
    let root = cwd.join("target/it-ctrl-root");
    let db = cwd.join("target/it-ctrl.db");
    let scripts = cwd.join("target/it-ctrl-scripts");
    let _svc = start_service(port, None, &root, &db, &scripts);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");

    rt.block_on(async {
        use tokio::time::timeout;
        let mut c = raw_authed_session(port).await;

        // 发送**无法解码**的 Ctrl 帧：帧类型正确，payload 是无法解析为任何
        // 已知变体的字节。注意不能全用 0x00 —— postcard 枚举变体是 varint，
        // `0x00` 就是变体 0 = Ctrl::Ping，服务端会正常回 Pong（那不是本测试要的）。
        // 0xFF 置最高位表示 varint 继续，会一直消耗到 payload 末尾而失败。
        let garbage = vec![0xFFu8; 7];
        c.write_frame(&Frame::new(FrameType::Ctrl, FrameFlags::new(), garbage))
            .await
            .expect("write garbage ctrl frame");

        // 精确定义行为：被忽略的帧**不应有任何响应**
        let stray = timeout(Duration::from_millis(700), c.read_frame()).await;
        assert!(
            stray.is_err(),
            "无法解码的 Ctrl 帧应被完全忽略（无任何响应），却收到了帧: {stray:?}"
        );

        // 会话必须仍然可用：正常指令照常响应
        let r = raw_cmd(
            &mut c,
            10,
            CmdType::Ls,
            &LsRequest { path: "/".into(), recursive: false },
        )
        .await;
        assert!(r.ok, "收到无法解码的 Ctrl 帧后，会话必须仍然可用: {}", r.message);

        // 再发一次（不同长度）垃圾帧 + 验证会话依然可用
        let garbage2 = vec![0xFFu8; 3];
        c.write_frame(&Frame::new(FrameType::Ctrl, FrameFlags::new(), garbage2))
            .await
            .expect("write second garbage ctrl frame");
        let stray2 = timeout(Duration::from_millis(700), c.read_frame()).await;
        assert!(stray2.is_err(), "第二个垃圾帧同样应被忽略: {stray2:?}");

        let r = raw_cmd(
            &mut c,
            11,
            CmdType::Mkdir,
            &MkdirRequest { paths: vec!["/still-alive".into()] },
        )
        .await;
        assert!(r.ok, "第二次垃圾帧后仍应可用: {}", r.message);
        assert!(
            root.join("still-alive").is_dir(),
            "指令应真正生效（不只是没报错）"
        );

        // 正常 Ctrl::Ping 仍然要回 Pong
        c.write_frame(&Frame::new(
            FrameType::Ctrl,
            FrameFlags::new(),
            postcard::to_allocvec(&rdep_protocol::Ctrl::Ping).unwrap(),
        ))
        .await
        .expect("write ping");
        let f = c
            .read_frame()
            .await
            .expect("read pong")
            .expect("pong frame present");
        assert_eq!(f.frame_type, FrameType::Ctrl, "Ping 应回 Ctrl 帧");
        let pong: rdep_protocol::Ctrl = postcard::from_bytes(&f.payload).expect("decode pong");
        assert!(matches!(pong, rdep_protocol::Ctrl::Pong), "应为 Pong");
    });

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);

    println!("CTRL FORWARD-COMPAT TEST PASSED");
}

// ===========================================================================
// Web 后台破坏性端点覆盖：/api/rollback 与 DELETE /api/projects/:id
// ===========================================================================

/// 这两个端点此前**零测试覆盖**，且都会改动生产状态：
/// - `POST /api/rollback` 触发回滚（把备份恢复到部署目录）——也正是此前
///   「任意目录读取」漏洞的攻击面，必须有 HTTP 层的越权回归测试；
/// - `DELETE /api/projects/:id` 删除项目。
#[test]
fn web_destructive_endpoints_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 20043u16;
    let web_port = 20044u16;
    let root = cwd.join("target/it-webdel-root");
    let db = cwd.join("target/it-webdel.db");
    let scripts = cwd.join("target/it-webdel-scripts");
    let _svc = start_service(port, Some(web_port), &root, &db, &scripts);
    std::fs::write(scripts.join("restart"), "#!/bin/sh\nexit 0\n").unwrap();

    // ---- 未认证：两个破坏性端点都必须 401 ----
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/rollback",
        None,
        Some(r#"{"remote_dir":"/pub","version":"2601071200"}"#),
    );
    assert_eq!(st, 401, "未认证回滚必须被拒");
    let (st, _) = http_request(web_port, "DELETE", "/api/projects/1", None, None);
    assert_eq!(st, 401, "未认证删除项目必须被拒");

    // ---- 登录 ----
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/login",
        None,
        Some(r#"{"username":"admin","password":"admin"}"#),
    );
    assert_eq!(st, 200, "login: {body}");
    let token = extract_token(&body);

    // ---- 准备可回滚的真实备份：先发布一次，产生 backup/<版本>/ ----
    let client = Client::new();
    connect(&client, port);
    wait_for(&client, Duration::from_secs(20), |e| matches!(e, Event::Connected))
        .expect("connected");

    let v0 = b"version-zero".to_vec();
    let v1 = b"version-one-new".to_vec();
    let s0 = cwd.join("target/it-webdel-v0.bin");
    let s1 = cwd.join("target/it-webdel-v1.bin");
    std::fs::write(&s0, &v0).unwrap();
    std::fs::write(&s1, &v1).unwrap();

    client.publish(
        "/pub".into(),
        "restart".into(),
        vec![PublishFile {
            remote_path: "/pub/app.bin".into(),
            local_path: s0.to_string_lossy().to_string(),
        }],
    );
    wait_for(&client, Duration::from_secs(20), |e| {
        matches!(e, Event::PublishDone { .. })
    })
    .expect("publish v0");
    assert_eq!(std::fs::read(root.join("pub/app.bin")).unwrap(), v0);

    // 再发布 v1（此时会先备份 v0，产生一个版本目录）
    client.publish(
        "/pub".into(),
        "restart".into(),
        vec![PublishFile {
            remote_path: "/pub/app.bin".into(),
            local_path: s1.to_string_lossy().to_string(),
        }],
    );
    wait_for(&client, Duration::from_secs(20), |e| {
        matches!(e, Event::PublishDone { .. })
    })
    .expect("publish v1");
    assert_eq!(std::fs::read(root.join("pub/app.bin")).unwrap(), v1);

    // 取一个真实版本号
    let (st, body) = http_request(web_port, "GET", "/api/backups", Some(&token), None);
    assert_eq!(st, 200, "backups: {body}");
    // 版本号格式为 YYMMDDHHmm（10 位数字，定宽可按字典序排序）
    let version = body
        .split('"')
        .find(|s| s.len() == 10 && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| panic!("应能从备份列表解析出 10 位版本号: {body}"))
        .to_string();

    // ---- 越权尝试：版本号做路径注入，必须被拒且不产生任何效果 ----
    let evil_ver = "../../../../etc";
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/rollback",
        Some(&token),
        Some(&format!(r#"{{"remote_dir":"/pub","version":"{evil_ver}"}}"#)),
    );
    assert_eq!(st, 400, "路径注入版本号必须 400: {body}");
    assert!(
        !root.join("etc").exists(),
        "注入不得在 root 内创建任何东西"
    );
    // 文件内容不受影响
    assert_eq!(
        std::fs::read(root.join("pub/app.bin")).unwrap(),
        v1,
        "失败的回滚不得改变文件"
    );

    // ---- 非法 remote_dir（路径穿越）同样必须被拒 ----
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/rollback",
        Some(&token),
        Some(&format!(
            r#"{{"remote_dir":"../../../../etc","version":"{version}"}}"#
        )),
    );
    assert_eq!(st, 400, "穿越 remote_dir 必须 400: {body}");

    // ---- 正常回滚：文件应恢复为 v0 ----
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/rollback",
        Some(&token),
        Some(&format!(r#"{{"remote_dir":"/pub","version":"{version}"}}"#)),
    );
    assert_eq!(st, 200, "正常回滚应成功: {body}");
    assert_eq!(
        std::fs::read(root.join("pub/app.bin")).unwrap(),
        v0,
        "回滚后应恢复为备份中的旧版本"
    );

    // ---- 不存在的版本 → 400，且不改变文件 ----
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/rollback",
        Some(&token),
        Some(r#"{"remote_dir":"/pub","version":"000000000000"}"#),
    );
    assert_eq!(st, 400, "不存在的版本应 400");
    assert_eq!(std::fs::read(root.join("pub/app.bin")).unwrap(), v0);

    // ---- DELETE /api/projects/:id ----
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/projects",
        Some(&token),
        Some(r#"{"name":"to-delete","remote_dir":"/srv/x","restart_script":"restart"}"#),
    );
    assert_eq!(st, 200, "create project");
    let (st, body) = http_request(web_port, "GET", "/api/projects", Some(&token), None);
    assert_eq!(st, 200);
    let pid = body
        .split("to-delete")
        .next()
        .and_then(|s| s.rfind("\"id\":"))
        .map(|i| {
            let r = &body[i + 5..];
            let end = r
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(r.len());
            r[..end].to_string()
        })
        .expect("project id");

    let (st, body) = http_request(web_port, "DELETE", &format!("/api/projects/{pid}"), Some(&token), None);
    assert_eq!(st, 200, "delete project: {body}");
    let (_, body) = http_request(web_port, "GET", "/api/projects", Some(&token), None);
    assert!(
        !body.contains("to-delete"),
        "项目应已删除: {body}"
    );

    // 删除不存在的 id：按 HTTP DELETE 的**幂等语义**返回 200（无副作用）。
    // 这与 delete_user 行为一致，属既定契约而非疏漏——显式断言以固化它。
    let (st, _) = http_request(web_port, "DELETE", "/api/projects/999999", Some(&token), None);
    assert_eq!(st, 200, "删除不存在的项目按幂等语义返回 200");
    // 且必须确认这没有误删任何真实项目
    let (_, body) = http_request(web_port, "GET", "/api/projects", Some(&token), None);
    assert!(
        !body.contains("to-delete") && !body.contains("/srv/x"),
        "幂等删除不得影响其他项目: {body}"
    );

    client.disconnect();
    wait_for(&client, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&s0);
    let _ = std::fs::remove_file(&s1);

    println!("WEB DESTRUCTIVE ENDPOINTS TEST PASSED");
}

// ===========================================================================
// API 令牌认证（CI/CD 场景）：签发 → 使用 → 吊销
// ===========================================================================

/// `AuthMethod::Token` 此前在协议里定义但服务端恒返回 false（死分支）。
/// 本用例验证完整闭环，并确认令牌**明文只出现一次**、吊销后立即失效。
#[test]
fn api_token_auth_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let port = 20053u16;
    let web_port = 20054u16;
    let root = cwd.join("target/it-tok-root");
    let db = cwd.join("target/it-tok.db");
    let scripts = cwd.join("target/it-tok-scripts");
    let _svc = start_service(port, Some(web_port), &root, &db, &scripts);

    // ---- 未认证不能管理令牌 ----
    let (st, _) = http_request(web_port, "GET", "/api/tokens", None, None);
    assert_eq!(st, 401, "未认证访问令牌列表必须 401");
    let (st, _) = http_request(
        web_port,
        "POST",
        "/api/tokens",
        None,
        Some(r#"{"username":"ci","label":"x"}"#),
    );
    assert_eq!(st, 401, "未认证签发令牌必须 401");

    // ---- 登录后签发令牌 ----
    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/login",
        None,
        Some(r#"{"username":"admin","password":"admin"}"#),
    );
    assert_eq!(st, 200, "login: {body}");
    let token = extract_token(&body);

    let (st, body) = http_request(
        web_port,
        "POST",
        "/api/tokens",
        Some(&token),
        Some(r#"{"username":"ci","label":"github-actions"}"#),
    );
    assert_eq!(st, 200, "签发令牌应成功: {body}");
    // 提取明文令牌
    let key = "\"token\":\"";
    let start = body.find(key).map(|i| i + key.len()).expect("token field");
    let rest = &body[start..];
    let plain = rest[..rest.find('"').expect("token end")].to_string();
    assert!(
        plain.starts_with("rdp_"),
        "令牌应有可识别前缀: {plain}"
    );
    assert!(plain.len() >= 32, "令牌长度不足: {}", plain.len());
    let tid = body
        .split("\"id\":")
        .nth(1)
        .and_then(|s| {
            let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            s[..end].to_string().parse::<i64>().ok()
        })
        .expect("token id");

    // ---- 列表只含元数据，绝不泄露明文 ----
    let (st, body) = http_request(web_port, "GET", "/api/tokens", Some(&token), None);
    assert_eq!(st, 200, "list tokens: {body}");
    assert!(body.contains("github-actions"), "应含标签: {body}");
    assert!(
        !body.contains(&plain),
        "令牌列表绝不能回显明文令牌: {body}"
    );

    // ---- 用令牌建立 rdep 会话（不做口令认证）----
    // 复用同一个 Client：connect 会替换其会话，四种认证场景顺序执行即可，
    // 避免为每个场景各起一个后台 runtime（本机内存受限）。
    let tok_client = Client::new();
    tok_client.connect(ConnectParams {
        host: "localhost".into(),
        port,
        user: "ci".into(),
        pass: plain.clone(),
        ca_cert: Some(certs_dir().join("server.crt")),
        use_forwarder: false,
        target_service_id: String::new(),
        relay_token: String::new(),
        use_token: true,
    });
    wait_for(&tok_client, Duration::from_secs(20), |e| {
        matches!(e, Event::Connected)
    })
    .expect("令牌认证应连接成功");

    // 令牌会话应能正常操作
    tok_client.ls("/");
    let ev = wait_for(&tok_client, Duration::from_secs(10), |e| {
        matches!(e, Event::DirListed { .. })
    })
    .expect("令牌会话应能执行指令");
    if let Event::DirListed { path, .. } = &ev {
        assert_eq!(path, "/");
    }
    tok_client.disconnect();
    wait_for(&tok_client, Duration::from_secs(10), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("disconnected");

    // ---- 使用后 last_used_at 应被更新（可审计）----
    let (_, body) = http_request(web_port, "GET", "/api/tokens", Some(&token), None);
    assert!(
        !body.contains("\"last_used_at\":0"),
        "令牌被使用后 last_used_at 应更新: {body}"
    );

    // ---- 错误令牌必须失败（复用同一 Client）----
    let bad = tok_client;
    bad.connect(ConnectParams {
        host: "localhost".into(),
        port,
        user: "ci".into(),
        pass: "rdp_deadbeef".into(),
        ca_cert: Some(certs_dir().join("server.crt")),
        use_forwarder: false,
        target_service_id: String::new(),
        relay_token: String::new(),
        use_token: true,
    });
    let ev = wait_for(&bad, Duration::from_secs(20), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("错误令牌应导致断开");
    if let Event::Disconnected = ev {}
    // 确认它没有建立会话
    let connected = wait_for(&bad, Duration::from_millis(500), |e| {
        matches!(e, Event::Connected)
    });
    assert!(connected.is_none(), "错误令牌绝不能建立会话");

    // ---- 吊销后立即失效 ----
    let (st, body) = http_request(
        web_port,
        "DELETE",
        &format!("/api/tokens/{tid}"),
        Some(&token),
        None,
    );
    assert_eq!(st, 200, "吊销令牌: {body}");

    let revoked_client = bad;
    revoked_client.connect(ConnectParams {
        host: "localhost".into(),
        port,
        user: "ci".into(),
        pass: plain.clone(),
        ca_cert: Some(certs_dir().join("server.crt")),
        use_forwarder: false,
        target_service_id: String::new(),
        relay_token: String::new(),
        use_token: true,
    });
    wait_for(&revoked_client, Duration::from_secs(20), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("已吊销令牌应导致断开");
    let connected = wait_for(&revoked_client, Duration::from_millis(500), |e| {
        matches!(e, Event::Connected)
    });
    assert!(connected.is_none(), "已吊销的令牌绝不能建立会话");

    // ---- 口令认证仍应正常（未被令牌机制影响；同样复用该 Client）----
    connect(&revoked_client, port);
    wait_for(&revoked_client, Duration::from_secs(20), |e| {
        matches!(e, Event::Connected)
    })
    .expect("口令认证仍应可用");
    revoked_client.disconnect();
    wait_for(&revoked_client, Duration::from_secs(10), |e| {
        matches!(e, Event::Disconnected)
    })
    .expect("disconnected");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_dir_all(&scripts);

    println!("API TOKEN AUTH TEST PASSED");
}
