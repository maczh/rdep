//! SFTP/SSH 协议后端（russh 纯 Rust SSH 栈 + russh-sftp）。
//!
//! ## 能力（对齐 GUI 需求）
//!
//! 浏览 / 上传 / 下载 / 新建目录 / 删除（递归）/ 改名 / **目录同步** /
//! **断点续传** / **tail（可跟随）** / **grep** / 远端编辑。
//!
//! 其中 tail 与 grep 通过 SSH **exec 通道**执行远端 `tail` / `grep` 命令实现
//! （命令字符串经 shell 单引号转义，防止注入）；其余通过 SFTP 子系统完成。
//!
//! ## 断点续传
//!
//! 与 FTP/rdep-客户端行为一致：传输先落到 `<目标>.rdep-part` 半成品文件，
//! 重试时按已有长度 seek 续传（上传按远端 part 大小 seek 本地文件；
//! 下载按本地 part 大小 seek 远端文件），成功后原子改名。本地文件被截短
//! （part 比源文件还大）时自动从头重传，避免拼出损坏文件。
//!
//! ## 运行时模型
//!
//! 与 `ftp.rs` 同构：GUI 线程持有 `SftpClient` 句柄，专用线程内跑一个
//! **tokio current-thread runtime** 顺序执行指令，事件经 `std::sync::mpsc`
//! 回传 GUI（复用 `client::Event`，GUI 面板无需区分协议）。tail-follow 是
//! 例外：作为独立 tokio task 运行，通过 `watch` 通道接收停止信号。
//!
//! ## 主机密钥（TOFU + 指纹记录）
//!
//! 首次连接记录服务器公钥指纹（SHA256）到 `rdep/known_hosts.json`；
//! 后续连接严格比对，**不一致立即拒绝**（防中间人）。这是 TOFU（Trust On
//! First Use）模型：首次连接本身无法防 MITM，但换用固定指纹文件后即可
//! 持续保护。拒绝时 GUI 日志会展示指纹，用户可核对后手工删除指纹文件重置。
//!
//! ## 与 rdep 协议的能力边界
//!
//! 发布/回滚/重启脚本是 rdep service 的自有语义，SFTP 不提供
//! （GUI 侧按 `Protocol::supports_publish` 显式禁用，不静默失败）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rdep_protocol::{Direction, FileEntry};
use russh::ChannelMsg;
use russh::client::{self, Handle};
use russh::keys::PublicKeyOrCertificate;
use russh::keys::HashAlg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc as async_mpsc;
use tokio::sync::watch;

use crate::client::Event;
use crate::i18n::{t, tf};
use crate::sites::Site;

/// 缓冲大小（与 ftp.rs 一致）。
const BUF: usize = 256 * 1024;
/// 连接 / 认证总超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 同步时允许的 mtime 秒级容差（SFTP u32 秒 vs 本地纳秒精度，±2s 内视为未变）。
const MTIME_TOLERANCE: i64 = 2;

static SFTP_ID: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
    SFTP_ID.fetch_add(1, Ordering::Relaxed)
}

fn send(evt: &std_mpsc::Sender<Event>, e: Event) {
    // 事件统一出口：SFTP 后端所有回传都留痕（排查"操作没反应"的关键证据）
    tracing::debug!(event = ?e, "sftp event -> gui");
    let _ = evt.send(e);
}

/// SFTP 连接参数。
#[derive(Clone)]
pub struct SftpParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    /// 登录后进入的初始目录（空 = 服务器默认）。
    pub initial_dir: String,
}

/// 自定义 Debug：口令在日志中必须脱敏（`SftpCommand::Connect` 会整包打印）。
impl std::fmt::Debug for SftpParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("pass", &"***")
            .field("initial_dir", &self.initial_dir)
            .finish()
    }
}

/// GUI → SFTP 后台线程的指令。
#[derive(Debug)]
pub enum SftpCommand {
    Connect(SftpParams),
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
    SyncDir {
        local_dir: String,
        remote_dir: String,
        delete_extra: bool,
        dry_run: bool,
    },
    Tail {
        path: String,
        lines: u32,
        follow: bool,
    },
    StopTail,
    Grep {
        path: String,
        pattern: String,
        flags: String,
    },
    EditGet {
        remote_path: String,
    },
    EditSave {
        remote_path: String,
        content: String,
    },
}

/// SFTP 客户端句柄（与 `Client`/`FtpClient` 同构）。
pub struct SftpClient {
    cmd_tx: async_mpsc::Sender<SftpCommand>,
    evt_rx: std_mpsc::Receiver<Event>,
}

impl SftpClient {
    pub fn new() -> Self {
        let (cmd_tx, mut cmd_rx) = async_mpsc::channel::<SftpCommand>(64);
        let (evt_tx, evt_rx) = std_mpsc::channel::<Event>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sftp backend: build tokio runtime");
            rt.block_on(async move {
                let mut sess: Option<Sess> = None;
                // 注意：此处已在 tokio runtime 内，必须用 recv().await；
                // 用 blocking_recv() 会 panic（"Cannot block the current thread
                // from within a runtime"），且后台线程 panic 是静默的——
                // 表现为所有 SFTP 操作"没有任何反应"。
                while let Some(cmd) = cmd_rx.recv().await {
                    tracing::debug!(?cmd, "sftp command <- gui");
                    match cmd {
                        SftpCommand::Connect(p) => {
                            send(
                                &evt_tx,
                                Event::Status(tf("SFTP connecting {host}:{port} ...", &[
                                    ("host", &p.host),
                                    ("port", &p.port.to_string()),
                                ])),
                            );
                            match do_connect(&p).await {
                                Ok(s) => {
                                    sess = Some(s);
                                    send(&evt_tx, Event::Connected);
                                    send(&evt_tx, Event::Status(t("SFTP connected").to_string()));
                                }
                                Err(e) => {
                                    sess = None;
                                    send(
                                        &evt_tx,
                                        Event::Error(format!(
                                            "{}: {e:#}",
                                            t("SFTP connect failed")
                                        )),
                                    );
                                    send(&evt_tx, Event::Disconnected);
                                }
                            }
                        }
                        SftpCommand::Disconnect => {
                            if let Some(s) = sess.take() {
                                s.shutdown().await;
                            }
                            send(&evt_tx, Event::Disconnected);
                        }
                        SftpCommand::Ls(path) => match sess.as_mut() {
                            Some(s) => do_ls(s, &path, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::Mkdir(paths) => match sess.as_mut() {
                            Some(s) => do_mkdir(s, &paths, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::Upload {
                            remote_path,
                            local_path,
                        } => match sess.as_mut() {
                            Some(s) => {
                                upload_one(s, &remote_path, &local_path, &evt_tx).await;
                            }
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::Download {
                            remote_path,
                            local_path,
                        } => match sess.as_mut() {
                            Some(s) => {
                                download_one(s, &remote_path, &local_path, &evt_tx).await;
                            }
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::Delete(paths) => match sess.as_mut() {
                            Some(s) => do_delete(s, &paths, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::Rename { src, new_name } => match sess.as_mut() {
                            Some(s) => do_rename(s, &src, &new_name, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::SyncDir {
                            local_dir,
                            remote_dir,
                            delete_extra,
                            dry_run,
                        } => match sess.as_mut() {
                            Some(s) => {
                                do_sync(s, &local_dir, &remote_dir, delete_extra, dry_run, &evt_tx)
                                    .await;
                            }
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::Tail {
                            path,
                            lines,
                            follow,
                        } => match sess.as_mut() {
                            Some(s) => do_tail(s, &path, lines, follow, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::StopTail => {
                            if let Some(s) = sess.as_mut() {
                                s.stop_tail();
                            }
                        }
                        SftpCommand::Grep {
                            path,
                            pattern,
                            flags,
                        } => match sess.as_mut() {
                            Some(s) => do_grep(s, &path, &pattern, &flags, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::EditGet { remote_path } => match sess.as_mut() {
                            Some(s) => do_edit_get(s, &remote_path, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                        SftpCommand::EditSave {
                            remote_path,
                            content,
                        } => match sess.as_mut() {
                            Some(s) => do_edit_save(s, &remote_path, &content, &evt_tx).await,
                            None => send(&evt_tx, Event::Error(t("not connected (SFTP)").to_string())),
                        },
                    }
                }
            });
        });
        SftpClient { cmd_tx, evt_rx }
    }

    pub fn connect(&self, p: SftpParams) {
        tracing::debug!(host = %p.host, port = p.port, user = %p.user, pass_len = p.pass.len(), "sftp api: connect");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Connect(p));
    }
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.blocking_send(SftpCommand::Disconnect);
    }
    pub fn ls(&self, path: &str) {
        tracing::debug!(path, "sftp api: ls");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Ls(path.to_string()));
    }
    pub fn mkdir(&self, paths: Vec<String>) {
        tracing::debug!(?paths, "sftp api: mkdir");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Mkdir(paths));
    }
    pub fn upload(&self, remote_path: String, local_path: String) {
        tracing::debug!(local = %local_path, remote = %remote_path, "sftp api: upload");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Upload {
            remote_path,
            local_path,
        });
    }
    pub fn download(&self, remote_path: String, local_path: String) {
        tracing::debug!(remote = %remote_path, local = %local_path, "sftp api: download");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Download {
            remote_path,
            local_path,
        });
    }
    pub fn delete(&self, paths: Vec<String>) {
        tracing::debug!(?paths, "sftp api: delete");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Delete(paths));
    }
    pub fn rename(&self, src: String, new_name: String) {
        tracing::debug!(src = %src, new_name = %new_name, "sftp api: rename");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Rename { src, new_name });
    }
    pub fn sync_dir(&self, local_dir: String, remote_dir: String, delete_extra: bool, dry_run: bool) {
        tracing::debug!(local = %local_dir, remote = %remote_dir, delete_extra, dry_run, "sftp api: sync_dir");
        let _ = self.cmd_tx.blocking_send(SftpCommand::SyncDir {
            local_dir,
            remote_dir,
            delete_extra,
            dry_run,
        });
    }
    pub fn tail(&self, path: String, lines: u32, follow: bool) {
        tracing::debug!(path = %path, lines, follow, "sftp api: tail");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Tail { path, lines, follow });
    }
    /// 与 `Client::request_stop_tail` 同语义（GUI 按钮直接可换）。
    pub fn request_stop_tail(&self) {
        let _ = self.cmd_tx.blocking_send(SftpCommand::StopTail);
    }
    pub fn grep(&self, path: String, pattern: String, flags: String) {
        tracing::debug!(path = %path, pattern = %pattern, flags = %flags, "sftp api: grep");
        let _ = self.cmd_tx.blocking_send(SftpCommand::Grep { path, pattern, flags });
    }
    pub fn edit_get(&self, remote_path: String) {
        tracing::debug!(path = %remote_path, "sftp api: edit_get");
        let _ = self.cmd_tx.blocking_send(SftpCommand::EditGet { remote_path });
    }
    pub fn edit_save(&self, remote_path: String, content: String) {
        tracing::debug!(path = %remote_path, bytes = content.len(), "sftp api: edit_save");
        let _ = self
            .cmd_tx
            .blocking_send(SftpCommand::EditSave { remote_path, content });
    }

    /// 非阻塞取一个事件（GUI 每帧调用）。
    pub fn next_event(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }

    /// 阻塞等待一个事件（测试用）。
    pub fn recv_timeout(&self, d: Duration) -> Option<Event> {
        self.evt_rx.recv_timeout(d).ok()
    }
}

// ===========================================================================
// 连接：主机密钥（TOFU） + 口令认证 + SFTP 子系统
// ===========================================================================

/// 一条已建立的 SFTP 会话。
struct Sess {
    session: Handle<HostKeyGate>,
    sftp: SftpSession,
    /// 当前 follow-tail 的停止信号（None = 无进行中的 tail）。
    tail_stop: Option<watch::Sender<bool>>,
}

impl Sess {
    fn stop_tail(&mut self) {
        if let Some(tx) = self.tail_stop.take() {
            let _ = tx.send(true);
        }
    }

    async fn shutdown(mut self) {
        self.stop_tail();
        let _ = self
            .session
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
    }
}

/// 主机密钥校验状态（Handler 与连接流程共享）。
#[derive(Default)]
struct GateShared {
    /// 已知指纹（来自 known_hosts.json；None = 首次见到该主机）。
    expected: Option<String>,
    /// 本次连接实际呈现的指纹（供连接流程记录/展示）。
    seen: Option<String>,
}

/// `check_server_key` 实现：TOFU（首次记录，其后严格比对）。
struct HostKeyGate {
    shared: std::sync::Arc<Mutex<GateShared>>,
}

impl client::Handler for HostKeyGate {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = fingerprint_of(server_public_key);
        let mut g = self.shared.lock().map_err(|_| russh::Error::UnknownKey)?;
        g.seen = Some(fp.clone());
        Ok(g.expected.as_deref() == Some(fp.as_str()) || g.expected.is_none())
    }
}

/// 计算服务器公钥的 SHA256 指纹（`SHA256:…`，与 `ssh-keygen -lf` 输出同格式）。
fn fingerprint_of(key: &PublicKeyOrCertificate) -> String {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => {
            key.fingerprint(HashAlg::Sha256).to_string()
        }
        PublicKeyOrCertificate::Certificate(c) => {
            c.public_key().fingerprint(HashAlg::Sha256).to_string()
        }
    }
}

/// 已知主机指纹库（`rdep/known_hosts.json`，`host:port → SHA256:…`）。
pub struct KnownHosts {
    path: PathBuf,
    map: BTreeMap<String, String>,
}

impl KnownHosts {
    pub fn load_default() -> Self {
        let path = crate::sites::config_base_dir()
            .join("rdep")
            .join("known_hosts.json");
        Self::with_path(path)
    }

    pub fn with_path(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<BTreeMap<String, String>>(&raw).ok())
            .unwrap_or_default();
        Self { path, map }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.map.get(key)
    }

    /// 记录并持久化新指纹（原子写）。
    pub fn insert_and_save(&mut self, key: String, fp: String) -> Result<()> {
        self.map.insert(key, fp);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_string_pretty(&self.map)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

async fn do_connect(p: &SftpParams) -> Result<Sess> {
    let hostport = format!("{}:{}", p.host, p.port);
    let mut known = KnownHosts::load_default();
    let shared = std::sync::Arc::new(Mutex::new(GateShared {
        expected: known.get(&hostport).cloned(),
        seen: None,
    }));

    let config = std::sync::Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        nodelay: true,
        ..client::Config::default()
    });

    let gate = HostKeyGate {
        shared: shared.clone(),
    };
    let connect_fut = client::connect(config, (p.host.as_str(), p.port), gate);
    let mut session: Handle<HostKeyGate> = tokio::time::timeout(CONNECT_TIMEOUT, connect_fut)
        .await
        .map_err(|_| anyhow!("connect timeout ({}s)", CONNECT_TIMEOUT.as_secs()))?
        .context("ssh connect")?;

    // 指纹结果：被拒（mismatch）或首次记录
    let seen = shared
        .lock()
        .map(|g| g.seen.clone())
        .unwrap_or_default()
        .unwrap_or_default();
    let known_fp = known.get(&hostport).cloned();
    if known_fp.is_some() && known_fp.as_deref() != Some(seen.as_str()) {
        bail!(
            "{}\nexpected: {}\nserver:    {}",
            t("HOST KEY MISMATCH (possible MITM); connection rejected"),
            known_fp.unwrap_or_default(),
            seen
        );
    }
    let new_host_recorded = known_fp.is_none();
    if new_host_recorded {
        known
            .insert_and_save(hostport.clone(), seen.clone())
            .with_context(|| format!("save known host {}", hostport))?;
    }

    let auth = tokio::time::timeout(CONNECT_TIMEOUT, session.authenticate_password(&p.user, &p.pass))
        .await
        .map_err(|_| anyhow!("auth timeout"))?
        .context("ssh auth")?;
    if !auth.success() {
        bail!("{}", t("SFTP auth failed"));
    }

    let channel = session
        .channel_open_session()
        .await
        .context("open ssh channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("start sftp session")?;

    let _ = new_host_recorded; // 首次记录提示由 GUI 在连接成功日志中呈现
    let _ = &p.initial_dir; // 初始目录由 GUI 在连接成功后 cd（与 FTP 一致）
    Ok(Sess {
        session,
        sftp,
        tail_stop: None,
    })
}

// ===========================================================================
// 基础文件操作
// ===========================================================================

async fn do_ls(s: &mut Sess, path: &str, evt: &std_mpsc::Sender<Event>) {
    let rd = match s.sftp.read_dir(path).await {
        Ok(rd) => rd,
        Err(e) => {
            send(
                evt,
                Event::Error(format!("{}: {e}", t("SFTP list failed"))),
            );
            return;
        }
    };
    let mut entries: Vec<FileEntry> = Vec::new();
    for e in rd {
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let m = e.metadata();
        entries.push(FileEntry {
            is_dir: e.file_type().is_dir(),
            name,
            size: m.size.unwrap_or(0),
            mtime: m.mtime.map(|v| i64::from(v)).unwrap_or(0),
            mode: m.permissions.map(|p| p & 0o777).unwrap_or(0),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    send(
        evt,
        Event::DirListed {
            path: path.to_string(),
            entries,
        },
    );
}

async fn do_mkdir(s: &mut Sess, paths: &[String], evt: &std_mpsc::Sender<Event>) {
    let mut errs = Vec::new();
    for p in paths {
        if let Err(e) = s.sftp.create_dir(p.as_str()).await {
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

/// 递归删除（目录/文件均可）。
async fn rm_all(sftp: &SftpSession, path: &str) -> Result<()> {
    let m = sftp.metadata(path).await?;
    if m.is_dir() {
        let rd = sftp.read_dir(path).await?;
        for e in rd {
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = join_remote(path, &name);
            Box::pin(rm_all(sftp, &child)).await?;
        }
        sftp.remove_dir(path).await?;
    } else {
        sftp.remove_file(path).await?;
    }
    Ok(())
}

async fn do_delete(s: &mut Sess, paths: &[String], evt: &std_mpsc::Sender<Event>) {
    let mut errs = Vec::new();
    let mut n = 0usize;
    for p in paths {
        match rm_all(&s.sftp, p).await {
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
                tf("deleted {n}; {f} failed", &[
                    ("n", &n.to_string()),
                    ("f", &errs.len().to_string()),
                ]) + &format!(": {}", errs.join("; "))
            },
        },
    );
}

async fn do_rename(s: &mut Sess, src: &str, new_name: &str, evt: &std_mpsc::Sender<Event>) {
    let dst = match src.rfind('/') {
        Some(0) => format!("/{new_name}"),
        Some(i) => format!("{}/{new_name}", &src[..i]),
        None => new_name.to_string(),
    };
    // SFTP 的 SSH_FXP_RENAME 在目标已存在时多数服务器报错：先探在再删目标
    let exists = s.sftp.try_exists(&dst).await.unwrap_or(false);
    if exists {
        let _ = s.sftp.remove_file(&dst).await;
    }
    match s.sftp.rename(src, &dst).await {
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
                message: format!("{}: {e}", t("SFTP rename failed")),
            },
        ),
    }
}

/// 拼接远端路径（与 app.rs 的 `join_remote` 同语义）。
fn join_remote(base: &str, name: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.is_empty() {
        format!("/{name}")
    } else {
        format!("{b}/{name}")
    }
}

// ===========================================================================
// 上传 / 下载（断点续传）
// ===========================================================================

/// 上传单个文件：`.rdep-part` 半成品 + 断点续传 + 完成后原子改名。
async fn upload_one(
    s: &mut Sess,
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
                message: m,
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
    let part = format!("{remote_path}.rdep-part");

    // 续传点：远端 part 已有长度（比源文件还长 → 本地文件被截短，从头重传）
    let mut start: u64 = 0;
    if let Ok(m) = s.sftp.metadata(&part).await {
        let have = m.size.unwrap_or(0);
        if have <= total {
            start = have;
            if have > 0 {
                send(
                    evt,
                    Event::Status(tf("resume from {n} bytes", &[("n", &have.to_string())])),
                );
            }
        } else {
            send(evt, Event::Status(t("local file shrunk; restart from scratch").to_string()));
        }
    }

    let mut local = match tokio::fs::File::open(local_path).await {
        Ok(f) => f,
        Err(e) => {
            fail(format!("{}: {e}", t("failed to open local file")));
            return;
        }
    };
    if let Err(e) = local.seek(std::io::SeekFrom::Start(start)).await {
        fail(format!("{}: {e}", t("failed to open local file")));
        return;
    }

    let flags = if start > 0 {
        OpenFlags::WRITE | OpenFlags::CREATE
    } else {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
    };
    let mut remote = match s.sftp.open_with_flags(&part, flags).await {
        Ok(f) => f,
        Err(e) => {
            fail(format!("{}: {e}", t("SFTP upload failed")));
            return;
        }
    };
    if start > 0 {
        // 非追加模式需要显式 seek 到续传点
        if let Err(e) = remote.seek(std::io::SeekFrom::Start(start)).await {
            fail(format!("{}: {e}", t("SFTP upload failed")));
            return;
        }
    }

    let mut buf = vec![0u8; BUF];
    let mut sent = start;
    loop {
        let n = match local.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                fail(format!("{}: {e}", t("failed to read local file")));
                return;
            }
        };
        if let Err(e) = remote.write_all(&buf[..n]).await {
            fail(format!("{}: {e}", t("SFTP upload failed")));
            return;
        }
        sent += n as u64;
        if sent % (BUF as u64 * 8) < n as u64 {
            send(
                evt,
                Event::TransferProgress { id, sent, total },
            );
        }
    }
    if let Err(e) = remote.flush().await {
        fail(format!("{}: {e}", t("SFTP upload failed")));
        return;
    }
    let _ = remote.sync_all().await;
    drop(remote);

    // 收尾：目标已存在则先移除（SFTP 原生 rename 不保证可覆盖），再原子改名
    if let Ok(true) = s.sftp.try_exists(remote_path).await {
        let _ = s.sftp.remove_file(remote_path).await;
    }
    if let Err(e) = s.sftp.rename(&part, remote_path).await {
        fail(format!("{}: {e}", t("SFTP upload failed")));
        return;
    }

    send(evt, Event::TransferProgress { id, sent: total, total });
    send(
        evt,
        Event::TransferDone {
            id,
            ok: true,
            message: String::new(),
        },
    );
}

/// 下载单个文件：本地 `.rdep-part` 半成品 + 断点续传 + 原子改名。
async fn download_one(
    s: &mut Sess,
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
                message: m,
            },
        );
    };

    let part = format!("{local_path}.rdep-part");
    let mut remote = match s.sftp.open(remote_path).await {
        Ok(f) => f,
        Err(e) => {
            fail(format!("{}: {e}", t("SFTP download failed")));
            return;
        }
    };
    let total = remote
        .metadata()
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);

    // 续传点：本地 part 已有长度（比远端还大 → 远端文件被替换，从头重传）
    let mut start: u64 = 0;
    if let Ok(m) = std::fs::metadata(&part) {
        let have = m.len();
        if total == 0 || have <= total {
            start = have;
            if have > 0 {
                send(
                    evt,
                    Event::Status(tf("resume from {n} bytes", &[("n", &have.to_string())])),
                );
            }
        } else {
            send(evt, Event::Status(t("local file shrunk; restart from scratch").to_string()));
        }
    }
    if let Err(e) = remote.seek(std::io::SeekFrom::Start(start)).await {
        fail(format!("{}: {e}", t("SFTP download failed")));
        return;
    }

    let mut local = if start > 0 {
        match tokio::fs::OpenOptions::new().append(true).open(&part).await {
            Ok(f) => f,
            Err(e) => {
                fail(format!("{}: {e}", t("failed to create temp file")));
                return;
            }
        }
    } else {
        match tokio::fs::File::create(&part).await {
            Ok(f) => f,
            Err(e) => {
                fail(format!("{}: {e}", t("failed to create temp file")));
                return;
            }
        }
    };

    let mut buf = vec![0u8; BUF];
    let mut got = start;
    loop {
        let n = match remote.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                fail(format!("{}: {e}", t("SFTP download failed")));
                return;
            }
        };
        if let Err(e) = local.write_all(&buf[..n]).await {
            fail(format!("{}: {e}", t("failed to write local file")));
            return;
        }
        got += n as u64;
        if got % (BUF as u64 * 8) < n as u64 {
            send(
                evt,
                Event::TransferProgress { id, sent: got, total },
            );
        }
    }
    if let Err(e) = local.flush().await {
        fail(format!("{}: {e}", t("failed to flush local file")));
        return;
    }
    drop(local);

    if let Err(e) = std::fs::rename(&part, local_path) {
        fail(format!("{}: {e}", t("rename failed")));
        return;
    }
    send(evt, Event::TransferProgress { id, sent: got, total });
    send(
        evt,
        Event::TransferDone {
            id,
            ok: true,
            message: String::new(),
        },
    );
}

// ===========================================================================
// tail / grep（SSH exec 通道）
// ===========================================================================

/// shell 单引号转义：`'` → `'\''`，整体包一层单引号。
///
/// SSH exec 的命令串最终经远端登录 shell 解释，任何含空格/分号/通配符的
/// 参数都必须转义，否则既是 bug 也是注入面。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 把 GUI 的 flags 字符串（`i`/`n`…）映射为 grep 参数（始终递归）。
fn grep_args(flags: &str) -> String {
    let mut out = String::from("-r");
    for c in flags.chars() {
        match c {
            'i' => out.push_str(" -i"),
            'n' => out.push_str(" -n"),
            _ => {}
        }
    }
    out
}

/// 构建 tail 命令：`tail -n N [-F] -- 'path'`（-F 支持轮转重建的日志文件）。
fn tail_cmd(path: &str, lines: u32, follow: bool) -> String {
    format!(
        "tail -n {lines} {} -- {}",
        if follow { "-F" } else { "" },
        shell_quote(path)
    )
}

/// 构建 grep 命令。
fn grep_cmd(path: &str, pattern: &str, flags: &str) -> String {
    format!(
        "grep {} -e {} -- {}",
        grep_args(flags),
        shell_quote(pattern),
        shell_quote(path)
    )
}

async fn do_tail(
    s: &mut Sess,
    path: &str,
    lines: u32,
    follow: bool,
    evt: &std_mpsc::Sender<Event>,
) {
    s.stop_tail(); // 一次只允许一个 follow-tail
    let ch = match s.session.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            send(evt, Event::Error(format!("{}: {e}", t("SFTP tail failed"))));
            return;
        }
    };
    if let Err(e) = ch.exec(true, tail_cmd(path, lines, follow)).await {
        send(evt, Event::Error(format!("{}: {e}", t("SFTP tail failed"))));
        return;
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    s.tail_stop = Some(stop_tx);
    let evt2 = evt.clone();
    tokio::spawn(async move {
        tail_task(ch, stop_rx, evt2).await;
    });
}

async fn tail_task(
    mut ch: russh::Channel<russh::client::Msg>,
    mut stop: watch::Receiver<bool>,
    evt: std_mpsc::Sender<Event>,
) {
    let mut carry = String::new();
    let stopped;
    loop {
        tokio::select! {
            // 停止信号（GUI 点「停止跟随」或连接被关闭）
            changed = stop.changed() => {
                let should_stop = changed.is_err() || *stop.borrow();
                if should_stop {
                    stopped = true;
                    break;
                }
                // false→false 的虚假唤醒：继续等
            }
            m = ch.wait() => match m {
                Some(ChannelMsg::Data { data }) => {
                    carry.push_str(&String::from_utf8_lossy(&data));
                    while let Some(pos) = carry.find('\n') {
                        let line: String = carry.drain(..=pos).collect();
                        let line = line.trim_end_matches(['\n', '\r']).to_string();
                        send(&evt, Event::TailLine { line });
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // stderr 也进输出流（tail 报错可见，如权限不足）
                    carry.push_str(&String::from_utf8_lossy(&data));
                    while let Some(pos) = carry.find('\n') {
                        let line: String = carry.drain(..=pos).collect();
                        let line = line.trim_end_matches(['\n', '\r']).to_string();
                        send(&evt, Event::TailLine { line });
                    }
                }
                Some(ChannelMsg::ExitStatus { .. }) => { /* 等 Eof/Close */ }
                Some(ChannelMsg::Eof) => { /* 输出结束，等 Close */ }
                Some(ChannelMsg::Close) | None => {
                    stopped = false;
                    break;
                }
                _ => {}
            },
        }
    }
    let _ = ch.close().await;
    // 兜底：carry 里可能还有最后一行（无换行结尾）
    if !carry.trim().is_empty() {
        send(&evt, Event::TailLine { line: carry.trim().to_string() });
    }
    send(
        &evt,
        Event::TailDone {
            ok: true,
            message: if stopped {
                t("tail stopped").to_string()
            } else {
                String::new()
            },
        },
    );
}

async fn do_grep(
    s: &mut Sess,
    path: &str,
    pattern: &str,
    flags: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    let ch = match s.session.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            send(evt, Event::Error(format!("{}: {e}", t("SFTP grep failed"))));
            return;
        }
    };
    if let Err(e) = ch.exec(true, grep_cmd(path, pattern, flags)).await {
        send(evt, Event::Error(format!("{}: {e}", t("SFTP grep failed"))));
        return;
    }
    let mut out: Vec<u8> = Vec::new();
    let mut ch = ch;
    loop {
        match ch.wait().await {
            Some(ChannelMsg::Data { data }) => out.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => out.extend_from_slice(&data),
            Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::Eof) => continue,
            Some(ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    let lines: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect();
    send(
        evt,
        Event::OpDone {
            ok: true,
            message: tf("{n} match(es)", &[("n", &lines.len().to_string())]),
        },
    );
    send(evt, Event::GrepResult { lines });
}

// ===========================================================================
// 远端编辑
// ===========================================================================

async fn do_edit_get(s: &mut Sess, remote_path: &str, evt: &std_mpsc::Sender<Event>) {
    match s.sftp.read(remote_path).await {
        Ok(bytes) => send(
            evt,
            Event::EditLoaded {
                content: String::from_utf8_lossy(&bytes).into_owned(),
            },
        ),
        Err(e) => send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("{}: {e}", t("server error")),
            },
        ),
    }
}

/// 覆盖前把远端旧内容备份为 `<path>.rdep-bak-<ts>`（与 rdep service 行为对齐）。
async fn backup_remote(sftp: &SftpSession, path: &str) -> Option<String> {
    let exists = sftp.try_exists(path).await.unwrap_or(false);
    if !exists {
        return None;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dst = format!("{path}.rdep-bak-{ts}");
    sftp.rename(path, &dst).await.ok().map(|_| dst)
}

async fn do_edit_save(s: &mut Sess, remote_path: &str, content: &str, evt: &std_mpsc::Sender<Event>) {
    let backed = backup_remote(&s.sftp, remote_path).await;
    match s.sftp.write(remote_path, content.as_bytes()).await {
        Ok(()) => {
            let msg = match backed {
                Some(d) => tf("backed up to {d}", &[("d", &d)]),
                None => tf("renamed to {d}", &[("d", remote_path)]),
            };
            send(evt, Event::OpDone { ok: true, message: msg });
        }
        Err(e) => send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("{}: {e}", t("server error")),
            },
        ),
    }
}

// ===========================================================================
// 目录同步
// ===========================================================================

#[derive(Debug, Clone)]
struct LocalFile {
    rel: String,
    size: u64,
    mtime: i64,
}

#[derive(Debug, Clone)]
struct RemoteFile {
    rel: String,
    size: u64,
    mtime: i64,
}

/// 递归收集本地目录（限深 32，防符号链接环）。
fn walk_local(dir: &Path, prefix: &str, out: &mut Vec<LocalFile>) {
    if prefix.split('/').count() > 32 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if meta.is_dir() {
            walk_local(&entry.path(), &rel, out);
        } else if meta.is_file() {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(LocalFile {
                rel,
                size: meta.len(),
                mtime,
            });
        }
    }
}

/// 递归收集远端目录（SFTP readdir）。
fn walk_remote<'a>(
    sftp: &'a SftpSession,
    dir: &'a str,
    prefix: &'a str,
    files: &'a mut Vec<RemoteFile>,
    dirs: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
    let rd = sftp.read_dir(dir).await?;
    for e in rd {
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let full = join_remote(dir, &name);
        let ft = e.file_type();
        if ft.is_dir() {
            dirs.push(full.clone());
            walk_remote(sftp, &full, &rel, files, dirs).await?;
        } else {
            let m = e.metadata();
            files.push(RemoteFile {
                rel,
                size: m.size.unwrap_or(0),
                mtime: m.mtime.map(i64::from).unwrap_or(0),
            });
        }
    }
    Ok(())
})
}

async fn mkdirs_remote(sftp: &SftpSession, path: &str) {
    // 逐级 create_dir，已存在报错一律忽略（幂等）
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(seg);
        let _ = sftp.create_dir(&cur).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_sync(
    s: &mut Sess,
    local_dir: &str,
    remote_dir: &str,
    delete_extra: bool,
    dry_run: bool,
    evt: &std_mpsc::Sender<Event>,
) {
    let result: Result<()> = (async {
        // 本地清单
        let mut locals = Vec::new();
        walk_local(Path::new(local_dir), "", &mut locals);
        if locals.is_empty() {
            bail!(
                "{}: {local_dir}",
                t("local dir is empty or unreadable")
            );
        }

        // 远端清单
        let mut remotes: Vec<RemoteFile> = Vec::new();
        let mut remote_dirs: Vec<String> = Vec::new();
        walk_remote(&s.sftp, remote_dir, "", &mut remotes, &mut remote_dirs)
            .await
            .with_context(|| format!("{}: {remote_dir}", t("SFTP list failed")))?;

        // 比对：变更 = 远端缺失 或 (大小不同 或 mtime 差 > 容差)
        use std::collections::HashMap;
        let remote_map: HashMap<&str, &RemoteFile> =
            remotes.iter().map(|r| (r.rel.as_str(), r)).collect();
        let mut changed: Vec<&LocalFile> = Vec::new();
        for l in &locals {
            match remote_map.get(l.rel.as_str()) {
                None => changed.push(l),
                Some(r) => {
                    if r.size != l.size || (l.mtime - r.mtime).abs() > MTIME_TOLERANCE {
                        changed.push(l);
                    }
                }
            }
        }
        let local_rels: std::collections::HashSet<&str> =
            locals.iter().map(|l| l.rel.as_str()).collect();
        let to_delete: Vec<String> = remotes
            .iter()
            .filter(|r| !local_rels.contains(r.rel.as_str()))
            .map(|r| r.rel.clone())
            .collect();

        // 下发同步计划（与 rdep 协议一致的预览语义）
        send(
            evt,
            Event::SyncPreview {
                changed: changed.iter().map(|f| f.rel.clone()).collect(),
                to_delete: to_delete.clone(),
            },
        );
        send(
            evt,
            Event::Status(tf("sync: local {n} files: {u} to upload, {d} to delete", &[
                ("n", &locals.len().to_string()),
                ("u", &changed.len().to_string()),
                ("d", &to_delete.len().to_string()),
            ])),
        );
        if dry_run {
            send(
                evt,
                Event::OpDone {
                    ok: true,
                    message: format!(
                        "[dry-run] {}",
                        tf("sync: local {n} files: {u} to upload, {d} to delete", &[
                            ("n", &locals.len().to_string()),
                            ("u", &changed.len().to_string()),
                            ("d", &to_delete.len().to_string()),
                        ])
                    ),
                },
            );
            return Ok(());
        }

        // 上传变更文件（覆盖前备份旧文件）
        let mut uploaded = 0usize;
        for f in &changed {
            let remote_path = join_remote(remote_dir, &f.rel);
            if let Some(parent) = parent_of(&remote_path) {
                mkdirs_remote(&s.sftp, &parent).await;
            }
            backup_remote(&s.sftp, &remote_path).await;
            let local_path = Path::new(local_dir).join(&f.rel);
            upload_one(s, &remote_path, &local_path.to_string_lossy().as_ref(), evt).await;
            uploaded += 1;
        }

        // 删除远端多余文件（先文件后空目录，自底向上尝试删目录并忽略失败）
        let mut deleted = 0usize;
        if delete_extra {
            for rel in &to_delete {
                let p = join_remote(remote_dir, rel);
                if s.sftp.remove_file(&p).await.is_ok() {
                    deleted += 1;
                }
            }
            // 深度优先尝试删空目录（长的先删）
            let mut dirs = remote_dirs.clone();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.split('/').count()));
            for d in dirs {
                let _ = s.sftp.remove_dir(&d).await;
            }
        }

        let mut msg = tf("sync: uploaded {n} file(s)", &[("n", &uploaded.to_string())]);
        if delete_extra {
            msg.push_str(&format!(
                ", {}",
                tf("sync: deleted {n} file(s)", &[("n", &deleted.to_string())])
            ));
        }
        send(evt, Event::OpDone { ok: true, message: msg });
        Ok(())
    })
    .await;
    if let Err(e) = result {
        send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("{}: {e:#}", t("sync failed")),
            },
        );
    }
}

fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let idx = trimmed.rfind('/')?;
    Some(if idx == 0 {
        "/".to_string()
    } else {
        trimmed[..idx].to_string()
    })
}

/// 站点 → SFTP 连接参数。
pub fn params_from_site(site: &Site) -> SftpParams {
    SftpParams {
        host: if site.host.trim().is_empty() {
            "127.0.0.1".into()
        } else {
            site.host.clone()
        },
        port: if site.port == 0 { 22 } else { site.port },
        user: site.user.clone(),
        pass: site.password_plain(),
        initial_dir: site.last_remote_dir.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// shell 单引号转义：普通串、含空格、含单引号、含元字符。
    #[test]
    fn shell_quote_safety() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("my file.txt"), "'my file.txt'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        // 元字符应被单引号完整包裹：$ / ` / $( 在单引号内均不展开，安全
        let q = shell_quote("a;b|c$d`e`f$(g)");
        assert!(q.starts_with('\'') && q.ends_with('\''));
        let inner = q.trim_start_matches('\'').trim_end_matches('\'');
        assert!(inner.contains(";b|c"), "metachars must stay inside quotes: {q}");
    }

    /// tail/grep 命令拼接。
    #[test]
    fn cmd_builders() {
        assert_eq!(tail_cmd("/a b.log", 50, false), "tail -n 50  -- '/a b.log'");
        assert_eq!(tail_cmd("/a.log", 10, true), "tail -n 10 -F -- '/a.log'");
        let c = grep_cmd("/opt/app", "ERR (?i)x", "in");
        assert!(c.starts_with("grep -r -i -n -e "), "got: {c}");
        assert!(c.contains("'ERR (?i)x'"), "pattern must be quoted: {c}");
        assert!(c.ends_with("-- '/opt/app'"));
    }

    /// grep flags：未知字符忽略；空 flags 只保留 -r。
    #[test]
    fn grep_flags_mapping() {
        assert_eq!(grep_args(""), "-r");
        assert_eq!(grep_args("i"), "-r -i");
        assert_eq!(grep_args("ni"), "-r -n -i");
        assert_eq!(grep_args("xyz"), "-r", "unknown flags ignored");
    }

    /// 远端路径拼接（与 GUI 的 join_remote 语义一致）。
    #[test]
    fn remote_join() {
        assert_eq!(join_remote("/", "a"), "/a");
        assert_eq!(join_remote("/opt", "a"), "/opt/a");
        assert_eq!(join_remote("/opt/", "a"), "/opt/a");
    }

    /// parent_of：路径父目录解析。
    #[test]
    fn parent_paths() {
        assert_eq!(parent_of("/a/b/c.txt"), Some("/a/b".into()));
        assert_eq!(parent_of("/a"), Some("/".into()));
        assert_eq!(parent_of("noslash"), None);
    }

    /// 已知主机指纹库：读 / 写 / 原子覆盖。
    #[test]
    fn known_hosts_roundtrip() {
        let p = std::env::temp_dir().join(format!("rdep-kh-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut kh = KnownHosts::with_path(p.clone());
        assert!(kh.get("h:22").is_none());
        kh.insert_and_save("h:22".into(), "SHA256:abc".into()).unwrap();
        // 重新加载应读到
        let mut kh2 = KnownHosts::with_path(p.clone());
        assert_eq!(kh2.get("h:22").map(|s| s.as_str()), Some("SHA256:abc"));
        // 覆盖
        kh2.insert_and_save("h:22".into(), "SHA256:xyz".into()).unwrap();
        let kh3 = KnownHosts::with_path(p);
        assert_eq!(kh3.get("h:22").map(|s| s.as_str()), Some("SHA256:xyz"));
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("rdep-kh-{}.json", std::process::id())),
        );
    }

    /// params_from_site：空主机兜底、0 端口兜底 22、密码还原。
    #[test]
    fn params_mapping() {
        use crate::sites::{obfuscate_for_storage, Protocol};
        let s = Site {
            name: "s1".into(),
            protocol: Protocol::Sftp,
            host: "  ".into(),
            port: 0,
            user: "u".into(),
            password: obfuscate_for_storage("pw"),
            ..Default::default()
        };
        let p = params_from_site(&s);
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 22);
        assert_eq!(p.pass, "pw");
    }
}
