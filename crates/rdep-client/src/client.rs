//! rdep-client 网络核心：GUI 主线程通过 channel 下发指令，后台 Tokio 线程执行协议交互并回传事件。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rdep_protocol::transport::FrameCodec;
use rdep_protocol::relay::{RelayConnect, RelayConnectResp};
use rdep_protocol::{
    AuthMethod, AuthRequest, BackupsRequest, BackupsResponse, ChmodRequest, CmdRequest, CmdResponse,
    CmdType, Ctrl, DataChunk, DeleteRequest, Direction, DownloadRequest, DownloadResponse, EditRequest,
    FileEntry, Frame, FrameFlags, FrameType, GrepRequest, GrepResponse, LsRequest, LsResponse,
    MkdirRequest, MoveRequest, CopyPolicy, CopyRequest, NamePolicy, PublishCommitRequest,
    PublishItem, PublishRequest, RenameRequest, RollbackRequest, StreamPush, TailRequest,
    UploadCommit, UploadInit, UploadInitAck, sha256, CHUNK_SIZE_DEFAULT,
};
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc as async_mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

/// 发布清单中的单个文件：本地源路径 + 远端目标路径。
#[derive(Debug, Clone)]
pub struct PublishFile {
    pub remote_path: String,
    pub local_path: String,
}

/// GUI → 后台 的指令。
#[derive(Debug)]
pub enum Command {
    Connect(ConnectParams),
    Disconnect,
    Ls(String),
    Mkdir(Vec<String>),
    Upload { remote_path: String, local_path: String },
    Download { remote_path: String, local_path: String },
    Delete(Vec<String>),
    Rename { src: String, new_name: String },
    Move { src: Vec<String>, dst_dir: String },
    Copy { src: Vec<String>, dst: String, keep: bool },
    /// 发布：先备份、再逐文件上传，最后执行重启脚本。
    Publish {
        remote_dir: String,
        restart_script_id: String,
        /// 项目名：非空时由 service 从 projects 表解析部署配置（单一来源），
        /// `remote_dir` / `restart_script_id` 被忽略。
        project: Option<String>,
        files: Vec<PublishFile>,
    },
    /// 回滚：把某个备份版本恢复到远端目录。
    Rollback { remote_dir: String, version: String },
    /// 列出服务端备份版本（回滚下拉框数据源）。
    ///
    /// 备份库在 service 的私有工作目录（meta），**不在部署根之下**，
    /// 因此不能靠 `Ls /backup` 取（备份位置一变就失效），走专用指令。
    ListBackups,
    /// 实时查看日志尾部。
    Tail {
        path: String,
        lines: u32,
        follow: bool,
    },
    /// 在文件/目录中检索内容。
    Grep {
        path: String,
        pattern: String,
        flags: String,
    },
    /// 读取远端文件内容用于编辑。
    EditGet { remote_path: String },
    /// 保存编辑后的内容（服务端先备份再覆盖）。
    EditSave { remote_path: String, content: String },
    /// 目录同步：本地目录 → 远端目录。用「大小 + mtime」快筛判定变更（rsync 风格），
    /// 对变更文件先备份再覆盖（复用断点续传上传）；可选删除远端多余文件。
    SyncDir {
        local_dir: String,
        remote_dir: String,
        delete_extra: bool,
        dry_run: bool,
    },
    /// 远端 `chmod`：在 `path` 上设置权限位 `mode`（unix `st_mode & 0o777`）。
    Chmod { path: String, mode: u32 },
}

/// 后台 → GUI 的事件。
#[derive(Debug, Clone)]
pub enum Event {
    Status(String),
    Connected,
    Disconnected,
    Error(String),
    DirListed { path: String, entries: Vec<FileEntry> },
    TransferStarted { id: u64, name: String, direction: Direction },
    TransferProgress { id: u64, sent: u64, total: u64 },
    TransferDone { id: u64, ok: bool, message: String },
    OpDone { ok: bool, message: String },
    /// 发布收尾（重启脚本执行）的结果。
    PublishDone { ok: bool, message: String },
    /// tail 推送的一行日志。
    TailLine { line: String },
    /// tail 结束（follow 被停止或到达末尾）。
    TailDone { ok: bool, message: String },
    /// grep 命中的行集合。
    GrepResult { lines: Vec<String> },
    /// 目录同步计划：将被上传的变更文件、将被删除的远端多余文件（dry-run 与执行前都会发）。
    SyncPreview {
        changed: Vec<String>,
        to_delete: Vec<String>,
    },
    /// 备份版本列表（回滚下拉框数据源）。
    BackupVersions { versions: Vec<String> },
    /// edit 读取到的远端文件内容。
    EditLoaded { content: String },
}

// 注意：Debug 手动实现（下面），确保口令/令牌在日志中脱敏为 `***`。
#[derive(Clone)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 口令 / API 令牌明文。自定义 Debug：日志中必须脱敏为 `***`。
    pub pass: String,
    pub ca_cert: Option<PathBuf>,
    /// 中转模式：host/port 指向 forwarder，由它路由到目标 service。
    pub use_forwarder: bool,
    /// 中转时要访问的 service id（forwarder 用它寻址）。
    pub target_service_id: String,
    /// 中转密钥（与 forwarder 一致）。日志同样脱敏。
    pub relay_token: String,
    /// 用 **API 令牌** 而口令认证（CI/CD 场景）。
    /// 此时 `pass` 携带令牌明文，`user` 仅作标注。
    /// 显式开关而非按 `rdp_` 前缀推断——认证方式不该靠内容猜。
    pub use_token: bool,
}

impl std::fmt::Debug for ConnectParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("pass", &"***")
            .field("ca_cert", &self.ca_cert)
            .field("use_forwarder", &self.use_forwarder)
            .field("target_service_id", &self.target_service_id)
            .field("relay_token", &"***")
            .field("use_token", &self.use_token)
            .finish()
    }
}

struct Session {
    codec: FrameCodec<tokio_rustls::client::TlsStream<TcpStream>>,
    seq: u32,
}

static TRANSFER_ID: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
    TRANSFER_ID.fetch_add(1, Ordering::Relaxed)
}

/// 读取本地文件权限位（unix `mode & 0o777`；非 unix / 读取失败返回 0 表示不设置）。
#[cfg(unix)]
fn file_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}
#[cfg(not(unix))]
fn file_mode(_path: &std::path::Path) -> u32 {
    0
}

/// 读取本地文件 mtime（Unix 秒；读取失败返回 0 表示不设置）。
fn file_mtime(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把权限位 `mode` 应用到本地 `path`（仅 unix 有效；非 unix / mode==0 忽略）。
#[cfg(unix)]
fn apply_local_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn apply_local_mode(_path: &std::path::Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// 把 mtime `t` 应用到本地 `path`（t<=0 忽略）。本工具链 std 用 `set_times` 而非 `set_modified`。
fn apply_local_mtime(path: &std::path::Path, t: i64) -> std::io::Result<()> {
    if t <= 0 {
        return Ok(());
    }
    let sys = std::time::UNIX_EPOCH + std::time::Duration::from_secs(t as u64);
    std::fs::set_times(path, std::fs::FileTimes::new().set_modified(sys))
}

/// 流式统计文件：返回 `(size, sha256, total_chunks)`，不把整文件读进内存。
/// 用于在发送 UploadInit 前先知道总大小 / 整文件哈希 / 分片数。
fn file_digest(path: &std::path::Path) -> Result<(u64, [u8; 32], u32)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let size = f.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE_DEFAULT];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha: [u8; 32] = hasher.finalize().into();
    let cs = CHUNK_SIZE_DEFAULT as u64;
    let total = if size == 0 { 0 } else { ((size + cs - 1) / cs) as u32 };
    Ok((size, sha, total))
}

/// 流式发送分片：按 `CHUNK_SIZE_DEFAULT` 逐块读盘发送（跳过 `received` 中的片），
/// 内存占用恒定（与文件大小无关）。`on_progress` 以累计发送字节回调。
async fn send_file_chunks<F>(
    s: &mut Session,
    transfer_id: u64,
    path: &std::path::Path,
    total: u32,
    received: &std::collections::HashSet<u32>,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(u64),
{
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = vec![0u8; CHUNK_SIZE_DEFAULT];
    let mut sent = 0u64;
    for i in 0..total {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break; // 文件比预期短（不应发生，commit 时会因 sha 不符失败）
        }
        if !received.contains(&i) {
            let dc = DataChunk::new(transfer_id, i, buf[..n].to_vec());
            let frame = Frame::new(
                FrameType::DataChunk,
                FrameFlags::new(),
                postcard::to_allocvec(&dc)?,
            );
            s.codec.write_frame(&frame).await?;
        }
        sent += n as u64;
        on_progress(sent);
    }
    Ok(())
}

/// 由「远端路径 + 文件内容 sha」派生的稳定 transfer_id。
/// 同一文件同一内容的重试/续传复用同一 id，服务端据此匹配暂存区实现断点续传；
/// 内容变化则 id 变化，天然隔离。
fn stable_id(remote_path: &str, file_sha: &[u8; 32]) -> u64 {
    let mut buf = Vec::with_capacity(remote_path.len() + 32);
    buf.extend_from_slice(remote_path.as_bytes());
    buf.extend_from_slice(file_sha);
    let d = sha256(&buf);
    u64::from_be_bytes(d[..8].try_into().unwrap())
}

fn send(evt: &std_mpsc::Sender<Event>, e: Event) {
    // 事件统一出口：所有后台→GUI 事件都留痕（排查「界面没反应」的关键证据）
    tracing::debug!(event = ?e, "client event -> gui");
    let _ = evt.send(e);
}

/// 客户端句柄：在 GUI 线程持有，通过 channel 与后台通信。
pub struct Client {
    cmd_tx: async_mpsc::Sender<Command>,
    evt_rx: std_mpsc::Receiver<Event>,
    /// tail(follow) 的停止标志：GUI 置位后，后台 do_tail 读到即发 Ctrl::Stop 并收尾。
    tail_stop: Arc<AtomicBool>,
}

impl Client {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = async_mpsc::channel::<Command>(64);
        let (evt_tx, evt_rx) = std_mpsc::channel::<Event>();
        let tail_stop = Arc::new(AtomicBool::new(false));
        let ts = tail_stop.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            rt.block_on(client_task(cmd_rx, evt_tx, ts));
        });
        Client {
            cmd_tx,
            evt_rx,
            tail_stop,
        }
    }

    /// 请求停止正在进行的 tail(follow)。
    pub fn request_stop_tail(&self) {
        self.tail_stop.store(true, Ordering::SeqCst);
    }

    pub fn connect(&self, p: ConnectParams) {
        let _ = self.cmd_tx.blocking_send(Command::Connect(p));
    }
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.blocking_send(Command::Disconnect);
    }
    pub fn ls(&self, path: &str) {
        let _ = self.cmd_tx.blocking_send(Command::Ls(path.to_string()));
    }
    pub fn mkdir(&self, paths: Vec<String>) {
        let _ = self.cmd_tx.blocking_send(Command::Mkdir(paths));
    }
    pub fn upload(&self, remote_path: String, local_path: String) {
        let _ = self
            .cmd_tx
            .blocking_send(Command::Upload { remote_path, local_path });
    }
    pub fn download(&self, remote_path: String, local_path: String) {
        let _ = self
            .cmd_tx
            .blocking_send(Command::Download { remote_path, local_path });
    }
    pub fn delete(&self, paths: Vec<String>) {
        let _ = self.cmd_tx.blocking_send(Command::Delete(paths));
    }
    pub fn rename(&self, src: String, new_name: String) {
        let _ = self
            .cmd_tx
            .blocking_send(Command::Rename { src, new_name });
    }
    pub fn mv(&self, src: Vec<String>, dst_dir: String) {
        let _ = self.cmd_tx.blocking_send(Command::Move { src, dst_dir });
    }
    pub fn copy(&self, src: Vec<String>, dst: String, keep: bool) {
        let _ = self
            .cmd_tx
            .blocking_send(Command::Copy { src, dst, keep });
    }
    /// 发布：先备份旧文件，再上传 `files`，最后执行 `restart_script_id` 指定的脚本。
    pub fn publish(&self, remote_dir: String, restart_script_id: String, files: Vec<PublishFile>) {
        self.publish_with_project(remote_dir, restart_script_id, None, files);
    }

    /// 按**项目**发布：部署目录与重启脚本以 service 端 projects 表记录为准，
    /// 客户端无需（也无法）自报这两项。项目不存在时服务端会明确报错。
    pub fn publish_by_project(&self, project: String, files: Vec<PublishFile>) {
        self.publish_with_project(String::new(), String::new(), Some(project), files);
    }

    fn publish_with_project(
        &self,
        remote_dir: String,
        restart_script_id: String,
        project: Option<String>,
        files: Vec<PublishFile>,
    ) {
        let _ = self.cmd_tx.blocking_send(Command::Publish {
            remote_dir,
            restart_script_id,
            project,
            files,
        });
    }
    /// 回滚：把 `version` 备份恢复到 `remote_dir`。
    pub fn rollback(&self, remote_dir: String, version: String) {
        let _ = self
            .cmd_tx
            .blocking_send(Command::Rollback { remote_dir, version });
    }
    /// 列出备份版本（服务端返回后以 `Event::BackupVersions` 回传）。
    pub fn list_backups(&self) {
        let _ = self.cmd_tx.blocking_send(Command::ListBackups);
    }
    /// tail：查看文件尾部，`follow` 为真时持续跟随（用 request_stop_tail 停止）。
    pub fn tail(&self, path: String, lines: u32, follow: bool) {
        let _ = self
            .cmd_tx
            .blocking_send(Command::Tail { path, lines, follow });
    }
    /// grep：在文件/目录中检索，`flags` 支持 i(忽略大小写)/n(显示行号)。
    pub fn grep(&self, path: String, pattern: String, flags: String) {
        let _ = self.cmd_tx.blocking_send(Command::Grep {
            path,
            pattern,
            flags,
        });
    }
    /// edit：读取远端文件内容。
    pub fn edit_get(&self, remote_path: String) {
        let _ = self.cmd_tx.blocking_send(Command::EditGet { remote_path });
    }
    /// edit：保存内容（服务端先备份再覆盖）。
    pub fn edit_save(&self, remote_path: String, content: String) {
        let _ = self.cmd_tx.blocking_send(Command::EditSave {
            remote_path,
            content,
        });
    }
    /// 远端 `chmod`：在 `path` 上设置权限位 `mode`（unix `st_mode & 0o777`）。
    pub fn chmod(&self, path: String, mode: u32) {
        let _ = self.cmd_tx.blocking_send(Command::Chmod { path, mode });
    }
    /// 目录同步：本地目录 → 远端目录（按大小+mtime 判定变更，备份后覆盖）。
    /// `delete_extra` 删除远端多余文件；`dry_run` 仅统计。
    pub fn sync_dir(
        &self,
        local_dir: String,
        remote_dir: String,
        delete_extra: bool,
        dry_run: bool,
    ) {
        let _ = self.cmd_tx.blocking_send(Command::SyncDir {
            local_dir,
            remote_dir,
            delete_extra,
            dry_run,
        });
    }

    /// 非阻塞取一个事件（GUI 每帧调用）。
    pub fn next_event(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }
    /// 带超时的阻塞取事件（测试用）。
    pub fn recv_timeout(&self, d: Duration) -> Option<Event> {
        self.evt_rx.recv_timeout(d).ok()
    }
}

async fn client_task(
    mut cmd_rx: async_mpsc::Receiver<Command>,
    evt_tx: std_mpsc::Sender<Event>,
    tail_stop: Arc<AtomicBool>,
) {
    tracing::debug!("client background task started");
    let mut session: Option<Session> = None;
    // 记住上次成功连接的参数：连接掉线后据此透明重连（让断点续传可被 GUI 直接复用）。
    let mut conn_params: Option<ConnectParams> = None;
    while let Some(cmd) = cmd_rx.recv().await {
        // 指令统一入口：GUI 的每个按钮操作最终都会在这里留痕
        tracing::debug!(?cmd, "client command <- gui");
        // 对需要会话的指令：先确保有一条「活着」的连接（探活 + 必要时自动重连）。
        // 探活统一在此处做，所有操作共享；命令之间会话空闲，此时发 PING 安全。
        if needs_session(&cmd) {
            if let Some(s) = &mut session {
                if !probe_alive(&mut s.codec).await {
                    tracing::warn!("session probe failed, connection considered dead");
                    session = None;
                }
            }
            if session.is_none() {
                if let Some(p) = &conn_params {
                    tracing::info!(host = %p.host, port = p.port, "auto-reconnecting before command");
                    send(&evt_tx, Event::Status("reconnecting...".into()));
                    match do_connect(p).await {
                        Ok(s) => {
                            session = Some(s);
                            send(&evt_tx, Event::Status("reconnected".into()));
                        }
                        Err(e) => {
                            tracing::error!("reconnect failed: {e:#}");
                            send(&evt_tx, Event::Error(format!("reconnect failed: {e:#}")));
                            continue; // 无连接，跳过本条指令
                        }
                    }
                } else {
                    tracing::debug!("no session and no stored connection params for command");
                }
            }
        }
        match cmd {
            Command::Connect(p) => {
                send(&evt_tx, Event::Status("connecting...".into()));
                match do_connect(&p).await {
                    Ok(s) => {
                        session = Some(s);
                        conn_params = Some(p.clone());
                        send(&evt_tx, Event::Connected);
                        send(&evt_tx, Event::Status("connected".into()));
                    }
                    Err(e) => {
                        session = None;
                        conn_params = None;
                        send(&evt_tx, Event::Error(format!("connect failed: {e:#}")));
                        send(&evt_tx, Event::Disconnected);
                    }
                }
            }
            Command::Disconnect => {
                session = None;
                conn_params = None;
                send(&evt_tx, Event::Disconnected);
            }
            Command::Ls(path) => {
                if let Some(s) = &mut session {
                    do_ls(s, &path, &evt_tx).await;
                } else {
                    send(&evt_tx, Event::Error("not connected".into()));
                }
            }
            Command::Mkdir(paths) => match &mut session {
                Some(s) => do_simple(s, CmdType::Mkdir, &MkdirRequest { paths }, &evt_tx).await,
                None => send(&evt_tx, Event::OpDone { ok: false, message: "not connected".into() }),
            },
            Command::Delete(paths) => match &mut session {
                Some(s) => do_simple(s, CmdType::Delete, &DeleteRequest { paths }, &evt_tx).await,
                None => send(&evt_tx, Event::OpDone { ok: false, message: "not connected".into() }),
            },
            Command::Rename { src, new_name } => match &mut session {
                Some(s) => do_simple(
                    s,
                    CmdType::Rename,
                    &RenameRequest { src, new_name },
                    &evt_tx,
                )
                .await,
                None => send(&evt_tx, Event::OpDone { ok: false, message: "not connected".into() }),
            },
            Command::Move { src, dst_dir } => match &mut session {
                Some(s) => do_simple(s, CmdType::Move, &MoveRequest { src, dst_dir }, &evt_tx).await,
                None => send(&evt_tx, Event::OpDone { ok: false, message: "not connected".into() }),
            },
            Command::Copy { src, dst, keep } => match &mut session {
                Some(s) => do_simple(
                    s,
                    CmdType::Copy,
                    &CopyRequest {
                        src,
                        dst,
                        policy: if keep {
                            CopyPolicy::Keep
                        } else {
                            CopyPolicy::Rename
                        },
                    },
                    &evt_tx,
                )
                .await,
                None => send(&evt_tx, Event::OpDone { ok: false, message: "not connected".into() }),
            },
            Command::Upload {
                remote_path,
                local_path,
            } => match &mut session {
                Some(s) => do_upload(s, &remote_path, &local_path, &evt_tx).await,
                None => send(
                    &evt_tx,
                    Event::TransferDone { id: 0, ok: false, message: "not connected".into() },
                ),
            },
            Command::Download {
                remote_path,
                local_path,
            } => match &mut session {
                Some(s) => do_download(s, &remote_path, &local_path, &evt_tx).await,
                None => send(
                    &evt_tx,
                    Event::TransferDone { id: 0, ok: false, message: "not connected".into() },
                ),
            },
            Command::Publish {
                remote_dir,
                restart_script_id,
                project,
                files,
            } => {
                if let Some(s) = &mut session {
                    do_publish(s, &remote_dir, &restart_script_id, project.as_deref(), &files, &evt_tx).await;
                } else {
                    send(
                        &evt_tx,
                        Event::PublishDone {
                            ok: false,
                            message: "not connected".into(),
                        },
                    );
                }
            }
            Command::ListBackups => match &mut session {
                Some(s) => do_backups(s, &evt_tx).await,
                None => send(&evt_tx, Event::Error("not connected".into())),
            },
            Command::Rollback { remote_dir, version } => {
                if let Some(s) = &mut session {
                    do_rollback(s, &remote_dir, &version, &evt_tx).await;
                } else {
                    send(
                        &evt_tx,
                        Event::OpDone {
                            ok: false,
                            message: "not connected".into(),
                        },
                    );
                }
            }
            Command::Tail { path, lines, follow } => {
                if let Some(s) = &mut session {
                    do_tail(s, &path, lines, follow, &tail_stop, &evt_tx).await;
                } else {
                    send(
                        &evt_tx,
                        Event::TailDone {
                            ok: false,
                            message: "not connected".into(),
                        },
                    );
                }
            }
            Command::Grep {
                path,
                pattern,
                flags,
            } => {
                if let Some(s) = &mut session {
                    do_grep(s, &path, &pattern, &flags, &evt_tx).await;
                } else {
                    send(
                        &evt_tx,
                        Event::Error("not connected".into()),
                    );
                }
            }
            Command::EditGet { remote_path } => {
                if let Some(s) = &mut session {
                    do_edit_get(s, &remote_path, &evt_tx).await;
                } else {
                    send(&evt_tx, Event::Error("not connected".into()));
                }
            }
            Command::EditSave {
                remote_path,
                content,
            } => {
                if let Some(s) = &mut session {
                    do_edit_save(s, &remote_path, &content, &evt_tx).await;
                } else {
                    send(
                        &evt_tx,
                        Event::OpDone {
                            ok: false,
                            message: "not connected".into(),
                        },
                    );
                }
            }
            Command::SyncDir {
                local_dir,
                remote_dir,
                delete_extra,
                dry_run,
            } => {
                if let Some(s) = &mut session {
                    do_sync_dir(
                        s,
                        &local_dir,
                        &remote_dir,
                        delete_extra,
                        dry_run,
                        &evt_tx,
                    )
                    .await;
                } else {
                    send(
                        &evt_tx,
                        Event::OpDone {
                            ok: false,
                            message: "not connected".into(),
                        },
                    );
                }
            }
            Command::Chmod { path, mode } => match &mut session {
                Some(s) => do_simple(
                    s,
                    CmdType::Chmod,
                    &ChmodRequest { path, mode },
                    &evt_tx,
                )
                .await,
                None => send(&evt_tx, Event::OpDone { ok: false, message: "not connected".into() }),
            },
        }
    }
}

async fn do_connect(p: &ConnectParams) -> Result<Session> {
    tracing::debug!(params = ?p, "connect: begin");
    let server_name = ServerName::try_from(p.host.clone())
        .map_err(|_| anyhow::anyhow!("invalid server name: {}", p.host))?;

    let mut roots = RootCertStore::empty();
    if let Some(ca) = &p.ca_cert {
        let pem = std::fs::read(ca)
            .with_context(|| format!("read CA cert {}", ca.display()))?;
        let certs = rustls_pemfile::certs(&mut &pem[..])
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("parse CA cert")?;
        let n = certs.len();
        for c in certs {
            let _ = roots.add(c);
        }
        tracing::debug!(ca = %ca.display(), certs = n, "connect: CA loaded");
    }
    if roots.is_empty() {
        tracing::error!("connect: no CA cert configured (set ca_cert)");
        anyhow::bail!("no CA cert configured (set ca_cert)");
    }

    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(roots))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(cfg));

    tracing::debug!(host = %p.host, port = p.port, "connect: dialing tcp");
    let tcp = TcpStream::connect((p.host.as_str(), p.port))
        .await
        .context("tcp connect")?;
    tracing::debug!("connect: tcp ok, starting tls handshake");
    let tls = connector
        .connect(server_name, tcp)
        .await
        .context("tls handshake")?;
    tracing::debug!("connect: tls handshake ok");
    let mut codec = FrameCodec::new(tls);

    // 中转模式：先向 forwarder 做路由握手（第一帧 RelayConnect），成功后再跑 rdep 协议
    if p.use_forwarder {
        tracing::debug!(service_id = %p.target_service_id, "connect: relay handshake to forwarder");
        let body = postcard::to_allocvec(&RelayConnect {
            target_service_id: p.target_service_id.clone(),
            token: p.relay_token.clone(),
        })?;
        let frame = Frame::new(FrameType::CmdRequest, FrameFlags::new(), body);
        codec.write_frame(&frame).await?;
        let fr = codec
            .read_frame()
            .await?
            .context("forwarder closed before relay ack")?;
        let rr: RelayConnectResp = postcard::from_bytes(&fr.payload)?;
        tracing::debug!(ok = rr.ok, code = rr.code, message = %rr.message, "connect: relay ack");
        if !rr.ok {
            anyhow::bail!("relay connect failed (code {}): {}", rr.code, rr.message);
        }
    }

    tracing::debug!(user = %p.user, token_auth = p.use_token, "connect: sending auth");
    send_cmd(
        &mut codec,
        &mut 0u32,
        CmdType::Auth,
        &AuthRequest {
            user: p.user.clone(),
            pass: p.pass.clone(),
            method: if p.use_token {
                AuthMethod::Token
            } else {
                AuthMethod::Password
            },
        },
    )
    .await?;
    let resp = read_response(&mut codec).await?;
    tracing::debug!(ok = resp.ok, message = %resp.message, "connect: auth response");
    if !resp.ok {
        anyhow::bail!("auth failed: {}", resp.message);
    }
    tracing::info!(host = %p.host, port = p.port, user = %p.user, "connect: session established");
    Ok(Session { codec, seq: 1 })
}

/// 除 Connect/Disconnect 外，其余指令都需要一条已建立的会话。
fn needs_session(cmd: &Command) -> bool {
    !matches!(cmd, Command::Connect(_) | Command::Disconnect)
}

/// 探活：发一个 `Ctrl::Ping`，若能在超时内收到任意响应帧即视为连接存活。
/// 服务端 `session` 对 `Ctrl::Ping` 会回 `Pong`。
async fn probe_alive<S>(codec: &mut FrameCodec<S>) -> bool
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let body = match postcard::to_allocvec(&Ctrl::Ping) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let frame = Frame::new(FrameType::Ctrl, FrameFlags::new(), body);
    if codec.write_frame(&frame).await.is_err() {
        tracing::debug!("probe: ping write failed (dead)");
        return false;
    }
    match timeout(Duration::from_secs(3), codec.read_frame()).await {
        Ok(Ok(Some(_))) => true, // 收到响应（正常是 Pong）→ 存活
        Ok(Ok(None)) => {
            tracing::debug!("probe: connection closed by peer (dead)");
            false
        }
        Ok(Err(e)) => {
            tracing::debug!("probe: read error: {e:#} (dead)");
            false
        }
        Err(_) => {
            tracing::debug!("probe: ping timeout 3s (assume dead)");
            false
        }
    }
}

async fn send_cmd<S>(
    codec: &mut FrameCodec<S>,
    seq: &mut u32,
    cmd: CmdType,
    body: &impl Serialize,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    *seq += 1;
    let req = CmdRequest {
        seq: *seq,
        cmd,
        body: postcard::to_allocvec(body)?,
    };
    tracing::debug!(seq = *seq, cmd = ?req.cmd, body_len = req.body.len(), "send cmd request");
    let frame = Frame::new(FrameType::CmdRequest, FrameFlags::new(), req.encode()?);
    codec.write_frame(&frame).await?;
    Ok(())
}

async fn read_response<S>(codec: &mut FrameCodec<S>) -> Result<CmdResponse>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let fr = codec
        .read_frame()
        .await
        .context("read response")?
        .context("connection closed")?;
    let resp = CmdResponse::decode(&fr.payload)?;
    tracing::debug!(
        seq = resp.seq,
        ok = resp.ok,
        message = %resp.message,
        body_len = resp.body.len(),
        "recv cmd response"
    );
    Ok(resp)
}

async fn do_ls(s: &mut Session, path: &str, evt: &std_mpsc::Sender<Event>) {
    tracing::debug!(path, "ls: sending request");
    let resp = match (async {
        send_cmd(
            &mut s.codec,
            &mut s.seq,
            CmdType::Ls,
            &LsRequest {
                path: path.to_string(),
                recursive: false,
            },
        )
        .await?;
        read_response(&mut s.codec).await
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(path, "ls failed: {e:#}");
            send(evt, Event::Error(format!("ls failed: {e:#}")));
            return;
        }
    };
    if !resp.ok {
        tracing::error!(path, message = %resp.message, "ls rejected by server");
        send(evt, Event::Error(format!("ls: {}", resp.message)));
        return;
    }
    let lsr: LsResponse = match postcard::from_bytes(&resp.body) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path, "ls decode failed: {e}");
            send(evt, Event::Error(format!("ls decode: {e}")));
            return;
        }
    };
    tracing::debug!(path, entries = lsr.entries.len(), "ls: response decoded");
    send(
        evt,
        Event::DirListed {
            path: path.to_string(),
            entries: lsr.entries,
        },
    );
}

/// 递归列目录（同步用）：返回该目录下所有文件的相对路径列表。
/// 失败返回空 vec（调用方据此跳过比对）。
async fn do_ls_recursive(s: &mut Session, path: &str) -> Vec<FileEntry> {
    let resp = match (async {
        send_cmd(
            &mut s.codec,
            &mut s.seq,
            CmdType::Ls,
            &LsRequest {
                path: path.to_string(),
                recursive: true,
            },
        )
        .await?;
        read_response(&mut s.codec).await
    })
    .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if !resp.ok {
        return Vec::new();
    }
    match postcard::from_bytes::<LsResponse>(&resp.body) {
        Ok(v) => v.entries,
        Err(_) => Vec::new(),
    }
}

async fn do_backups(s: &mut Session, evt: &std_mpsc::Sender<Event>) {
    tracing::debug!("backups: sending request");
    let resp = match (async {
        send_cmd(&mut s.codec, &mut s.seq, CmdType::Backups, &BackupsRequest {}).await?;
        read_response(&mut s.codec).await
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("backups failed: {e:#}");
            send(evt, Event::Error(format!("list backups failed: {e:#}")));
            return;
        }
    };
    if !resp.ok {
        tracing::error!(message = %resp.message, "backups rejected by server");
        send(evt, Event::Error(format!("backups: {}", resp.message)));
        return;
    }
    let br: BackupsResponse = match postcard::from_bytes(&resp.body) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("backups decode failed: {e}");
            send(evt, Event::Error(format!("backups decode: {e}")));
            return;
        }
    };
    tracing::debug!(versions = br.versions.len(), "backups: response decoded");
    send(evt, Event::BackupVersions { versions: br.versions });
}

async fn do_simple<T: Serialize>(
    s: &mut Session,
    cmd: CmdType,
    body: &T,
    evt: &std_mpsc::Sender<Event>,
) {
    tracing::debug!(?cmd, "simple op: sending");
    let resp = match (async {
        send_cmd(&mut s.codec, &mut s.seq, cmd, body).await?;
        read_response(&mut s.codec).await
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(?cmd, "simple op failed: {e:#}");
            send(evt, Event::OpDone { ok: false, message: format!("{e:#}") });
            return;
        }
    };
    tracing::debug!(?cmd, ok = resp.ok, message = %resp.message, "simple op done");
    send(
        evt,
        Event::OpDone {
            ok: resp.ok,
            message: resp.message,
        },
    );
}

async fn do_upload(
    s: &mut Session,
    remote_path: &str,
    local_path: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    let name = remote_path.to_string();
    let lp = std::path::Path::new(local_path);
    // 流式统计（不把整文件读进内存）：size / sha256 / 分片数
    let (size, sha, total) = match file_digest(lp) {
        Ok(v) => v,
        Err(e) => {
            send(
                evt,
                Event::TransferDone {
                    id: 0,
                    ok: false,
                    message: format!("read local: {e}"),
                },
            );
            return;
        }
    };
    // 稳定 id → 同一(路径,内容)重试可续传
    let id = stable_id(&name, &sha);

    send(
        evt,
        Event::TransferStarted {
            id,
            name: name.clone(),
            direction: Direction::Upload,
        },
    );

    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Upload,
        &UploadInit {
            transfer_id: id,
            remote_path: name.clone(),
            size,
            mtime: file_mtime(lp),
            chunk_size: CHUNK_SIZE_DEFAULT as u32,
            total_chunks: total,
            file_sha256: sha,
            backup_first: false,
            mode: file_mode(lp),
        },
    )
    .await
    {
        send(evt, Event::TransferDone { id, ok: false, message: format!("init: {e}") });
        return;
    }
    let init_resp = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(evt, Event::TransferDone { id, ok: false, message: format!("init resp: {e}") });
            return;
        }
    };
    if !init_resp.ok {
        send(evt, Event::TransferDone { id, ok: false, message: init_resp.message });
        return;
    }

    // 断点续传：服务端回传已收片集合，client 只补缺失片
    let received: std::collections::HashSet<u32> = postcard::from_bytes::<UploadInitAck>(
        &init_resp.body,
    )
    .map(|a| a.received.into_iter().collect())
    .unwrap_or_default();
    if !received.is_empty() {
        send(
            evt,
            Event::Status(crate::i18n::tf("resume {n}: server has {u}/{d} chunks", &[
                ("n", name.as_str()),
                ("u", &received.len().to_string()),
                ("d", &total.to_string()),
            ])),
        );
    }

    // 流式发送分片（内存恒定）
    if let Err(e) = send_file_chunks(s, id, lp, total, &received, |sent| {
        send(evt, Event::TransferProgress { id, sent, total: size });
    })
    .await
    {
        send(evt, Event::TransferDone { id, ok: false, message: format!("send chunk: {e}") });
        return;
    }

    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Upload,
        &UploadCommit { transfer_id: id },
    )
    .await
    {
        send(evt, Event::TransferDone { id, ok: false, message: format!("commit: {e}") });
        return;
    }
    let resp = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(evt, Event::TransferDone { id, ok: false, message: format!("commit resp: {e}") });
            return;
        }
    };
    send(evt, Event::TransferDone { id, ok: resp.ok, message: resp.message });
}

async fn do_download(
    s: &mut Session,
    remote_path: &str,
    local_path: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    let id = next_id();
    let name = remote_path.to_string();
    send(
        evt,
        Event::TransferStarted {
            id,
            name: name.clone(),
            direction: Direction::Download,
        },
    );

    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Download,
        &DownloadRequest {
            remote_path: name.clone(),
            policy: NamePolicy::Overwrite,
        },
    )
    .await
    {
        send(evt, Event::TransferDone { id, ok: false, message: format!("request: {e}") });
        return;
    }

    // 流式落盘：边收分片边写入临时文件（避免整文件驻留内存），
    // 收完校验 sha256 后再原子改名为目标文件（失败不留下半成品）。
    let tmp_path = format!("{local_path}.rdep-part");
    let mut file = match std::fs::File::create(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            send(evt, Event::TransferDone { id, ok: false, message: format!("create temp: {e}") });
            return;
        }
    };
    use std::io::Write;
    let mut hasher = Sha256::new();
    let mut sent = 0u64;
    let mut expected_sha: Option<[u8; 32]> = None;
    // 远端的权限位与 mtime，下载完成后还原到本地文件（实现「下载后属性/时间一致」）。
    let mut remote_mode: u32 = 0;
    let mut remote_mtime: i64 = 0;
    let mut failed: Option<String> = None;

    loop {
        let f = match s.codec.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                failed = Some("connection closed".into());
                break;
            }
            Err(e) => {
                failed = Some(format!("{e}"));
                break;
            }
        };
        match f.frame_type {
            FrameType::DataChunk => {
                let dc: DataChunk = match postcard::from_bytes(&f.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        failed = Some(format!("dec chunk: {e}"));
                        break;
                    }
                };
                if file.write_all(&dc.data).is_err() {
                    failed = Some("write temp file failed".into());
                    break;
                }
                hasher.update(&dc.data);
                sent += dc.data.len() as u64;
                send(evt, Event::TransferProgress { id, sent, total: 0 });
            }
            FrameType::CmdResponse => {
                let r = match CmdResponse::decode(&f.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        failed = Some(format!("dec resp: {e}"));
                        break;
                    }
                };
                if !r.ok {
                    failed = Some(r.message);
                    break;
                }
                // 应答体为 DownloadResponse{ mode, mtime, sha256 }（postcard 编码）
                if let Ok(dr) = postcard::from_bytes::<DownloadResponse>(&r.body) {
                    expected_sha = Some(dr.sha256);
                    remote_mode = dr.mode;
                    remote_mtime = dr.mtime;
                }
                break;
            }
            _ => {}
        }
    }
    drop(file); // flush + close

    if let Some(msg) = failed {
        let _ = std::fs::remove_file(&tmp_path);
        send(evt, Event::TransferDone { id, ok: false, message: msg });
        return;
    }

    // 完整性校验
    if let Some(exp) = expected_sha {
        let got: [u8; 32] = hasher.finalize().into();
        if got != exp {
            let _ = std::fs::remove_file(&tmp_path);
            send(
                evt,
                Event::TransferDone {
                    id,
                    ok: false,
                    message: "downloaded file sha256 mismatch".into(),
                },
            );
            return;
        }
    }

    // 原子改名到目标路径
    if let Err(e) = std::fs::rename(&tmp_path, local_path) {
        let _ = std::fs::remove_file(&tmp_path);
        send(evt, Event::TransferDone { id, ok: false, message: format!("rename: {e}") });
        return;
    }
    // 还原远端文件的权限位与 mtime（仅 unix 有效；记警告不阻断下载成功）
    let lp = std::path::Path::new(local_path);
    if let Err(e) = apply_local_mode(lp, remote_mode) {
        tracing::warn!(path = %local_path, "download: apply mode failed: {e}");
    }
    if let Err(e) = apply_local_mtime(lp, remote_mtime) {
        tracing::warn!(path = %local_path, mtime = remote_mtime, "download: apply mtime failed: {e}");
    }
    send(evt, Event::TransferDone { id, ok: true, message: String::new() });
}

/// 发布：宣布发布 → 逐文件「带备份上传」(init/分片/commit) → 收尾执行重启脚本。
#[allow(clippy::too_many_arguments)]
async fn do_publish(
    s: &mut Session,
    remote_dir: &str,
    restart_script_id: &str,
    project: Option<&str>,
    files: &[PublishFile],
    evt: &std_mpsc::Sender<Event>,
) {
    // 逐文件流式统计（size/sha/分片数），只保留元数据不把内容读进内存
    let mut items = Vec::with_capacity(files.len());
    // (transfer_id, remote_path, local_path, sha, size, total_chunks)
    let mut plans: Vec<(u64, String, String, [u8; 32], u64, u32)> = Vec::with_capacity(files.len());
    for f in files {
        let (size, sha, total) = match file_digest(std::path::Path::new(&f.local_path)) {
            Ok(v) => v,
            Err(e) => {
                send(
                    evt,
                    Event::PublishDone {
                        ok: false,
                        message: format!("read {}: {e}", f.local_path),
                    },
                );
                return;
            }
        };
        items.push(PublishItem {
            remote_path: f.remote_path.clone(),
            size,
            chunks: total,
            sha256: sha,
        });
        plans.push((
            next_id(),
            f.remote_path.clone(),
            f.local_path.clone(),
            sha,
            size,
            total,
        ));
    }

    // 1) 宣布发布（携带重启脚本 id 与文件清单，服务端校验脚本存在并进入发布态）
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Publish,
        &PublishRequest {
            remote_dir: remote_dir.to_string(),
            restart_script_id: restart_script_id.to_string(),
            project: project.map(str::to_string),
            items,
        },
    )
    .await
    {
        send(
            evt,
            Event::PublishDone {
                ok: false,
                message: format!("announce: {e}"),
            },
        );
        return;
    }
    match read_response(&mut s.codec).await {
        Ok(r) if r.ok => {}
        Ok(r) => {
            send(evt, Event::PublishDone { ok: false, message: r.message });
            return;
        }
        Err(e) => {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("announce resp: {e}"),
                },
            );
            return;
        }
    }

    // 2) 逐文件带备份上传（流式，内存恒定）
    for (id, remote_path, local_path, sha, size, total) in &plans {
        if let Err(e) = send_cmd(
            &mut s.codec,
            &mut s.seq,
            CmdType::Upload,
            &UploadInit {
                transfer_id: *id,
                remote_path: remote_path.clone(),
                size: *size,
                mtime: file_mtime(std::path::Path::new(local_path)),
                chunk_size: CHUNK_SIZE_DEFAULT as u32,
                total_chunks: *total,
                file_sha256: *sha,
                backup_first: true,
                mode: file_mode(std::path::Path::new(local_path)),
            },
        )
        .await
        {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("init {remote_path}: {e}"),
                },
            );
            return;
        }
        let r = match read_response(&mut s.codec).await {
            Ok(r) => r,
            Err(e) => {
                send(
                    evt,
                    Event::PublishDone {
                        ok: false,
                        message: format!("init resp {remote_path}: {e}"),
                    },
                );
                return;
            }
        };
        if !r.ok {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("init {remote_path}: {}", r.message),
                },
            );
            return;
        }
        let received: std::collections::HashSet<u32> =
            postcard::from_bytes::<UploadInitAck>(&r.body)
                .map(|a| a.received.into_iter().collect())
                .unwrap_or_default();
        if let Err(e) =
            send_file_chunks(s, *id, std::path::Path::new(local_path), *total, &received, |_sent| {})
                .await
        {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("send chunk {remote_path}: {e}"),
                },
            );
            return;
        }

        if let Err(e) = send_cmd(
            &mut s.codec,
            &mut s.seq,
            CmdType::Upload,
            &UploadCommit { transfer_id: *id },
        )
        .await
        {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("commit {remote_path}: {e}"),
                },
            );
            return;
        }
        let r = match read_response(&mut s.codec).await {
            Ok(r) => r,
            Err(e) => {
                send(
                    evt,
                    Event::PublishDone {
                        ok: false,
                        message: format!("commit resp {remote_path}: {e}"),
                    },
                );
                return;
            }
        };
        if !r.ok {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("commit {remote_path}: {}", r.message),
                },
            );
            return;
        }
        send(
            evt,
            Event::TransferDone {
                id: *id,
                ok: true,
                message: remote_path.clone(),
            },
        );
    }

    // 3) 收尾：触发重启脚本
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::PublishCommit,
        &PublishCommitRequest {
            remote_dir: remote_dir.to_string(),
        },
    )
    .await
    {
        send(
            evt,
            Event::PublishDone {
                ok: false,
                message: format!("commit: {e}"),
            },
        );
        return;
    }
    let r = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(
                evt,
                Event::PublishDone {
                    ok: false,
                    message: format!("commit resp: {e}"),
                },
            );
            return;
        }
    };
    send(
        evt,
        Event::PublishDone {
            ok: r.ok,
            message: r.message,
        },
    );
}

/// 回滚：请求服务端把指定版本恢复到远端目录。
async fn do_rollback(
    s: &mut Session,
    remote_dir: &str,
    version: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Rollback,
        &RollbackRequest {
            remote_dir: remote_dir.to_string(),
            version: version.to_string(),
        },
    )
    .await
    {
        send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("rollback: {e}"),
            },
        );
        return;
    }
    let r = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(
                evt,
                Event::OpDone {
                    ok: false,
                    message: format!("rollback resp: {e}"),
                },
            );
            return;
        }
    };
    send(
        evt,
        Event::OpDone {
            ok: r.ok,
            message: r.message,
        },
    );
}

/// tail：请求服务端推送文件尾部行；follow 模式下持续读流，
/// 检测到 `tail_stop` 置位则发 `Ctrl::Stop` 收尾，再读到结束标记为止（保持会话同步）。
async fn do_tail(
    s: &mut Session,
    path: &str,
    lines: u32,
    follow: bool,
    tail_stop: &AtomicBool,
    evt: &std_mpsc::Sender<Event>,
) {
    tail_stop.store(false, Ordering::SeqCst);
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Tail,
        &TailRequest {
            path: path.to_string(),
            lines,
            follow,
        },
    )
    .await
    {
        send(
            evt,
            Event::TailDone {
                ok: false,
                message: format!("tail: {e}"),
            },
        );
        return;
    }

    let mut stopping = false;
    loop {
        // 收到停止请求 → 发 Ctrl::Stop，然后进入“排空”模式读到结束标记
        if !stopping && tail_stop.load(Ordering::SeqCst) {
            let body = match postcard::to_allocvec(&Ctrl::Stop) {
                Ok(v) => v,
                Err(e) => {
                    send(
                        evt,
                        Event::TailDone {
                            ok: false,
                            message: format!("stop encode: {e}"),
                        },
                    );
                    return;
                }
            };
            let frame = Frame::new(FrameType::Ctrl, FrameFlags::new(), body);
            if s.codec.write_frame(&frame).await.is_err() {
                return;
            }
            stopping = true;
        }

        let wait = if stopping {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(200)
        };
        match timeout(wait, s.codec.read_frame()).await {
            Ok(Ok(Some(frame))) => match frame.frame_type {
                FrameType::StreamPush => {
                    let sp: StreamPush = match postcard::from_bytes(&frame.payload) {
                        Ok(v) => v,
                        Err(e) => {
                            send(
                                evt,
                                Event::TailDone {
                                    ok: false,
                                    message: format!("dec push: {e}"),
                                },
                            );
                            return;
                        }
                    };
                    if !sp.line.is_empty() {
                        send(evt, Event::TailLine { line: sp.line });
                    }
                    // eof 仅表示“无更多行”，仍需继续读 CmdResponse 以保持会话同步
                }
                FrameType::CmdResponse => {
                    let r = match CmdResponse::decode(&frame.payload) {
                        Ok(v) => v,
                        Err(e) => {
                            send(
                                evt,
                                Event::TailDone {
                                    ok: false,
                                    message: format!("dec resp: {e}"),
                                },
                            );
                            return;
                        }
                    };
                    send(
                        evt,
                        Event::TailDone {
                            ok: r.ok,
                            message: r.message,
                        },
                    );
                    break;
                }
                _ => {}
            },
            Ok(Ok(None)) => {
                send(
                    evt,
                    Event::TailDone {
                        ok: false,
                        message: "connection closed".into(),
                    },
                );
                break;
            }
            Ok(Err(e)) => {
                send(
                    evt,
                    Event::TailDone {
                        ok: false,
                        message: format!("{e}"),
                    },
                );
                break;
            }
            Err(_) => {
                // 普通轮询超时：继续；停止排空超时：放弃
                if stopping {
                    break;
                }
            }
        }
    }
}

/// grep：请求服务端检索，返回命中行集合。
async fn do_grep(
    s: &mut Session,
    path: &str,
    pattern: &str,
    flags: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Grep,
        &GrepRequest {
            path: path.to_string(),
            pattern: pattern.to_string(),
            flags: flags.to_string(),
        },
    )
    .await
    {
        send(evt, Event::Error(format!("grep: {e}")));
        return;
    }
    let r = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(evt, Event::Error(format!("grep resp: {e}")));
            return;
        }
    };
    if !r.ok {
        send(evt, Event::Error(format!("grep: {}", r.message)));
        return;
    }
    let gr: GrepResponse = match postcard::from_bytes(&r.body) {
        Ok(v) => v,
        Err(e) => {
            send(evt, Event::Error(format!("grep decode: {e}")));
            return;
        }
    };
    send(evt, Event::GrepResult { lines: gr.lines });
}

/// edit（读）：拉取远端文件内容。
async fn do_edit_get(s: &mut Session, remote_path: &str, evt: &std_mpsc::Sender<Event>) {
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Edit,
        &EditRequest {
            remote_path: remote_path.to_string(),
            content: None,
        },
    )
    .await
    {
        send(evt, Event::Error(format!("edit: {e}")));
        return;
    }
    let r = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(evt, Event::Error(format!("edit resp: {e}")));
            return;
        }
    };
    if !r.ok {
        send(evt, Event::Error(format!("edit: {}", r.message)));
        return;
    }
    let content = String::from_utf8_lossy(&r.body).into_owned();
    send(evt, Event::EditLoaded { content });
}

/// edit（写）：保存内容（服务端先备份再覆盖）。
async fn do_edit_save(
    s: &mut Session,
    remote_path: &str,
    content: &str,
    evt: &std_mpsc::Sender<Event>,
) {
    if let Err(e) = send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Edit,
        &EditRequest {
            remote_path: remote_path.to_string(),
            content: Some(content.to_string()),
        },
    )
    .await
    {
        send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("edit save: {e}"),
            },
        );
        return;
    }
    let r = match read_response(&mut s.codec).await {
        Ok(r) => r,
        Err(e) => {
            send(
                evt,
                Event::OpDone {
                    ok: false,
                    message: format!("edit save resp: {e}"),
                },
            );
            return;
        }
    };
    send(
        evt,
        Event::OpDone {
            ok: r.ok,
            message: r.message,
        },
    );
}

/// 本地待同步文件的一项（相对路径 + 内容指纹）。
struct LocalFile {
    rel: String,
    abs: PathBuf,
    size: u64,
    mtime: i64,
}

/// 递归收集本地目录下的所有文件（相对路径、大小、mtime）。空目录返回空 vec。
/// 跳过符号链接（不跟随/不复制），避免同步错误内容与链接环无限递归。
fn collect_local_files(root: &std::path::Path) -> Vec<LocalFile> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<LocalFile>) {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for e in rd.flatten() {
            let p = e.path();
            // 用 symlink_metadata：链接一律跳过（不进入链接目录、不复制链接目标）
            let meta = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(LocalFile {
                    rel,
                    abs: p,
                    size: meta.len(),
                    mtime: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                });
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// 目录同步：本地目录 → 远端目录。
///
/// 变更判定（rsync 风格快筛）：远端不存在 / 大小不同 / 远端 mtime 早于本地 mtime
/// → 视为变更。每个变更文件走「先备份再覆盖」的断点续传上传；`delete_extra` 时
/// 删除远端多余文件。`dry_run` 只统计不落地。
async fn do_sync_dir(
    s: &mut Session,
    local_dir: &str,
    remote_dir: &str,
    delete_extra: bool,
    dry_run: bool,
    evt: &std_mpsc::Sender<Event>,
) {
    let root = PathBuf::from(local_dir);
    if !root.is_dir() {
        send(
            evt,
            Event::OpDone {
                ok: false,
                message: format!("local dir not found: {local_dir}"),
            },
        );
        return;
    }
    let local = collect_local_files(&root);
    // 远端现状（递归，相对路径 + 大小 + mtime）
    let remote_entries = do_ls_recursive(s, remote_dir).await;
    let mut remote_map: std::collections::HashMap<String, (u64, i64)> =
        std::collections::HashMap::new();
    for e in &remote_entries {
        if !e.is_dir {
            remote_map.insert(e.name.clone(), (e.size, e.mtime));
        }
    }

    // 变更文件（需要上传）
    let mut changed: Vec<&LocalFile> = Vec::new();
    for lf in &local {
        let need = match remote_map.get(&lf.rel) {
            None => true, // 远端没有
            Some((rsize, rmtime)) => *rsize != lf.size || *rmtime < lf.mtime,
        };
        if need {
            changed.push(lf);
        }
    }
    // 远端多余文件（本地没有）
    let local_rels: std::collections::HashSet<String> =
        local.iter().map(|f| f.rel.clone()).collect();
    let extras: Vec<String> = remote_map
        .keys()
        .filter(|k| !local_rels.contains(*k))
        .cloned()
        .collect();

    let summary = crate::i18n::tf("sync: local {n} files: {u} to upload, {d} to delete", &[
        ("n", &local.len().to_string()),
        ("u", &changed.len().to_string()),
        ("d", &extras.len().to_string()),
    ]);
    send(evt, Event::Status(summary.clone()));
    // 下发同步计划（GUI 可展示「将上传/将删除」明细，删除前有预览）
    send(
        evt,
        Event::SyncPreview {
            changed: changed.iter().map(|f| f.rel.clone()).collect(),
            to_delete: extras.clone(),
        },
    );

    if dry_run {
        send(evt, Event::OpDone { ok: true, message: format!("[dry-run] {summary}") });
        return;
    }

    // 逐个上传变更文件（先备份再覆盖，走可续传上传）。
    // 单个文件失败不中断整次同步，最后统一汇总（避免一个坏文件毁掉整批部署）。
    let mut failures: Vec<String> = Vec::new();
    for lf in &changed {
        let remote_path = join_remote_path(remote_dir, &lf.rel);
        if let Err(e) = upload_file_backup(s, &remote_path, &lf.abs, evt).await {
            failures.push(format!("{}: {e}", lf.rel));
        }
    }

    // 删除远端多余文件
    if delete_extra && !extras.is_empty() {
        let paths: Vec<String> = extras
            .iter()
            .map(|r| join_remote_path(remote_dir, r))
            .collect();
        match (async {
            send_cmd(
                &mut s.codec,
                &mut s.seq,
                CmdType::Delete,
                &DeleteRequest { paths },
            )
            .await?;
            read_response(&mut s.codec).await
        })
        .await
        {
            Ok(r) if r.ok => {}
            Ok(r) => failures.push(format!("delete: {}", r.message)),
            Err(e) => failures.push(format!("delete: {e}")),
        }
    }

    if failures.is_empty() {
        send(evt, Event::OpDone { ok: true, message: summary });
    } else {
        let detail = failures.join("; ");
        send(
            evt,
            Event::OpDone {
                ok: false,
                message: crate::i18n::tf("{summary}; {n} failed: {d}", &[
                    ("summary", summary.as_str()),
                    ("n", &failures.len().to_string()),
                    ("d", detail.as_str()),
                ]),
            },
        );
    }
}

/// 远端路径拼接（与 publish 一致：base + "/" + rel）。
fn join_remote_path(base: &str, rel: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.is_empty() {
        format!("/{rel}")
    } else {
        format!("{b}/{rel}")
    }
}

/// 上传单个文件且 `backup_first=true`（同步/覆盖场景）。内部复用稳定 transfer_id，可续传。
/// 过程通过 `TransferStarted/Progress/Done` 事件上报，便于 GUI 传输队列显示每个文件进度。
/// 成功返回 Ok(())，失败返回原因（调用方负责汇总）。
async fn upload_file_backup(
    s: &mut Session,
    remote_path: &str,
    local_abs: &std::path::Path,
    evt: &std_mpsc::Sender<Event>,
) -> Result<()> {
    // 流式统计（不把整文件读进内存）
    let (size, sha, total) = file_digest(local_abs)?;
    let id = stable_id(remote_path, &sha);

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
        anyhow::anyhow!(m)
    };

    send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Upload,
        &UploadInit {
            transfer_id: id,
            remote_path: remote_path.to_string(),
            size,
            mtime: file_mtime(local_abs),
            chunk_size: CHUNK_SIZE_DEFAULT as u32,
            total_chunks: total,
            file_sha256: sha,
            backup_first: true,
            mode: file_mode(local_abs),
        },
    )
    .await
    .map_err(|e| fail(format!("init: {e}")))?;
    let ack = read_response(&mut s.codec)
        .await
        .map_err(|e| fail(format!("init resp: {e}")))?;
    if !ack.ok {
        return Err(fail(format!("init: {}", ack.message)));
    }
    let received: std::collections::HashSet<u32> =
        postcard::from_bytes::<UploadInitAck>(&ack.body)
            .map(|a| a.received.into_iter().collect())
            .unwrap_or_default();
    // 流式发送分片（内存恒定）
    send_file_chunks(s, id, local_abs, total, &received, |sent| {
        send(evt, Event::TransferProgress { id, sent, total: size });
    })
    .await
    .map_err(|e| fail(format!("send chunk: {e}")))?;
    send_cmd(
        &mut s.codec,
        &mut s.seq,
        CmdType::Upload,
        &UploadCommit { transfer_id: id },
    )
    .await
    .map_err(|e| fail(format!("commit: {e}")))?;
    let resp = read_response(&mut s.codec)
        .await
        .map_err(|e| fail(format!("commit resp: {e}")))?;
    if !resp.ok {
        return Err(fail(format!("commit: {}", resp.message)));
    }
    send(
        evt,
        Event::TransferDone {
            id,
            ok: true,
            message: String::new(),
        },
    );
    Ok(())
}
