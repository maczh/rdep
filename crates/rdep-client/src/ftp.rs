//! FTP 协议后端。
//!
//! 原始需求要求 client 支持 `ftp | sftp | rdep`。本模块实现 **FTP** 后端，
//! 覆盖**基础文件操作**（浏览 / 上传 / 下载 / 新建目录 / 删除 / 改名）。
//!
//! ## 能力边界（重要）
//!
//! FTP 是通用文件传输协议，**不具备** rdep 的高级能力：
//! - 无「发布前备份 + 覆盖 + 回滚」模型（无原子回滚、无重启脚本执行）
//! - 无流式 tail / 远程 grep / 远程编辑
//! - 无断点续传（rdep 的 `stable_id` + 服务端分片暂存是自有机制）
//! - 无 forwarder 中转
//!
//! 因此 FTP 站点的 GUI 高级窗口会被**显式禁用并说明原因**（`Protocol::supports_advanced`），
//! 而不是静默失败。
//!
//! ## 运行时说明
//!
//! `suppaftp` 的异步实现基于 **async-std**（`put_with_stream` 等返回
//! `async_std::io::Read/Write` 的 `DataStream`），与 rdep 其余部分使用的 tokio 不同。
//! 因此本模块**独立运行在自己的线程 + async-std runtime** 上，通过 channel 与 GUI 通信，
//! 与 `client.rs` 的 tokio 桥接模型保持一致的形态但不共享 runtime。
//!
//! ## 安全
//!
//! 本实现使用**明文 FTP**（未启用 FTPS 隐式 TLS 变体）。公网传输请优先使用
//! rdep（TLS）或 SFTP。`Protocol::Ftp` 的界面标签已标注此限制。
//!
//! ## 与 GUI 的接口
//!
//! 本后端**复用 `client::Event`**，因此 GUI 的 `drain_events` 与各面板
//! 无需改动即可同时服务 rdep / FTP 两种后端。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_std::fs as afs;
use async_std::io::{ReadExt, WriteExt};
use rdep_protocol::{Direction, FileEntry};
use suppaftp::AsyncFtpStream;
use tokio::sync::mpsc as async_mpsc;

use crate::client::Event;
use crate::i18n::{t, tf};

/// 缓冲大小（读文件 / 写文件 / 数据流包装共用）。
const BUF: usize = 256 * 1024;

static FTP_ID: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
    FTP_ID.fetch_add(1, Ordering::Relaxed)
}

fn send(tx: &std_mpsc::Sender<Event>, e: Event) {
    // 事件统一出口：FTP 后端所有回传都留痕
    tracing::debug!(event = ?e, "ftp event -> gui");
    let _ = tx.send(e);
}

/// FTP 连接参数。
#[derive(Clone)]
pub struct FtpParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    /// 登录后进入的初始目录（空 = 服务器默认）。
    pub initial_dir: String,
}

/// 自定义 Debug：口令在日志中必须脱敏（`FtpCommand::Connect` 会整包打印）。
impl std::fmt::Debug for FtpParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("pass", &"***")
            .field("initial_dir", &self.initial_dir)
            .finish()
    }
}

/// GUI → FTP 后台 的指令。
#[derive(Debug)]
pub enum FtpCommand {
    Connect(FtpParams),
    Disconnect,
    Ls(String),
    Mkdir(Vec<String>),
    Upload {
        remote_path: String,
        local_path: String,
    },
    Download {
        remote_path: String,
        local_path: String,
    },
    Delete(Vec<String>),
    Rename {
        src: String,
        new_name: String,
    },
}

/// FTP 客户端句柄：与 `Client` 同样的「GUI 线程持有 + 后台线程执行」模型。
///
/// 差别在于后台跑的是 **async-std** runtime（suppaftp 的要求），而 `Client` 跑 tokio。
pub struct FtpClient {
    cmd_tx: async_mpsc::Sender<FtpCommand>,
    evt_rx: std_mpsc::Receiver<Event>,
}

impl FtpClient {
    /// 启动后台任务与事件通道。
    pub fn new() -> Self {
        let (cmd_tx, mut cmd_rx) = async_mpsc::channel::<FtpCommand>(64);
        let (evt_tx, evt_rx) = std_mpsc::channel::<Event>();
        // 指令接收端跨 runtime：用 blocking_recv 拉取，再 spawn_local 执行。
        // 这避免了两个 async runtime 之间的桥接复杂度。
        std::thread::spawn(move || {
            async_std::task::block_on(async move {
                let mut client: Option<AsyncFtpStream> = None;
                // FTP 后端跑在独立的 async-std runtime（非 tokio），此处 blocking_recv 是安全的
                while let Some(cmd) = cmd_rx.blocking_recv() {
                    tracing::debug!(?cmd, "ftp command <- gui");
                    match cmd {
                        FtpCommand::Connect(p) => {
                            send(
                                &evt_tx,
                                Event::Status(tf("connecting {host}:{port} ...", &[
                                    ("host", &p.host),
                                    ("port", &p.port.to_string()),
                                ])),
                            );
                            match do_connect(&p).await {
                                Ok(c) => {
                                    client = Some(c);
                                    send(&evt_tx, Event::Connected);
                                    send(&evt_tx, Event::Status(t("FTP connected").to_string()));
                                }
                                Err(e) => {
                                    client = None;
                                    send(
                                        &evt_tx,
                                        Event::Error(format!("{}: {e:#}", t("FTP connect failed"))),
                                    );
                                    send(&evt_tx, Event::Disconnected);
                                }
                            }
                        }
                        FtpCommand::Disconnect => {
                            if let Some(c) = client.as_mut() {
                                let _ = c.quit().await;
                            }
                            client = None;
                            send(&evt_tx, Event::Disconnected);
                        }
                        FtpCommand::Ls(path) => match client.as_mut() {
                            Some(c) => do_ls(c, &path, &evt_tx).await,
                            None => send(
                                &evt_tx,
                                Event::Error(t("not connected (FTP)").to_string()),
                            ),
                        },
                        FtpCommand::Mkdir(paths) => match client.as_mut() {
                            Some(c) => do_mkdir(c, &paths, &evt_tx).await,
                            None => send(
                                &evt_tx,
                                Event::Error(t("not connected (FTP)").to_string()),
                            ),
                        },
                        FtpCommand::Upload {
                            remote_path,
                            local_path,
                        } => match client.as_mut() {
                            Some(c) => do_upload(c, &remote_path, &local_path, &evt_tx).await,
                            None => send(
                                &evt_tx,
                                Event::Error(t("not connected (FTP)").to_string()),
                            ),
                        },
                        FtpCommand::Download {
                            remote_path,
                            local_path,
                        } => match client.as_mut() {
                            Some(c) => do_download(c, &remote_path, &local_path, &evt_tx).await,
                            None => send(
                                &evt_tx,
                                Event::Error(t("not connected (FTP)").to_string()),
                            ),
                        },
                        FtpCommand::Delete(paths) => match client.as_mut() {
                            Some(c) => do_delete(c, &paths, &evt_tx).await,
                            None => send(
                                &evt_tx,
                                Event::Error(t("not connected (FTP)").to_string()),
                            ),
                        },
                        FtpCommand::Rename { src, new_name } => match client.as_mut() {
                            Some(c) => do_rename(c, &src, &new_name, &evt_tx).await,
                            None => send(
                                &evt_tx,
                                Event::Error(t("not connected (FTP)").to_string()),
                            ),
                        },
                    }
                }
            });
        });
        FtpClient { cmd_tx, evt_rx }
    }

    pub fn connect(&self, p: FtpParams) {
        tracing::debug!(host = %p.host, port = p.port, user = %p.user, pass_len = p.pass.len(), "ftp api: connect");
        let _ = self.cmd_tx.blocking_send(FtpCommand::Connect(p));
    }
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.blocking_send(FtpCommand::Disconnect);
    }
    pub fn ls(&self, path: &str) {
        tracing::debug!(path, "ftp api: ls");
        let _ = self
            .cmd_tx
            .blocking_send(FtpCommand::Ls(path.to_string()));
    }
    pub fn mkdir(&self, paths: Vec<String>) {
        tracing::debug!(?paths, "ftp api: mkdir");
        let _ = self.cmd_tx.blocking_send(FtpCommand::Mkdir(paths));
    }
    pub fn upload(&self, remote_path: String, local_path: String) {
        tracing::debug!(local = %local_path, remote = %remote_path, "ftp api: upload");
        let _ = self.cmd_tx.blocking_send(FtpCommand::Upload {
            remote_path,
            local_path,
        });
    }
    pub fn download(&self, remote_path: String, local_path: String) {
        tracing::debug!(remote = %remote_path, local = %local_path, "ftp api: download");
        let _ = self.cmd_tx.blocking_send(FtpCommand::Download {
            remote_path,
            local_path,
        });
    }
    pub fn delete(&self, paths: Vec<String>) {
        tracing::debug!(?paths, "ftp api: delete");
        let _ = self.cmd_tx.blocking_send(FtpCommand::Delete(paths));
    }
    pub fn rename(&self, src: String, new_name: String) {
        tracing::debug!(src = %src, new_name = %new_name, "ftp api: rename");
        let _ = self.cmd_tx.blocking_send(FtpCommand::Rename { src, new_name });
    }

    /// 非阻塞取一个事件（GUI 每帧调用，与 `Client::next_event` 同语义）。
    pub fn next_event(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }

    /// 阻塞等待一个事件（测试用）。
    pub fn recv_timeout(&self, d: Duration) -> Option<Event> {
        self.evt_rx.recv_timeout(d).ok()
    }
}

async fn do_connect(p: &FtpParams) -> Result<AsyncFtpStream> {
    // suppaftp 的 connect_timeout 需要一个已解析的 SocketAddr
    let addr = async_std::net::ToSocketAddrs::to_socket_addrs(&(p.host.as_str(), p.port))
        .await
        .with_context(|| format!("resolve {}:{}", p.host, p.port))?
        .next()
        .with_context(|| {
            format!(
                "{}: {}",
                tf("failed to resolve {host}:{port}", &[
                    ("host", &p.host),
                    ("port", &p.port.to_string()),
                ]),
                "unresolved",
            )
        })?;

    let mut c = AsyncFtpStream::connect_timeout(addr, Duration::from_secs(15))
        .await
        .context("ftp connect")?;
    c.login(p.user.as_str(), p.pass.as_str())
        .await
        .context("ftp login")?;
    if !p.initial_dir.trim().is_empty() {
        c.cwd(p.initial_dir.trim())
            .await
            .with_context(|| format!("cwd {}", p.initial_dir))?;
    }
    Ok(c)
}

/// 解析 `mlsd`/`list` 的一行输出为 `FileEntry`。
///
/// 优先 MLSD（`type=dir;size=0;modify=...; name`，跨平台稳定、字段结构化）；
/// 解析失败时回退到 UNIX `ls -l`（取第 5 字段为大小、第 9 字段起为名字，支持含空格名）。
fn parse_list_line(line: &str) -> Option<FileEntry> {
    let line = line.trim();
    if line.is_empty() || line == "." || line == ".." {
        return None;
    }

    // --- MLSD 格式 ---
    if line.contains('=') && line.contains(';') {
        // 名字在最后一个空格之后（MLSD 会转义空格为 \040，但我们也容忍原始空格）
        let (facts, name) = match line.rfind(' ') {
            Some(sp) => (&line[..sp], line[sp + 1..].trim().to_string()),
            None => (line, String::new()),
        };
        let mut is_dir = false;
        let mut size = 0u64;
        let mut mtime = 0i64;
        for f in facts.split(';') {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            let Some((k, v)) = f.split_once('=') else {
                continue;
            };
            match k.trim().to_ascii_lowercase().as_str() {
                "type" => is_dir = v.trim().eq_ignore_ascii_case("dir") || v.trim() == "cdir",
                "size" => size = v.trim().parse().unwrap_or(0),
                "modify" => mtime = parse_ftp_time(v.trim()),
                _ => {}
            }
        }
        if name.is_empty() {
            return None;
        }
        return Some(FileEntry {
            name: unescape_ftp_name(&name),
            is_dir,
            size,
            mtime,
            mode: 0,
        });
    }

    // --- UNIX ls -l 回退格式 ---
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 8 && line.starts_with(['-', 'd', 'l']) {
        let is_dir = line.starts_with('d');
        let size = parts.get(4).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let name = parts[8..].join(" ");
        if name.is_empty() {
            return None;
        }
        return Some(FileEntry {
            name,
            is_dir,
            size,
            mtime: 0,
            mode: 0,
        });
    }

    // --- 兜底：只有名字 ---
    if !line.contains(' ') {
        return Some(FileEntry {
            name: line.to_string(),
            is_dir: false,
            size: 0,
            mtime: 0,
            mode: 0,
        });
    }
    None
}

/// 还原 FTP 对特殊字符的转义。
///
/// FTP 用反斜杠 + **八进制**表示特殊字符：`\040`=空格、`\011`=Tab、`\012`=换行、
/// `\\`=反斜杠本身。例：`my\040file.txt` 表示 `my file.txt`。
fn unescape_ftp_name(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 1 >= bytes.len() {
                // 结尾悬空反斜杠：畸形输入，无法构成完整转义 → 丢弃
                break;
            }
            // 三位八进制转义：\040
            if i + 3 < bytes.len() {
                let oct = &s[i + 1..i + 4];
                if let Ok(v) = u8::from_str_radix(oct, 8) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
            // 两字符转义：\\ 等
            out.push(bytes[i + 1]);
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析 FTP 的 `YYYYMMDDHHMMSS` 时间为 Unix 秒（按 UTC 处理，足够 UI 展示与排序）。
fn parse_ftp_time(v: &str) -> i64 {
    if v.len() < 14 {
        return 0;
    }
    let g = |a: usize, b: usize| -> i64 { v[a..b].parse().unwrap_or(0) };
    let (y, mo, d) = (g(0, 4), g(4, 6), g(6, 8));
    let (h, mi, s) = (g(8, 10), g(10, 12), g(12, 14));
    if y == 0 || mo == 0 || d == 0 {
        return 0;
    }
    // Howard Hinnant 的 days_from_civil 算法（避免引入 chrono 依赖）
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + h * 3600 + mi * 60 + s
}

async fn do_ls(c: &mut AsyncFtpStream, path: &str, evt: &std_mpsc::Sender<Event>) {
    // 优先 mlsd（结构化），空结果或失败则回退 list
    let lines = match c.mlsd(Some(path)).await {
        Ok(v) if !v.is_empty() => v,
        _ => match c.list(Some(path)).await {
            Ok(v) => v,
            Err(e) => {
                send(
                    evt,
                    Event::Error(format!("{}: {e}", t("FTP list failed"))),
                );
                return;
            }
        },
    };
    let mut entries: Vec<FileEntry> = lines.iter().filter_map(|l| parse_list_line(l)).collect();
    // 目录优先 + 名称排序（与 rdep 侧 ls 行为一致）
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    send(
        evt,
        Event::DirListed {
            path: path.to_string(),
            entries,
        },
    );
}

async fn do_mkdir(c: &mut AsyncFtpStream, paths: &[String], evt: &std_mpsc::Sender<Event>) {
    let mut errs = Vec::new();
    for p in paths {
        if let Err(e) = c.mkdir(p.as_str()).await {
            errs.push(format!("{p}: {e}"));
        }
    }
    send(
        evt,
        Event::OpDone {
            ok: errs.is_empty(),
            message: if errs.is_empty() {
                tf("created {n} directories", &[("n", &paths.len().to_string())])
            } else {
                format!("{}: {}", t("partial failures"), errs.join("; "))
            },
        },
    );
}

/// 上传：流式读本地文件 → 写 FTP 数据流，内存恒定。
async fn do_upload(
    c: &mut AsyncFtpStream,
    remote_path: &str,
    local_path: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    let id = next_id();
    send(
        evt,
        Event::TransferStarted {
            id,
            name: remote_path.to_string(),
            direction: Direction::Upload,
        },
    );
    let fail = |m: String| {
        send(
            evt,
            Event::TransferDone {
                id,
                ok: false,
                message: m.clone(),
            },
        );
    };

    let total = match std::fs::metadata(local_path) {
        Ok(m) => m.len(),
        Err(e) => {
            fail(format!("{}: {e}", t("failed to read local file metadata")));
            return;
        }
    };
    let mut reader = match afs::File::open(local_path).await {
        Ok(f) => f,
        Err(e) => {
            fail(format!("{}: {e}", t("failed to open local file")));
            return;
        }
    };

    // put_with_stream 返回数据流；写完必须调 finalize_put_stream 读服务器响应
    let mut stream = match c.put_with_stream(remote_path).await {
        Ok(s) => s,
        Err(e) => {
            fail(format!("{}: {e}", t("FTP upload failed")));
            return;
        }
    };
    let mut buf = vec![0u8; BUF];
    let mut sent = 0u64;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = c.abort(stream).await;
                fail(format!("{}: {e}", t("failed to read local file")));
                return;
            }
        };
        if let Err(e) = stream.write_all(&buf[..n]).await {
            let _ = c.abort(stream).await;
            fail(format!("{}: {e}", t("failed to write FTP data stream")));
            return;
        }
        sent += n as u64;
        if sent % (BUF as u64 * 8) < n as u64 {
            send(
                evt,
                Event::TransferProgress {
                    id,
                    sent,
                    total,
                },
            );
        }
    }
    if let Err(e) = stream.flush().await {
        let _ = c.abort(stream).await;
        fail(format!("{}: {e}", t("failed to close FTP data stream")));
        return;
    }
    drop(stream);
    if let Err(e) = c.finalize_put_stream(async_std::io::Cursor::new(Vec::<u8>::new())).await {
        // finalize 需要一个已关闭的流句柄；这里传空游标仅为触发响应读取
        fail(format!("{}: {e}", t("FTP upload not confirmed")));
        return;
    }

    send(
        evt,
        Event::TransferProgress {
            id,
            sent: total,
            total,
        },
    );
    send(
        evt,
        Event::TransferDone {
            id,
            ok: true,
            message: String::new(),
        },
    );
}

/// 下载：写临时文件 `<local>.rdep-part` → 成功后原子改名（不留半成品）。
async fn do_download(
    c: &mut AsyncFtpStream,
    remote_path: &str,
    local_path: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    let id = next_id();
    send(
        evt,
        Event::TransferStarted {
            id,
            name: remote_path.to_string(),
            direction: Direction::Download,
        },
    );
    let fail = |m: String| {
        send(
            evt,
            Event::TransferDone {
                id,
                ok: false,
                message: m.clone(),
            },
        );
    };

    let part = format!("{}.rdep-part", local_path);
    let mut writer = match afs::File::create(&part).await {
        Ok(f) => f,
        Err(e) => {
            fail(format!("{}: {e}", t("failed to create temp file")));
            return;
        }
    };

    let mut stream = match c.retr_as_stream(remote_path).await {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            fail(format!("{}: {e}", t("FTP download failed")));
            return;
        }
    };
    let mut buf = vec![0u8; BUF];
    let mut got = 0u64;
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                fail(format!("{}: {e}", t("failed to read FTP data stream")));
                return;
            }
        };
        if let Err(e) = writer.write_all(&buf[..n]).await {
            let _ = std::fs::remove_file(&part);
            fail(format!("{}: {e}", t("failed to write local file")));
            return;
        }
        got += n as u64;
        if got % (BUF as u64 * 8) < n as u64 {
            send(
                evt,
                Event::TransferProgress {
                    id,
                    sent: got,
                    total: 0,
                },
            );
        }
    }
    if let Err(e) = writer.flush().await {
        let _ = std::fs::remove_file(&part);
        fail(format!("{}: {e}", t("failed to flush local file")));
        return;
    }
    drop(writer);
    if let Err(e) = c.finalize_retr_stream(async_std::io::Cursor::new(Vec::<u8>::new())).await {
        let _ = std::fs::remove_file(&part);
        fail(format!("{}: {e}", t("FTP download not confirmed")));
        return;
    }

    // 原子改名
    if let Err(e) = std::fs::rename(&part, local_path) {
        let _ = std::fs::remove_file(&part);
        fail(format!("{}: {e}", t("rename failed")));
        return;
    }
    send(
        evt,
        Event::TransferProgress {
            id,
            sent: got,
            total: got,
        },
    );
    send(
        evt,
        Event::TransferDone {
            id,
            ok: true,
            message: String::new(),
        },
    );
}

async fn do_delete(c: &mut AsyncFtpStream, paths: &[String], evt: &std_mpsc::Sender<Event>) {
    let mut errs = Vec::new();
    let mut n = 0usize;
    for p in paths {
        // FTP 的 DELE/RMD 对类型敏感且无统一类型查询：先试删文件，失败再试删目录
        let r = match c.rm(p.as_str()).await {
            Ok(_) => Ok(()),
            Err(e1) => c.rmdir(p.as_str()).await.map_err(|_| e1),
        };
        match r {
            Ok(()) => n += 1,
            Err(e) => errs.push(format!("{p}: {e}")),
        }
    }
    send(
        evt,
        Event::OpDone {
            ok: errs.is_empty(),
            message: if errs.is_empty() {
                tf("deleted {n} items", &[("n", &n.to_string())])
            } else {
                format!(
                    "{}: {}",
                    tf("deleted {n}; {f} failed", &[
                        ("n", &n.to_string()),
                        ("f", &errs.len().to_string()),
                    ]),
                    errs.join("; ")
                )
            },
        },
    );
}

async fn do_rename(
    c: &mut AsyncFtpStream,
    src: &str,
    new_name: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    // 目标为源所在目录 + 新名
    let dst = match src.rfind('/') {
        Some(0) => format!("/{new_name}"),
        Some(i) => format!("{}/{new_name}", &src[..i]),
        None => new_name.to_string(),
    };
    match c.rename(src, dst.as_str()).await {
        Ok(()) => send(
            evt,
            Event::OpDone {
                ok: true,
                message: tf("renamed to {d}", &[("d", &dst)]),
            },
        ),
        Err(e) => send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("{}: {e}", t("rename failed")),
            },
        ),
    }
}

/// 站点 → FTP 连接参数。
pub fn params_from_site(site: &crate::sites::Site) -> FtpParams {
    FtpParams {
        host: if site.host.trim().is_empty() {
            "127.0.0.1".into()
        } else {
            site.host.clone()
        },
        port: site.port,
        user: site.user.clone(),
        pass: site.password_plain(),
        initial_dir: site.last_remote_dir.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mlsd_dir_and_file() {
        let d = parse_list_line("type=dir;modify=20260115103000;perm=el; dirname")
            .expect("mlsd dir");
        assert_eq!(d.name, "dirname");
        assert!(d.is_dir);

        let f = parse_list_line("type=file;size=12345;modify=20260115103000; a.txt")
            .expect("mlsd file");
        assert_eq!(f.name, "a.txt");
        assert!(!f.is_dir);
        assert_eq!(f.size, 12345);
        assert!(f.mtime > 0, "modify should parse to epoch");
    }

    #[test]
    fn parse_mlsd_cdir_is_treated_as_dir() {
        let d = parse_list_line("type=cdir;modify=20260115103000; .").expect("cdir");
        // "." 本身被前面的过滤剔除，这里直接验证 cdir 判定逻辑
        let d2 = parse_list_line("type=cdir;modify=20260115103000; sub").expect("cdir named");
        assert!(d2.is_dir, "cdir should be treated as directory");
        assert_eq!(d2.name, "sub");
        let _ = d;
    }

    #[test]
    fn parse_unix_ls_fallback() {
        let l = parse_list_line("-rw-r--r-- 1 root root 4096 Jan 15 10:30 file.log")
            .expect("ls file");
        assert_eq!(l.name, "file.log");
        assert!(!l.is_dir);
        assert_eq!(l.size, 4096);

        let d = parse_list_line("drwxr-xr-x 2 root root 4096 Jan 15 10:30 sub")
            .expect("ls dir");
        assert_eq!(d.name, "sub");
        assert!(d.is_dir);
    }

    #[test]
    fn parse_skips_dot_entries() {
        assert!(parse_list_line(".").is_none());
        assert!(parse_list_line("..").is_none());
        assert!(parse_list_line("   ").is_none());
    }

    /// UNIX ls 行中名字含空格。
    #[test]
    fn parse_name_with_spaces() {
        let l = parse_list_line("-rw-r--r-- 1 r g 10 Jan  1 10:00 my file.txt")
            .expect("spaced name");
        assert_eq!(l.name, "my file.txt");
    }

    /// FTP 时间解析：2026-01-15 10:30:00 UTC → 1768473000。
    #[test]
    fn ftp_time_parsing() {
        assert_eq!(parse_ftp_time("20260115103000"), 1_768_473_000);
        assert_eq!(parse_ftp_time("19700101000000"), 0);
        assert_eq!(parse_ftp_time("bad"), 0);
        assert_eq!(parse_ftp_time("2026"), 0);
    }

    /// MLSD 八进制转义名还原（`\040`=空格、`\\`=反斜杠）。
    #[test]
    fn unescape_mlsd_name() {
        assert_eq!(unescape_ftp_name("my\\040file.txt"), "my file.txt");
        assert_eq!(unescape_ftp_name("a\\011b"), "a\tb");
        assert_eq!(unescape_ftp_name("back\\\\slash"), "back\\slash");
        assert_eq!(unescape_ftp_name("plain.txt"), "plain.txt");
        // 结尾悬空反斜杠不应 panic
        assert_eq!(unescape_ftp_name("trail\\"), "trail");
    }

    /// 站点 → FTP 参数映射（含空主机兜底）。
    #[test]
    fn params_from_site_mapping() {
        use crate::sites::{obfuscate_for_storage, Site};
        let s = Site {
            name: "ftp1".into(),
            protocol: crate::sites::Protocol::Ftp,
            host: "10.0.0.9".into(),
            port: 2121,
            user: "u".into(),
            password: obfuscate_for_storage("pw"),
            last_remote_dir: "/pub".into(),
            ..Default::default()
        };
        let p = params_from_site(&s);
        assert_eq!(p.host, "10.0.0.9");
        assert_eq!(p.port, 2121);
        assert_eq!(p.user, "u");
        assert_eq!(p.pass, "pw", "password must be deobfuscated");
        assert_eq!(p.initial_dir, "/pub");

        // 空主机兜底为 127.0.0.1
        let mut blank = s.clone();
        blank.host = "  ".into();
        assert_eq!(params_from_site(&blank).host, "127.0.0.1");
    }
}
