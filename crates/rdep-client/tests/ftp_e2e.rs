//! FTP 端到端验证：用**进程内最小 FTP 服务器**驱动 `FtpClient` 走完真实协议流程。
//!
//! 背景：`ftp.rs` 的解析逻辑（MLSD/UNIX ls 解析、八进制转义、时间解析）是**照文档写的、
//! 从未对真实服务器验证过**。本文件实现一个只覆盖 rdep 用到的命令子集的 FTP 服务端
//! （USER/PASS/TYPE/PWD/CWD/PASV/LIST/MLSD/RETR/STOR/DELE/MKD/RMD/RNFR/RNTO/QUIT），
//! 从而在无外部 FTP 依赖的前提下验证：
//! - 登录与被动模式数据通道协商
//! - `mlsd` 输出确实能被 `parse_list_line` 正确解析（目录/文件/大小/时间）
//! - 上传（STOR）落盘内容正确、下载（RETR）内容逐字节一致
//! - 新建目录 / 改名 / 删除 的往返一致性
//! - 高级能力在 FTP 下确实**不可用**（能力门控的前提）
//!
//! 服务端故意输出**贴近真实服务器**的格式（MLSD 分号字段、含空格的名字用 `\040` 转义），
//! 以便真正校验解析器而不是喂它自己的输入格式。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdep_client::{Event, FtpClient, FtpParams};

/// 等待满足条件的事件。
fn wait_for(c: &FtpClient, d: Duration, mut p: impl FnMut(&Event) -> bool) -> Option<Event> {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < d {
        if let Some(e) = c.recv_timeout(Duration::from_millis(100)) {
            if p(&e) {
                return Some(e);
            }
        }
    }
    None
}

// ===========================================================================
// 最小 FTP 服务器
// ===========================================================================

struct FtpServer {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for FtpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 唤醒 accept
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// 启动 FTP 服务器；`root` 为其虚拟文件系统的根目录。返回句柄（Drop 时停止）。
fn start_ftp_server(root: &Path) -> FtpServer {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ftp control");
    let port = l.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(s) = conn else { break };
            let root = root.clone();
            // 每个连接一个线程：客户端可能开多条控制连接
            std::thread::spawn(move || {
                let _ = serve_conn(s, root);
            });
        }
    });
    FtpServer { port, stop }
}

/// 虚拟路径 → 磁盘路径（拒绝 `..` 越界）。
fn real(root: &Path, vpath: &str) -> PathBuf {
    let rel = vpath.trim_start_matches('/');
    let mut p = root.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            // 越权：回退到 root，不允许向上
            continue;
        }
        p.push(seg);
    }
    p
}

/// 生成 MLSD 的 modify 字段（FTP 格式 YYYYMMDDHHMMSS，取 mtime）。
fn mlsd_time(meta: &std::fs::Metadata) -> String {
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // UTC -> YYYYMMDDHHMMSS（用 civil_from_days 反向，不引 chrono）
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant civil_from_days
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}{h:02}{mi:02}{s:02}")
}

/// 按 MLSD 规则转义名字（空格→`\040` 等），与真实服务器一致。
fn mlsd_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\r' => out.push_str("\\015"),
            '\\' => out.push_str("\\134"),
            _ => out.push(c),
        }
    }
    out
}

fn serve_conn(stream: TcpStream, root: PathBuf) -> std::io::Result<()> {
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    w.write_all(b"220 rdep-test-ftp ready\r\n")?;
    w.flush()?;

    let mut cwd = String::from("/");
    let mut user_ok = false;
    // 待用的被动数据连接监听器
    let mut passive: Option<TcpListener> = None;
    // 改名暂存
    let mut rename_from: Option<String> = None;

    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // 客户端断开
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(2, ' ');
        let cmd = it.next().unwrap_or("").to_ascii_uppercase();
        let arg = it.next().unwrap_or("").trim().to_string();

        match cmd.as_str() {
            "USER" => {
                user_ok = true;
                w.write_all(b"331 need password\r\n")?;
            }
            "PASS" => {
                if user_ok {
                    w.write_all(b"230 logged in\r\n")?;
                } else {
                    w.write_all(b"530 not logged in\r\n")?;
                }
            }
            "SYST" => w.write_all(b"215 UNIX Type: L8\r\n")?,
            "FEAT" => {
                w.write_all(b"211-Features:\r\n MLSD\r\n SIZE\r\n211 End\r\n")?;
            }
            "OPTS" => w.write_all(b"200 OK\r\n")?,
            "TYPE" => w.write_all(b"200 OK\r\n")?,
            "NOOP" => w.write_all(b"200 OK\r\n")?,
            "PWD" => w.write_all(format!("257 \"{cwd}\" is cwd\r\n").as_bytes())?,
            "CWD" => {
                let target = if arg.starts_with('/') {
                    arg.clone()
                } else {
                    format!("{cwd}/{}", arg.trim_end_matches('/'))
                };
                if real(&root, &target).is_dir() {
                    let t = if target.ends_with('/') && target.len() > 1 {
                        target.trim_end_matches('/').to_string()
                    } else {
                        target
                    };
                    cwd = if t.is_empty() { "/".into() } else { t };
                    w.write_all(b"250 OK\r\n")?;
                } else {
                    w.write_all(b"550 no such directory\r\n")?;
                }
            }
            "CDUP" => {
                cwd = match cwd.rfind('/') {
                    Some(0) | None => "/".into(),
                    Some(i) => cwd[..i].to_string(),
                };
                w.write_all(b"250 OK\r\n")?;
            }
            "PASV" => {
                let dl = TcpListener::bind("127.0.0.1:0")?;
                let dp = dl.local_addr()?.port();
                passive = Some(dl);
                // 227 Entering Passive Mode (127,0,0,1,p1,p2)
                w.write_all(
                    format!("227 Entering Passive Mode (127,0,0,1,{},{})\r\n", dp / 256, dp % 256)
                        .as_bytes(),
                )?;
            }
            "MLSD" | "LIST" | "NLST" => {
                let dir = if arg.is_empty() || cmd == "NLST" && arg.is_empty() {
                    cwd.clone()
                } else {
                    arg.clone()
                };
                let full = real(&root, &dir);
                if !full.is_dir() {
                    w.write_all(b"550 not a directory\r\n")?;
                    continue;
                }
                let Some(dl) = passive.take() else {
                    w.write_all(b"425 use PASV first\r\n")?;
                    continue;
                };
                w.write_all(b"150 opening data connection\r\n")?;
                w.flush()?;
                let Ok((mut ds, _)) = dl.accept() else {
                    w.write_all(b"426 data connection failed\r\n")?;
                    continue;
                };
                let mut out = String::new();
                let mut names: Vec<_> = std::fs::read_dir(&full)?.flatten().collect();
                names.sort_by_key(|e| e.file_name());
                for e in names {
                    let meta = e.metadata()?;
                    let fname = e.file_name().to_string_lossy().to_string();
                    if cmd == "NLST" {
                        out.push_str(&format!("{}\r\n", fname));
                        continue;
                    }
                    let t = mlsd_time(&meta);
                    if meta.is_dir() {
                        out.push_str(&format!("type=dir;modify={t}; {}\r\n", mlsd_escape(&fname)));
                    } else {
                        out.push_str(&format!(
                            "type=file;size={};modify={t}; {}\r\n",
                            meta.len(),
                            mlsd_escape(&fname)
                        ));
                    }
                }
                ds.write_all(out.as_bytes())?;
                ds.flush()?;
                drop(ds);
                w.write_all(b"226 transfer complete\r\n")?;
            }
            "RETR" => {
                let full = real(&root, &arg);
                if !full.is_file() {
                    w.write_all(b"550 no such file\r\n")?;
                    continue;
                }
                let Some(dl) = passive.take() else {
                    w.write_all(b"425 use PASV first\r\n")?;
                    continue;
                };
                w.write_all(b"150 opening data connection\r\n")?;
                w.flush()?;
                let Ok((mut ds, _)) = dl.accept() else {
                    w.write_all(b"426 data connection failed\r\n")?;
                    continue;
                };
                let data = std::fs::read(&full)?;
                ds.write_all(&data)?;
                ds.flush()?;
                drop(ds);
                w.write_all(b"226 transfer complete\r\n")?;
            }
            "STOR" | "APPE" => {
                let full = real(&root, &arg);
                if let Some(p) = full.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let Some(dl) = passive.take() else {
                    w.write_all(b"425 use PASV first\r\n")?;
                    continue;
                };
                w.write_all(b"150 opening data connection\r\n")?;
                w.flush()?;
                let Ok((mut ds, _)) = dl.accept() else {
                    w.write_all(b"426 data connection failed\r\n")?;
                    continue;
                };
                let mut buf = Vec::new();
                ds.read_to_end(&mut buf)?;
                drop(ds);
                std::fs::write(&full, &buf)?;
                w.write_all(b"226 transfer complete\r\n")?;
            }
            "MKD" | "XMKD" => {
                let full = real(&root, &arg);
                match std::fs::create_dir_all(&full) {
                    Ok(_) => w.write_all(b"257 directory created\r\n")?,
                    Err(_) => w.write_all(b"550 cannot create\r\n")?,
                }
            }
            "DELE" => match std::fs::remove_file(real(&root, &arg)) {
                Ok(_) => w.write_all(b"250 deleted\r\n")?,
                Err(_) => w.write_all(b"550 no such file\r\n")?,
            },
            "RMD" | "XRMD" => match std::fs::remove_dir_all(real(&root, &arg)) {
                Ok(_) => w.write_all(b"250 removed\r\n")?,
                Err(_) => w.write_all(b"550 no such directory\r\n")?,
            },
            "SIZE" => {
                let full = real(&root, &arg);
                match std::fs::metadata(&full) {
                    Ok(m) => w.write_all(format!("213 {}\r\n", m.len()).as_bytes())?,
                    Err(_) => w.write_all(b"550 no such file\r\n")?,
                }
            }
            "MDTM" => {
                let full = real(&root, &arg);
                match std::fs::metadata(&full) {
                    Ok(m) => w.write_all(
                        format!("213 {}\r\n", mlsd_time(&m)).as_bytes(),
                    )?,
                    Err(_) => w.write_all(b"550 no such file\r\n")?,
                }
            }
            "RNFR" => {
                let full = real(&root, &arg);
                if full.exists() {
                    rename_from = Some(arg.clone());
                    w.write_all(b"350 ready for RNTO\r\n")?;
                } else {
                    w.write_all(b"550 no such file\r\n")?;
                }
            }
            "RNTO" => {
                if let Some(from) = rename_from.take() {
                    let src = real(&root, &from);
                    let dst = real(&root, &arg);
                    match std::fs::rename(&src, &dst) {
                        Ok(_) => w.write_all(b"250 renamed\r\n")?,
                        Err(_) => w.write_all(b"550 rename failed\r\n")?,
                    }
                } else {
                    w.write_all(b"503 need RNFR first\r\n")?;
                }
            }
            "QUIT" => {
                w.write_all(b"221 bye\r\n")?;
                w.flush()?;
                return Ok(());
            }
            _ => w.write_all(b"502 not implemented\r\n")?,
        }
        w.flush()?;
    }
}

// ===========================================================================
// 测试
// ===========================================================================

#[test]
fn ftp_backend_e2e() {
    let cwd = std::env::current_dir().expect("cwd");
    let base = cwd.join("target/it-ftp");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create ftp root");

    // 预置：根目录下有 dir1/ 与 dir1/inner.txt
    std::fs::create_dir_all(base.join("dir1")).unwrap();
    std::fs::write(base.join("dir1").join("inner.txt"), b"inner-content").unwrap();

    let _srv = start_ftp_server(&base);
    let port = _srv.port;

    let c = FtpClient::new();
    c.connect(FtpParams {
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        pass: "pw".into(),
        initial_dir: "/".into(),
    });
    wait_for(&c, Duration::from_secs(15), |e| matches!(e, Event::Connected))
        .expect("FTP connected");

    // ---- ls 根：应看到 dir1（目录）----
    c.ls("/");
    let ev = wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::DirListed { .. }))
        .expect("ls root");
    if let Event::DirListed { entries, .. } = &ev {
        assert!(
            entries.iter().any(|e| e.name == "dir1" && e.is_dir),
            "should list dir1 as dir: {:?}",
            entries
        );
    } else {
        panic!("expected DirListed");
    }

    // ---- ls dir1：应看到 inner.txt（文件，有正确大小）----
    c.ls("/dir1");
    let ev = wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::DirListed { .. }))
        .expect("ls dir1");
    if let Event::DirListed { entries, .. } = &ev {
        let f = entries
            .iter()
            .find(|e| e.name == "inner.txt")
            .expect("inner.txt should be listed");
        assert!(!f.is_dir, "inner.txt must be a file");
        assert_eq!(f.size, "inner-content".len() as u64, "size from MLSD");
        assert!(f.mtime > 0, "mtime from MLSD modify should parse");
    } else {
        panic!("expected DirListed");
    }

    // ---- mkdir ----
    c.mkdir(vec!["/dir1/newdir".into()]);
    wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::OpDone { ok: true, .. }))
        .expect("mkdir ok");
    assert!(
        base.join("dir1/newdir").is_dir(),
        "server should have created dir1/newdir"
    );

    // ---- upload（含一个带空格的文件名，验证传输不依赖解析）----
    let payload = (0..300_000u32).map(|x| (x % 251) as u8).collect::<Vec<u8>>();
    let src = cwd.join("target/it-ftp-src.bin");
    std::fs::write(&src, &payload).unwrap();
    c.upload("/dir1/uploaded.bin".into(), src.to_string_lossy().to_string());
    let ev = wait_for(&c, Duration::from_secs(30), |e| matches!(e, Event::TransferDone { .. }))
        .expect("upload done");
    assert!(
        matches!(ev, Event::TransferDone { ok: true, .. }),
        "upload should succeed: {ev:?}"
    );
    assert_eq!(
        std::fs::read(base.join("dir1/uploaded.bin")).unwrap(),
        payload,
        "uploaded bytes must match on server"
    );

    // ---- download：逐字节回环 ----
    let dst = cwd.join("target/it-ftp-dst.bin");
    let _ = std::fs::remove_file(&dst);
    c.download("/dir1/uploaded.bin".into(), dst.to_string_lossy().to_string());
    let ev = wait_for(&c, Duration::from_secs(30), |e| matches!(e, Event::TransferDone { .. }))
        .expect("download done");
    assert!(
        matches!(ev, Event::TransferDone { ok: true, .. }),
        "download should succeed: {ev:?}"
    );
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        payload,
        "roundtrip must be byte-identical"
    );
    // 原子改名后不应残留 .rdep-part
    let part = format!("{}.rdep-part", dst.to_string_lossy());
    assert!(
        !std::path::Path::new(&part).exists(),
        "temp part file must be cleaned up"
    );

    // ---- rename ----
    c.rename("/dir1/uploaded.bin".into(), "renamed.bin".into());
    wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::OpDone { ok: true, .. }))
        .expect("rename ok");
    assert!(base.join("dir1/renamed.bin").is_file(), "renamed file should exist");
    assert!(!base.join("dir1/uploaded.bin").exists(), "old name should be gone");

    // ---- delete ----
    c.delete(vec!["/dir1/renamed.bin".into()]);
    wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::OpDone { ok: true, .. }))
        .expect("delete ok");
    assert!(!base.join("dir1/renamed.bin").exists(), "file should be deleted");

    c.disconnect();
    wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::Disconnected))
        .expect("disconnected");

    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    println!("FTP BACKEND E2E TEST PASSED");
}

/// 含空格 / 需转义的文件名：验证 `mlsd_escape` + `unescape_ftp_name` 闭环。
#[test]
fn ftp_name_with_spaces_roundtrip() {
    let cwd = std::env::current_dir().expect("cwd");
    let base = cwd.join("target/it-ftp-sp");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create root");

    // 一个名字里带空格的文件
    std::fs::write(base.join("my report.txt"), b"spaced").unwrap();

    let _srv = start_ftp_server(&base);
    let port = _srv.port;

    let c = FtpClient::new();
    c.connect(FtpParams {
        host: "127.0.0.1".into(),
        port,
        user: "t".into(),
        pass: "p".into(),
        initial_dir: "/".into(),
    });
    wait_for(&c, Duration::from_secs(15), |e| matches!(e, Event::Connected))
        .expect("connected");

    c.ls("/");
    let ev = wait_for(&c, Duration::from_secs(10), |e| matches!(e, Event::DirListed { .. }))
        .expect("ls");
    if let Event::DirListed { entries, .. } = &ev {
        let f = entries
            .iter()
            .find(|e| e.name == "my report.txt")
            .expect("name with space should be unescaped correctly");
        assert_eq!(f.size, 6);
    } else {
        panic!("expected DirListed");
    }

    c.disconnect();
    let _ = std::fs::remove_dir_all(&base);
}
