use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rdep_protocol::{
    AuthMethod, AuthRequest, AuthResponse, BackupsResponse, CmdRequest, CmdResponse, CmdType, CopyPolicy,
    CopyRequest, Ctrl, DataChunk, DeleteRequest, DownloadRequest, EditRequest, ErrorCode, Frame,
    FrameFlags, FrameType, GrepRequest, GrepResponse, LsRequest, MkdirRequest, MoveRequest,
    PublishCommitRequest, PublishRequest, RenameRequest, RollbackRequest, StreamPush, TailRequest,
    UploadCommit, UploadInit, UploadInitAck, sha256, CHUNK_SIZE_DEFAULT,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

/// 单连接上允许的连续认证失败次数上限，超过后要求重连（暴力破解节流）。
const MAX_AUTH_FAILURES: u32 = 5;

use crate::db::Db;
use crate::storage::Storage;
use crate::transport::FrameCodec;

/// 一次上传的进行中状态（分片数据落盘暂存，见 `Storage` 的 staging 区）。
struct ActiveUpload {
    /// 远端目标路径（协议层字符串，写入时再 resolve）。
    remote_path: String,
    /// 提交时是否先备份旧文件。
    backup: bool,
    total_chunks: u32,
    file_sha256: [u8; 32],
    /// 落盘后要应用的权限位（0 = 默认）。
    mode: u32,
}

/// 一次发布（发布=多文件带备份上传 + 收尾重启脚本）的进行中状态。
struct PublishState {
    remote_dir: String,
    restart_script_id: String,
    /// 项目模式下的路径前缀（`Some(项目 remote_dir)`）：后续 Upload 的目标路径
    /// 会被重新落到此前缀之下，使项目记录成为**唯一**决定部署位置的一方。
    /// 普通发布为 None，沿用客户端自报的路径。
    path_prefix: Option<String>,
    /// 已完成的发布文件数（每成功提交一个 Upload 计一次）。
    completed: usize,
}

/// 处理单个已建立 TLS 连接的会话：循环读帧、按类型分发。
///
/// `peer` 仅用于日志标识（直连时为 `ip:port`，中转时为 `forwarder/<service_id>`）。
pub async fn handle<S>(
    stream: S,
    storage: Arc<Storage>,
    db: Arc<Db>,
    scripts_dir: &Path,
    peer: &str,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    tracing::debug!(peer, "session: opened");
    let mut codec = FrameCodec::new(stream);
    let mut authed = false;
    let mut uploads: HashMap<u64, ActiveUpload> = HashMap::new();
    let mut publish: Option<PublishState> = None;
    // 本连接上的连续认证失败次数（暴力破解节流，见 handle_cmd 的 Auth 分支）。
    let mut auth_failures: u32 = 0;

    loop {
        let frame = match codec.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                tracing::debug!(peer, "session: peer closed connection cleanly");
                return Ok(());
            }
            // 读帧失败（网络中断 / 半帧 / 解析错误）：记录后结束会话，
            // 这是定位「client 报 read stream」类问题的关键日志。
            // 客户端被 kill / 进程退出时不会发 TLS close_notify，属常见的正常断连，
            // 用 debug 记（否则这类噪音会淹没真正的故障信号）。
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("close_notify") {
                    tracing::debug!(peer, "session: client disconnected abruptly (no close_notify): {msg}");
                } else {
                    tracing::warn!(peer, "session: frame read failed, closing: {msg}");
                }
                return Err(e);
            }
        };
        tracing::debug!(peer, frame_type = ?frame.frame_type, payload_len = frame.payload.len(), "session: frame received");
        let result: Result<()> = match frame.frame_type {
            FrameType::CmdRequest => {
                let req = match CmdRequest::decode(&frame.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(peer, "session: cmd decode failed: {e}");
                        return Err(e.into());
                    }
                };
                match handle_cmd(
                    &req,
                    peer,
                    &mut authed,
                    &mut auth_failures,
                    &mut uploads,
                    &mut publish,
                    scripts_dir,
                    &storage,
                    db.clone(),
                    &mut codec,
                )
                .await
                {
                    Ok(Some(resp)) => {
                        tracing::debug!(peer, seq = req.seq, cmd = ?req.cmd, resp_ok = resp_ok(&resp), "session: sending response");
                        codec.write_frame(&resp).await
                            .context("write response frame")
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            FrameType::DataChunk => {
                let chk: DataChunk = match postcard::from_bytes(&frame.payload) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(peer, "session: chunk decode failed: {e}");
                        return Err(e.into());
                    }
                };
                // 仅对本会话活跃的传输落盘暂存；先校验分片 SHA，坏片直接丢弃
                if uploads.contains_key(&chk.transfer_id)
                    && sha256(&chk.data) == chk.chunk_sha256
                {
                    tracing::trace!(peer, transfer_id = chk.transfer_id, index = chk.index, bytes = chk.data.len(), "session: chunk staged");
                    storage.stage_chunk(chk.transfer_id, chk.index, &chk.data)
                        .with_context(|| format!("stage chunk {} of transfer {}", chk.index, chk.transfer_id))
                } else {
                    // 未知 transfer_id / 校验失败的分片忽略（容错，client 会重传）
                    tracing::debug!(peer, transfer_id = chk.transfer_id, index = chk.index, "session: dropped chunk (unknown transfer or sha mismatch)");
                    Ok(())
                }
            }
            FrameType::Ctrl => {
                // 向前兼容：无法解码的控制帧（未来协议新增变体、旧客户端发送废弃变体）
                // 一律**忽略**，绝不能因此终止会话——否则协议版本错配会直接断连。
                match postcard::from_bytes::<Ctrl>(&frame.payload) {
                    Ok(Ctrl::Ping) => {
                        tracing::debug!(peer, "session: ping -> pong");
                        codec.write_frame(&ctrl_frame(Ctrl::Pong)).await
                            .context("write pong frame")
                    }
                    Ok(_) => Ok(()), /* Stop 等在会话层无意义，忽略 */
                    Err(e) => {
                        tracing::debug!(peer, "session: ignore undecodable ctrl frame: {e}");
                        Ok(())
                    }
                }
            }
            FrameType::StreamPush | FrameType::CmdResponse => {
                // 服务端不会收到这两类帧；忽略
                tracing::debug!(peer, frame_type = ?frame.frame_type, "session: ignored unexpected frame");
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::warn!(peer, "session: handler error, closing: {e:#}");
            return Err(e);
        }
    }
}

/// 读取响应帧的 ok 位（仅用于日志，不解码完整 CmdResponse）。
fn resp_ok(frame: &Frame) -> bool {
    CmdResponse::decode(&frame.payload).map(|r| r.ok).unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
async fn handle_cmd<S>(
    req: &CmdRequest,
    peer: &str,
    authed: &mut bool,
    auth_failures: &mut u32,
    uploads: &mut HashMap<u64, ActiveUpload>,
    publish: &mut Option<PublishState>,
    scripts_dir: &Path,
    storage: &Storage,
    db: Arc<Db>,
    codec: &mut FrameCodec<S>,
) -> Result<Option<Frame>>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // 除 AUTH 外，其余指令必须先认证
    if req.cmd != CmdType::Auth && !*authed {
        tracing::warn!(peer, cmd = ?req.cmd, seq = req.seq, "cmd rejected: not authenticated");
        return Ok(Some(make_response(
            req,
            false,
            ErrorCode::AuthFailed,
            "not authenticated",
            vec![],
        )));
    }

    match req.cmd {
        CmdType::Auth => {
            let ar: AuthRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "auth: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, user = %ar.user, method = ?ar.method, pass_len = ar.pass.len(), "auth: request");
            let ok = match ar.method {
                AuthMethod::Password => db.auth_async(&ar.user, &ar.pass).await,
                // API 令牌：明文放在 `pass` 字段，`user` 仅作标注（便于审计）。
                // 令牌本身是 256 bit 随机值，校验只需一次哈希查表，无需 PBKDF2。
                AuthMethod::Token => db.verify_token(&ar.pass).is_ok_and(|u| u.is_some()),
            };
            if *auth_failures >= MAX_AUTH_FAILURES {
                // 同一连接上认证失败次数过多：拒绝继续尝试，要求重连。
                // 局限：这是**每连接**节流，重连即可重置计数，因此只提高单连接爆破成本，
                // 无法阻止分布式慢速爆破。彻底方案需按账号/IP 的跨连接限流（需共享状态）。
                tracing::warn!(peer, "auth: too many failed attempts, requiring reconnect");
                return Ok(Some(make_response(
                    req,
                    false,
                    ErrorCode::AuthFailed,
                    "too many failed attempts, reconnect",
                    vec![],
                )));
            }
            if ok {
                *authed = true;
                *auth_failures = 0;
                tracing::info!(peer, user = %ar.user, "auth: success");
                // token 字段留空：协议层不签发会话令牌（连接本身即会话），
                // 此处曾硬编码 "dev-token"，形似可用凭据却从未被校验——为避免误导，改为空串。
                let body = postcard::to_allocvec(&AuthResponse {
                    token: String::new(),
                    expires: 0,
                })?;
                Ok(Some(make_response(req, true, ErrorCode::Ok, "", body)))
            } else {
                *auth_failures = auth_failures.saturating_add(1);
                tracing::warn!(peer, user = %ar.user, failures = *auth_failures, "auth: invalid credentials");
                // 逐次递增延时，进一步抬高单连接爆破成本
                let delay = Duration::from_millis(200 * u64::from(*auth_failures));
                tokio::time::sleep(delay.min(Duration::from_secs(3))).await;
                Ok(Some(make_response(
                    req,
                    false,
                    ErrorCode::AuthFailed,
                    "invalid credentials",
                    vec![],
                )))
            }
        }

        CmdType::Ls => {
            let lr: LsRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "ls: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, path = %lr.path, recursive = lr.recursive, "ls: request");
            let resp = match storage.ls(&lr) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, path = %lr.path, "ls: failed: {e:#}");
                    return Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::PathNotFound,
                        &format!("ls failed: {e:#}"),
                        vec![],
                    )));
                }
            };
            tracing::debug!(peer, path = %lr.path, entries = resp.entries.len(), "ls: ok");
            let body = postcard::to_allocvec(&resp)?;
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", body)))
        }

        CmdType::Mkdir => {
            let mr: MkdirRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "mkdir: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, paths = ?mr.paths, "mkdir: request");
            match storage.mkdir(&mr.paths) {
                Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![]))),
                Err(e) => {
                    tracing::warn!(peer, paths = ?mr.paths, "mkdir: failed: {e:#}");
                    Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::NameConflict,
                        &format!("mkdir failed: {e:#}"),
                        vec![],
                    )))
                }
            }
        }

        CmdType::Upload => {
            // 请求体可能是 UploadInit 或 UploadCommit，按可解析性区分
            if let Ok(init) = postcard::from_bytes::<UploadInit>(&req.body) {
                tracing::debug!(peer, transfer_id = init.transfer_id, path = %init.remote_path, size = init.size, chunks = init.total_chunks, backup = init.backup_first, "upload init");
                // 续传：若该 transfer_id 已有暂存片，回传已收集合，client 只补缺失片
                let received = storage.received_chunks(init.transfer_id)?;
                tracing::debug!(peer, transfer_id = init.transfer_id, resumed = received.len(), "upload init: staged chunks from previous attempts");
                // 项目模式：把上传目标重新落到项目目录之下（只取文件名），
                // 客户端无法借此写到项目之外。
                let effective_path = match publish.as_ref().and_then(|p| p.path_prefix.as_ref()) {
                    Some(base) => {
                        let fname = init
                            .remote_path
                            .rsplit('/')
                            .find(|s| !s.is_empty())
                            .unwrap_or(&init.remote_path);
                        format!("{base}/{fname}")
                    }
                    None => init.remote_path.clone(),
                };
                let au = ActiveUpload {
                    remote_path: effective_path,
                    backup: init.backup_first,
                    total_chunks: init.total_chunks,
                    file_sha256: init.file_sha256,
                    mode: init.mode,
                };
                uploads.insert(init.transfer_id, au);
                let body = postcard::to_allocvec(&UploadInitAck { received })?;
                Ok(Some(make_response(req, true, ErrorCode::Ok, "", body)))
            } else if let Ok(commit) = postcard::from_bytes::<UploadCommit>(&req.body) {
                tracing::debug!(peer, transfer_id = commit.transfer_id, "upload commit");
                match uploads.remove(&commit.transfer_id) {
                    Some(au) => {
                        // 从暂存区按序合并，校验整文件 SHA
                        let data = match storage.assemble_staged(commit.transfer_id, au.total_chunks) {
                            Ok(d) => d,
                            Err(e) => {
                                tracing::warn!(peer, transfer_id = commit.transfer_id, path = %au.remote_path, "upload commit: assemble failed: {e:#}");
                                return Ok(Some(make_response(
                                    req,
                                    false,
                                    ErrorCode::FileChecksumMismatch,
                                    &format!("assemble failed: {e:#}"),
                                    vec![],
                                )));
                            }
                        };
                        if sha256(&data) != au.file_sha256 {
                            tracing::warn!(peer, transfer_id = commit.transfer_id, path = %au.remote_path, bytes = data.len(), "upload commit: sha256 mismatch");
                            storage.cleanup_staging(commit.transfer_id);
                            return Ok(Some(make_response(
                                req,
                                false,
                                ErrorCode::FileChecksumMismatch,
                                "assembled file sha256 mismatch",
                                vec![],
                            )));
                        }
                        if au.backup {
                            tracing::debug!(peer, path = %au.remote_path, bytes = data.len(), "upload commit: saving with backup");
                            if let Err(e) = storage.save_with_backup(&au.remote_path, &data) {
                                tracing::error!(peer, path = %au.remote_path, "upload commit: save failed: {e:#}");
                                return Ok(Some(make_response(
                                    req,
                                    false,
                                    ErrorCode::FileChecksumMismatch,
                                    &format!("save failed: {e:#}"),
                                    vec![],
                                )));
                            }
                        } else {
                            let dest = match storage.resolve(&au.remote_path) {
                                Ok(d) => d,
                                Err(e) => {
                                    tracing::warn!(peer, path = %au.remote_path, "upload commit: resolve failed: {e:#}");
                                    return Ok(Some(make_response(
                                        req,
                                        false,
                                        ErrorCode::PathNotFound,
                                        &format!("path rejected: {e:#}"),
                                        vec![],
                                    )));
                                }
                            };
                            tracing::debug!(peer, path = %au.remote_path, dest = %dest.display(), bytes = data.len(), "upload commit: saving");
                            if let Err(e) = storage.save(&dest, &data) {
                                tracing::error!(peer, dest = %dest.display(), "upload commit: save failed: {e:#}");
                                return Ok(Some(make_response(
                                    req,
                                    false,
                                    ErrorCode::FileChecksumMismatch,
                                    &format!("save failed: {e:#}"),
                                    vec![],
                                )));
                            }
                        }
                        // 落盘后恢复权限位（保留可执行位等）
                        let dest = storage.resolve(&au.remote_path)?;
                        if let Err(e) = storage.apply_mode(&dest, au.mode) {
                            tracing::warn!(peer, dest = %dest.display(), "upload commit: apply mode failed: {e:#}");
                        }
                        storage.cleanup_staging(commit.transfer_id);
                        if let Some(p) = publish.as_mut() {
                            p.completed += 1;
                        }
                        tracing::info!(peer, path = %au.remote_path, bytes = data.len(), "upload commit: saved");
                        Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![])))
                    }
                    None => {
                        tracing::warn!(peer, transfer_id = commit.transfer_id, "upload commit: unknown transfer_id");
                        Ok(Some(make_response(
                            req,
                            false,
                            ErrorCode::NameConflict,
                            "unknown transfer_id",
                            vec![],
                        )))
                    }
                }
            } else {
                tracing::warn!(peer, cmd = ?req.cmd, "upload: bad body (neither init nor commit)");
                Ok(Some(make_response(
                    req,
                    false,
                    ErrorCode::NameConflict,
                    "bad upload body",
                    vec![],
                )))
            }
        }

        CmdType::Download => {
            let dr: DownloadRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "download: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, path = %dr.remote_path, "download: request");
            let data = match storage.read_file(&dr.remote_path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(peer, path = %dr.remote_path, "download: read failed: {e:#}");
                    return Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::PathNotFound,
                        &format!("read failed: {e:#}"),
                        vec![],
                    )));
                }
            };
            let parts = rdep_protocol::split_file(&data, CHUNK_SIZE_DEFAULT);
            let transfer_id = req.seq as u64;
            let n = parts.len();
            for (i, chunk) in parts {
                let dc = DataChunk::new(transfer_id, i, chunk);
                let f = Frame::new(
                    FrameType::DataChunk,
                    FrameFlags::new(),
                    postcard::to_allocvec(&dc)?,
                );
                codec.write_frame(&f).await?;
            }
            tracing::debug!(peer, path = %dr.remote_path, bytes = data.len(), chunks = n, "download: streamed");
            // 响应体带文件 sha256，client 落盘后可校验完整性
            let sha = sha256(&data);
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", sha.to_vec())))
        }

        CmdType::Delete => {
            let dtr: DeleteRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "delete: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, paths = ?dtr.paths, "delete: request");
            match storage.delete(&dtr.paths) {
                Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![]))),
                Err(e) => {
                    tracing::warn!(peer, paths = ?dtr.paths, "delete: failed: {e:#}");
                    Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::PathNotFound,
                        &format!("delete failed: {e:#}"),
                        vec![],
                    )))
                }
            }
        }

        CmdType::Copy => {
            let cr: CopyRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "copy: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            let keep = matches!(cr.policy, CopyPolicy::Keep);
            tracing::debug!(peer, src = ?cr.src, dst = %cr.dst, keep, "copy: request");
            match storage.copy(&cr.src, &cr.dst, keep) {
                Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![]))),
                Err(e) => {
                    tracing::warn!(peer, src = ?cr.src, dst = %cr.dst, "copy: failed: {e:#}");
                    Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::NameConflict,
                        &format!("copy failed: {e:#}"),
                        vec![],
                    )))
                }
            }
        }

        CmdType::Move => {
            let mr: MoveRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "move: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, src = ?mr.src, dst_dir = %mr.dst_dir, "move: request");
            match storage.move_(&mr.src, &mr.dst_dir) {
                Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![]))),
                Err(e) => {
                    tracing::warn!(peer, src = ?mr.src, dst_dir = %mr.dst_dir, "move: failed: {e:#}");
                    Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::NameConflict,
                        &format!("move failed: {e:#}"),
                        vec![],
                    )))
                }
            }
        }

        CmdType::Rename => {
            let rr: RenameRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "rename: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, src = %rr.src, new_name = %rr.new_name, "rename: request");
            match storage.rename(&rr.src, &rr.new_name) {
                Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![]))),
                Err(e) => {
                    tracing::warn!(peer, src = %rr.src, new_name = %rr.new_name, "rename: failed: {e:#}");
                    Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::NameConflict,
                        &format!("rename failed: {e:#}"),
                        vec![],
                    )))
                }
            }
        }

        CmdType::Publish => {
            let pr: PublishRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "publish: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, project = ?pr.project, remote_dir = %pr.remote_dir, script = %pr.restart_script_id, files = pr.items.len(), "publish: request");
            // 指定项目时，部署配置以 **projects 表记录为准**（remote_dir / 重启脚本），
            // 使「项目」成为部署配置的单一来源；`items` 只是声明性清单。
            let (remote_dir, restart_script_id, path_prefix) =
                match pr.project.as_deref().map(str::trim) {
                Some(name) if !name.is_empty() => match db.find_project(name) {
                    Ok(Some(p)) => {
                        let base = p.remote_dir.trim_end_matches('/').to_string();
                        (p.remote_dir, p.restart_script, Some(base))
                    }
                    Ok(None) => {
                        return Ok(Some(make_response(
                            req,
                            false,
                            ErrorCode::NameConflict,
                            "project not found",
                            vec![],
                        )))
                    }
                    Err(e) => {
                        return Ok(Some(make_response(
                            req,
                            false,
                            ErrorCode::NameConflict,
                            &format!("project lookup failed: {e}"),
                            vec![],
                        )))
                    }
                },
                _ => (pr.remote_dir, pr.restart_script_id, None),
            };
            if resolve_script(scripts_dir, &restart_script_id).is_none() {
                tracing::warn!(peer, script = %restart_script_id, "publish: restart script not found");
                return Ok(Some(make_response(
                    req,
                    false,
                    ErrorCode::NameConflict,
                    "restart script not found",
                    vec![],
                )));
            }
            tracing::info!(peer, remote_dir = %remote_dir, script = %restart_script_id, "publish: state entered");
            *publish = Some(PublishState {
                remote_dir,
                restart_script_id,
                path_prefix,
                completed: 0,
            });
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![])))
        }

        CmdType::PublishCommit => {
            tracing::debug!(peer, "publish commit: request");
            let _pcr: PublishCommitRequest =
                postcard::from_bytes(&req.body).unwrap_or(PublishCommitRequest {
                    remote_dir: String::new(),
                });
            match publish.take() {
                Some(p) => {
                    let script = resolve_script(scripts_dir, &p.restart_script_id)
                        .unwrap_or_else(|| scripts_dir.join(&p.restart_script_id));
                    tracing::info!(peer, dir = %p.remote_dir, script = %script.display(), completed = p.completed, "publish commit: running restart script");
                    match run_script(&script, &p.remote_dir) {
                        Ok(()) => Ok(Some(make_response(
                            req,
                            true,
                            ErrorCode::Ok,
                            "restart ok",
                            vec![],
                        ))),
                        Err(e) => Ok(Some(make_response(
                            req,
                            false,
                            ErrorCode::RestartFailed,
                            &format!("restart failed: {e}"),
                            vec![],
                        ))),
                    }
                }
                None => Ok(Some(make_response(
                    req,
                    false,
                    ErrorCode::NameConflict,
                    "no active publish",
                    vec![],
                ))),
            }
        }

        CmdType::Rollback => {
            let rr: RollbackRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "rollback: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::info!(peer, dir = %rr.remote_dir, version = %rr.version, "rollback: request");
            match storage.rollback(&rr.remote_dir, &rr.version) {
                Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![]))),
                Err(e) => Ok(Some(make_response(
                    req,
                    false,
                    ErrorCode::PathNotFound,
                    &format!("rollback failed: {e}"),
                    vec![],
                ))),
            }
        }

        CmdType::Tail => {
            let tr: TailRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "tail: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, path = %tr.path, lines = tr.lines, follow = tr.follow, "tail: request");
            let transfer_id = req.seq as u64;
            let n = if tr.lines == 0 { 10 } else { tr.lines as usize };

            // 先推送最后 n 行
            for line in storage.tail_lines(&tr.path, n)? {
                codec
                    .write_frame(&push_frame(transfer_id, &line, false)?)
                    .await?;
            }

            if tr.follow {
                // 跟随模式：轮询文件新增内容，遇 Ctrl::Stop / 连接关闭 / 超时上限结束
                let mut offset = std::fs::metadata(storage.resolve(&tr.path)?)?.len();
                let mut pending = String::new();
                let deadline = Instant::now() + Duration::from_secs(3600);
                loop {
                    match storage.read_from_offset(&tr.path, offset) {
                        Ok((bytes, new_off)) => {
                            offset = new_off;
                            pending.push_str(&String::from_utf8_lossy(&bytes));
                            while let Some(nl) = pending.find('\n') {
                                let line = pending[..nl].to_string();
                                pending.drain(..nl + 1);
                                codec
                                    .write_frame(&push_frame(transfer_id, &line, false)?)
                                    .await?;
                            }
                        }
                        Err(_) => break,
                    }
                    if Instant::now() > deadline {
                        break;
                    }
                    // 等待新帧（Stop）或超时继续轮询
                    match timeout(Duration::from_millis(200), codec.read_frame()).await {
                        Ok(Ok(Some(frame))) => match frame.frame_type {
                            FrameType::Ctrl => {
                                let ctrl: Ctrl = postcard::from_bytes(&frame.payload)?;
                                if matches!(ctrl, Ctrl::Stop) {
                                    break;
                                }
                            }
                            FrameType::CmdRequest => {
                                let r2 = CmdRequest::decode(&frame.payload)?;
                                if r2.cmd == CmdType::Ping {
                                    codec.write_frame(&ctrl_frame(Ctrl::Pong)).await?;
                                } else {
                                    let resp = make_response(
                                        &r2,
                                        false,
                                        ErrorCode::NameConflict,
                                        "busy: tail follow in progress",
                                        vec![],
                                    );
                                    codec.write_frame(&resp).await?;
                                }
                            }
                            _ => {}
                        },
                        Ok(Ok(None)) => break, // 连接关闭
                        Ok(Err(_)) => break,
                        Err(_) => { /* 超时，继续轮询 */ }
                    }
                }
            }

            // 结束标记
            codec
                .write_frame(&push_frame(transfer_id, "", true)?)
                .await?;
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![])))
        }

        CmdType::Grep => {
            let gr: GrepRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "grep: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            tracing::debug!(peer, path = %gr.path, pattern = %gr.pattern, flags = %gr.flags, "grep: request");
            let flags = gr.flags.to_ascii_lowercase();
            let ignore_case = flags.contains('i');
            let show_no = flags.contains('n');
            let needle = if ignore_case {
                gr.pattern.to_lowercase()
            } else {
                gr.pattern.clone()
            };
            let files = storage.walk_files(&gr.path)?;
            let mut results = Vec::new();
            for f in files {
                let rel = storage.rel(&f);
                let data = match std::fs::read(&f) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                for (i, line) in String::from_utf8_lossy(&data).lines().enumerate() {
                    let hay = if ignore_case {
                        line.to_lowercase()
                    } else {
                        line.to_string()
                    };
                    if hay.contains(&needle) {
                        results.push(if show_no {
                            format!("{}:{}: {}", rel, i + 1, line)
                        } else {
                            format!("{}: {}", rel, line)
                        });
                    }
                }
            }
            let body = postcard::to_allocvec(&GrepResponse { lines: results })?;
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", body)))
        }

        CmdType::Edit => {
            let er: EditRequest = match postcard::from_bytes(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "edit: body decode failed: {e}");
                    return Err(e.into());
                }
            };
            match er.content {
                None => {
                    // 读取：响应 body 即文件内容
                    tracing::debug!(peer, path = %er.remote_path, "edit: read");
                    match storage.read_file(&er.remote_path) {
                        Ok(data) => Ok(Some(make_response(req, true, ErrorCode::Ok, "", data))),
                        Err(e) => {
                            tracing::warn!(peer, path = %er.remote_path, "edit read failed: {e:#}");
                            Ok(Some(make_response(
                                req,
                                false,
                                ErrorCode::PathNotFound,
                                &format!("read failed: {e:#}"),
                                vec![],
                            )))
                        }
                    }
                }
                Some(content) => {
                    // 保存：先备份再覆盖
                    tracing::debug!(peer, path = %er.remote_path, bytes = content.len(), "edit: save");
                    match storage.save_with_backup(&er.remote_path, content.as_bytes()) {
                        Ok(()) => Ok(Some(make_response(req, true, ErrorCode::Ok, "saved", vec![]))),
                        Err(e) => {
                            tracing::warn!(peer, path = %er.remote_path, "edit save failed: {e:#}");
                            Ok(Some(make_response(
                                req,
                                false,
                                ErrorCode::NameConflict,
                                &format!("save failed: {e:#}"),
                                vec![],
                            )))
                        }
                    }
                }
            }
        }

        CmdType::Backups => {
            tracing::debug!(peer, "backups: request");
            let versions = match storage.list_backup_versions() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(peer, "backups: failed: {e:#}");
                    return Ok(Some(make_response(
                        req,
                        false,
                        ErrorCode::PathNotFound,
                        &format!("list backups failed: {e:#}"),
                        vec![],
                    )));
                }
            };
            tracing::debug!(peer, versions = versions.len(), "backups: ok");
            let body = postcard::to_allocvec(&BackupsResponse { versions })?;
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", body)))
        }

        CmdType::Ping => {
            tracing::trace!(peer, seq = req.seq, "ping request");
            Ok(Some(make_response(req, true, ErrorCode::Ok, "", vec![])))
        },
    }
}

/// 构造一个 StreamPush 帧（流通道推送一行）。
fn push_frame(transfer_id: u64, line: &str, eof: bool) -> Result<Frame> {
    let sp = StreamPush {
        transfer_id,
        line: line.to_string(),
        eof,
    };
    Ok(Frame::new(
        FrameType::StreamPush,
        FrameFlags::new(),
        postcard::to_allocvec(&sp)?,
    ))
}

/// 在脚本目录中按 `id` 或 `id.sh` 查找可执行脚本。
fn resolve_script(dir: &Path, id: &str) -> Option<PathBuf> {
    let a = dir.join(id);
    if a.is_file() {
        return Some(a);
    }
    let b = dir.join(format!("{id}.sh"));
    if b.is_file() {
        return Some(b);
    }
    None
}

/// 同步执行重启脚本：`sh <script> <remote_dir>`，非零退出视为失败。
fn run_script(script: &Path, remote_dir: &str) -> Result<()> {
    let out = std::process::Command::new("sh")
        .arg(script)
        .arg(remote_dir)
        .output()
        .context("spawn restart script")?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("exit={} stderr={}", out.status, stderr.trim())
    }
}

/// 构造一个 CmdResponse 帧。
fn make_response(req: &CmdRequest, ok: bool, code: ErrorCode, msg: &str, body: Vec<u8>) -> Frame {
    let resp = if ok {
        CmdResponse::success(req.seq, body)
    } else {
        CmdResponse::failure(req.seq, code, msg)
    };
    Frame::new(
        FrameType::CmdResponse,
        FrameFlags::new(),
        resp.encode().unwrap(),
    )
}

fn ctrl_frame(c: Ctrl) -> Frame {
    Frame::new(
        FrameType::Ctrl,
        FrameFlags::new(),
        postcard::to_allocvec(&c).unwrap(),
    )
}
