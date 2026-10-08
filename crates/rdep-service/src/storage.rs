use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rdep_protocol::{FileEntry, LsRequest, LsResponse};

/// 远程文件系统访问层：所有路径都被约束在 `root` 之内（防 `..` 越权）。
///
/// `root` 与 `meta` 是**两个不同的目录**：
/// - `root`  = 对外暴露的部署根目录（客户端看到的 `/`），可能是只读或不可写
///   （如 `RDEP_ROOT=/` 而 service 以普通用户运行）；
/// - `meta`  = service 自己的**工作目录**（断点续传暂存区、备份版本库），
///   必须始终可写，且**不混进部署目录**（否则客户端 `ls /` 会看到
///   `.rdep-staging`、`backup` 这类内部目录，造成困惑）。
///
/// 早期实现把暂存区与备份都建在 `root` 之下，于是 `RDEP_ROOT=/` 时上传直接
/// `Permission denied (os error 13)`（无法在 `/` 下建 `.rdep-staging`）。
/// 现在二者分离；`Storage::new` 保留 `meta = root` 的旧行为以兼容既有测试。
pub struct Storage {
    root: PathBuf,
    /// service 私有工作目录（暂存区 / 备份库）。默认等于 root（兼容旧行为）。
    meta: PathBuf,
    /// 保留的备份版本数（超过则剪枝最旧的）。
    backup_keep: usize,
}

impl Storage {
    pub fn new(root: PathBuf, backup_keep: usize) -> Result<Self> {
        std::fs::create_dir_all(&root).context("create root dir")?;
        let root = std::fs::canonicalize(&root).context("canonicalize root dir")?;
        Ok(Self {
            root: root.clone(),
            meta: root,
            backup_keep: backup_keep.max(1),
        })
    }

    /// 指定独立工作目录（`meta`）构造：`root` 只作部署根，`meta` 承载暂存与备份。
    /// 生产环境**必须**用这个构造函数，否则 `RDEP_ROOT=/` 这类只读根会上传失败。
    pub fn with_meta(root: PathBuf, meta: PathBuf, backup_keep: usize) -> Result<Self> {
        std::fs::create_dir_all(&root).context("create root dir")?;
        let root = std::fs::canonicalize(&root).context("canonicalize root dir")?;
        std::fs::create_dir_all(&meta).with_context(|| format!("create meta dir {}", meta.display()))?;
        let meta = std::fs::canonicalize(&meta).context("canonicalize meta dir")?;
        tracing::info!(
            root = %root.display(),
            meta = %meta.display(),
            "storage: root (deployment) and meta (staging/backup) separated",
        );
        Ok(Self {
            root,
            meta,
            backup_keep: backup_keep.max(1),
        })
    }

    /// 生效的远程根目录（canonicalize 之后）——日志/排障用：
    /// 「客户端看到的 `/` 到底是什么目录」这一疑问的唯一权威答案。
    pub fn root_display(&self) -> String {
        self.root.display().to_string()
    }

    /// 私有工作目录（暂存区 / 备份库）——排障「上传 Permission denied」时先看它是否可写。
    pub fn meta_display(&self) -> String {
        self.meta.display().to_string()
    }

    /// 将协议层传来的（可能含前导 `/` 与 `..`）路径解析为 root 内的绝对路径。
    ///
    /// 按路径分量逐级拼接，遇到 `..` 直接拒绝（防止越权逃逸）；不依赖父目录是否已存在，
    /// 因此既适用于新建（mkdir 多级）也适用于已存在的文件。
    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        let rel = p.trim_start_matches('/');
        let mut out = self.root.clone();
        for comp in rel.split('/') {
            if comp.is_empty() || comp == "." {
                continue;
            }
            if comp == ".." {
                tracing::warn!(path = %p, "resolve: rejected path escape attempt");
                anyhow::bail!("path escape not allowed: {}", p);
            }
            out.push(comp);
        }
        // root 在构造时已 canonicalize，且仅做 push，必然仍在 root 内；
        // 额外兜底校验，防止极端情况。
        if !out.starts_with(&self.root) {
            tracing::error!(path = %p, resolved = %out.display(), "resolve: path escaped root");
            anyhow::bail!("path escapes root: {}", p);
        }
        Ok(out)
    }

    pub fn ls(&self, req: &LsRequest) -> Result<LsResponse> {
        let path = self.resolve(&req.path)?;
        tracing::debug!(
            requested = %req.path,
            resolved = %path.display(),
            recursive = req.recursive,
            "storage.ls: enter"
        );
        let mut entries = Vec::new();
        if req.recursive && path.is_dir() {
            // 递归：返回目录下所有文件的「相对路径」（供客户端同步做差异比对）
            // 不跟随符号链接（不进入链接目录），避免链接环无限递归。
            fn walk(dir: &Path, base: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
                for e in std::fs::read_dir(dir)? {
                    let e = e?;
                    let p = e.path();
                    let meta = match std::fs::symlink_metadata(&p) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if meta.file_type().is_symlink() {
                        continue; // 跳过符号链接
                    }
                    if meta.is_dir() {
                        walk(&p, base, out)?;
                    } else {
                        out.push(FileEntry {
                            name: p
                                .strip_prefix(base)
                                .unwrap_or(&p)
                                .to_string_lossy()
                                .into_owned(),
                            is_dir: false,
                            size: meta.len(),
                            mtime: meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0),
                            mode: mode_of(&meta),
                        });
                    }
                }
                Ok(())
            }
            walk(&path, &path, &mut entries)?;
            entries.sort_by(|a, b| a.name.cmp(&b.name));
        } else if path.is_dir() {
            for e in std::fs::read_dir(&path)? {
                let e = e?;
                let meta = e.metadata()?;
                entries.push(FileEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                    mtime: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                    mode: mode_of(&meta),
                });
            }
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
        } else {
            tracing::debug!(requested = %req.path, resolved = %path.display(), "storage.ls: not a directory (returns empty)");
        }
        tracing::debug!(requested = %req.path, entries = entries.len(), "storage.ls: done");
        Ok(LsResponse { entries })
    }

    pub fn mkdir(&self, paths: &[String]) -> Result<()> {
        for p in paths {
            let path = self.resolve(p)?;
            tracing::debug!(requested = %p, resolved = %path.display(), "storage.mkdir");
            std::fs::create_dir_all(&path).with_context(|| format!("mkdir {}", p))?;
        }
        Ok(())
    }

    /// 将已合并好的文件内容写入目标路径（自动建父目录）。
    ///
    /// 若目标本身是一个符号链接，先删除该链接再写普通文件——**不穿过链接写**，
    /// 避免被诱导写到 root 之外（或覆盖链接指向的真实文件）。
    pub fn save(&self, dest: &Path, data: &[u8]) -> Result<()> {
        tracing::debug!(dest = %dest.display(), bytes = data.len(), "storage.save");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_if_symlink(dest);
        std::fs::write(dest, data).context("write file")?;
        Ok(())
    }

    /// 应用文件权限位（unix）。`mode == 0` 表示不设置（保持默认）。
    #[cfg(unix)]
    pub fn apply_mode(&self, path: &Path, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if mode != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))
                .with_context(|| format!("set mode on {}", path.display()))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    pub fn apply_mode(&self, _path: &Path, _mode: u32) -> Result<()> {
        Ok(())
    }

    /// 远端 `chmod`：在 root 内的 `path` 上设置权限位（unix `st_mode & 0o777`）。
    ///
    /// 路径必须经 `resolve()` 校验（防越权），`mode == 0` 直接忽略（调用方不该传 0）。
    pub fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        if mode == 0 {
            anyhow::bail!("chmod: invalid mode 0 (use the file's existing mode)");
        }
        let dest = self.resolve(path)?;
        tracing::debug!(requested = %path, resolved = %dest.display(), mode = format!("{mode:o}"), "storage.chmod");
        self.apply_mode(&dest, mode)
            .with_context(|| format!("chmod {} to {mode:o}", path))
    }

    /// 把 `path` 的修改时间（mtime）设为 `mtime`（Unix 秒）。用于上传后「保留原文件时间」。
    ///
    /// 路径经 `resolve()` 校验。服务端通常用 `std::fs::set_modified`（跨平台），
    /// 在 unix 上等价设置 mtime。`mtime <= 0` 视为无效，忽略。
    pub fn set_mtime(&self, path: &str, mtime: i64) -> Result<()> {
        if mtime <= 0 {
            return Ok(()); // 没有有意义的时间戳，保持现状，不算错误
        }
        let dest = self.resolve(path)?;
        let dur = std::time::Duration::from_secs(mtime as u64);
        let system_time = std::time::UNIX_EPOCH + dur;
        tracing::debug!(requested = %path, resolved = %dest.display(), mtime, "storage.set_mtime");
        // 注意：本工具链 std 未提供 `std::fs::set_modified`，改用 `set_times` +
        // `FileTimes::set_modified`（等价设置 mtime）。
        std::fs::set_times(&dest, std::fs::FileTimes::new().set_modified(system_time))
            .with_context(|| format!("set mtime on {} to {mtime}", path))
    }

    /// 读取文件并一并返回其**权限位**与**修改时间**，供下载时把属性随文件交还客户端。
    pub fn read_file_meta(&self, remote_path: &str) -> Result<(Vec<u8>, u32, i64)> {
        let path = self.resolve(remote_path)?;
        let data = std::fs::read(&path).with_context(|| format!("read {}", remote_path))?;
        let meta = std::fs::metadata(&path)?;
        let mode = mode_of(&meta);
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok((data, mode, mtime))
    }

    // ---- 断点续传暂存区 ----
    // 每个 transfer_id 一个目录，内含 `<index>.chunk` 文件；「已收片集合」= 现存
    // 的 chunk 文件，无需额外位图持久化，天然支持跨连接/跨会话续传。

    fn staging_dir(&self, transfer_id: u64) -> PathBuf {
        // 暂存区在 meta（service 私有、保证可写），绝不放在 root 之下
        self.meta.join(".rdep-staging").join(transfer_id.to_string())
    }

    /// 列出某传输已暂存的分片序号（升序）。
    pub fn received_chunks(&self, transfer_id: u64) -> Result<Vec<u32>> {
        let dir = self.staging_dir(transfer_id);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut idx: Vec<u32> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".chunk")?.parse::<u32>().ok()
            })
            .collect();
        idx.sort_unstable();
        Ok(idx)
    }

    /// 暂存一个分片（先写临时文件再原子改名，避免半片被当作已收）。
    pub fn stage_chunk(&self, transfer_id: u64, index: u32, data: &[u8]) -> Result<()> {
        let dir = self.staging_dir(transfer_id);
        std::fs::create_dir_all(&dir)?;
        let final_path = dir.join(format!("{index}.chunk"));
        let tmp_path = dir.join(format!("{index}.chunk.tmp"));
        std::fs::write(&tmp_path, data).context("write staged chunk")?;
        std::fs::rename(&tmp_path, &final_path).context("commit staged chunk")?;
        Ok(())
    }

    /// 按 index 顺序合并暂存分片（0..total_chunks）。
    pub fn assemble_staged(&self, transfer_id: u64, total_chunks: u32) -> Result<Vec<u8>> {
        let dir = self.staging_dir(transfer_id);
        let mut out = Vec::new();
        for i in 0..total_chunks {
            let p = dir.join(format!("{i}.chunk"));
            let data = std::fs::read(&p)
                .with_context(|| format!("read staged chunk {i} (transfer {transfer_id})"))?;
            out.extend_from_slice(&data);
        }
        Ok(out)
    }

    /// 清理某传输的暂存目录（commit 成功后调用；错误忽略）。
    pub fn cleanup_staging(&self, transfer_id: u64) {
        let _ = std::fs::remove_dir_all(self.staging_dir(transfer_id));
    }

    /// 清理「陈旧」的暂存目录：超过 `max_age` 未被修改的传输视为已放弃
    /// （client 中断上传后再未续传），回收其磁盘占用。返回回收的目录数。
    pub fn cleanup_stale_staging(&self, max_age: std::time::Duration) -> Result<usize> {
        let base = self.meta.join(".rdep-staging");
        if !base.is_dir() {
            return Ok(0);
        }
        let now = std::time::SystemTime::now();
        let mut removed = 0usize;
        for entry in std::fs::read_dir(&base)?.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            // 以目录 mtime 近似「最后活动」时间（每次 stage_chunk 写文件会刷新目录 mtime）
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok();
            let stale = match modified {
                Some(t) => now
                    .duration_since(t)
                    .map(|d| d > max_age)
                    .unwrap_or(false),
                None => false,
            };
            if stale {
                if std::fs::remove_dir_all(&p).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// 写入新内容前，先将已有文件备份到 `backup/<YYMMDDHHmm>/<相对路径>`，
    /// 并在超出保留上限时剪枝最旧的版本。
    pub fn save_with_backup(&self, remote_path: &str, data: &[u8]) -> Result<()> {
        let dest = self.resolve(remote_path)?;
        if dest.exists() {
            let version = self.version_stamp();
            let rel = dest
                .strip_prefix(&self.root)
                .unwrap_or(&dest)
                .to_path_buf();
            let backup_path = self.backup_root().join(&version).join(&rel);
            if let Some(p) = backup_path.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&dest, &backup_path)
                .with_context(|| format!("backup {} -> {:?}", remote_path, backup_path))?;
            tracing::debug!(version = %version, backup = %backup_path.display(), "backup saved");
            self.prune_backups()?;
        }
        self.save(&dest, data)
    }

    /// 备份库根目录（在 meta 下，不污染部署目录）。
    fn backup_root(&self) -> PathBuf {
        self.meta.join("backup")
    }

    /// 列出已有的备份版本（版本号字符串，升序）。版本号即 `backup/` 下的目录名。
    pub fn list_backup_versions(&self) -> Result<Vec<String>> {
        let backup_root = self.backup_root();
        if !backup_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut v: Vec<String> = std::fs::read_dir(&backup_root)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        v.sort();
        Ok(v)
    }

    /// 将指定版本的备份恢复到 `remote_dir`（覆盖当前文件）。
    pub fn rollback(&self, remote_dir: &str, version: &str) -> Result<()> {
        // 防御：版本号只允许数字，避免路径注入
        if !version.chars().all(|c| c.is_ascii_digit()) {
            anyhow::bail!("invalid version: {}", version);
        }
        // 关键：remote_dir 必须经 resolve() 校验后再用于拼接备份路径。
        // 否则 `remote_dir = "../../../../etc"` 会拼出 root 之外的备份源目录，
        // 造成任意目录读取（copy 到部署目录后即可下载）。
        let dest_base = self.resolve(remote_dir)?;
        let rel = dest_base.strip_prefix(&self.root).unwrap_or(&dest_base);
        let backup_dir = self.backup_root().join(version).join(rel);
        // 兜底：备份源必须仍在 meta 工作目录内（防止任意目录读取）。
        if !backup_dir.starts_with(&self.meta) {
            tracing::error!(dir = %remote_dir, version, "rollback: backup source escapes meta dir");
            anyhow::bail!("path escapes backup store: {}", remote_dir);
        }
        if !backup_dir.is_dir() {
            anyhow::bail!("backup version not found: {}", version);
        }
        tracing::info!(dir = %remote_dir, version, from = %backup_dir.display(), to = %dest_base.display(), "rollback: restoring");
        copy_recursive(&backup_dir, &dest_base)?;
        Ok(())
    }

    /// 把 root 内的绝对路径转为相对路径字符串（用于回显）。
    pub fn rel(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.root)
            .unwrap_or(abs)
            .to_string_lossy()
            .into_owned()
    }

    /// 递归列出目录下的所有普通文件（绝对路径）；若 `remote_path` 是文件则返回它自身。
    /// 不跟随符号链接（不进入链接目录），避免链接环导致无限递归。
    pub fn walk_files(&self, remote_path: &str) -> Result<Vec<PathBuf>> {
        let path = self.resolve(remote_path)?;
        let mut out = Vec::new();
        let start_meta = match meta_nofollow(&path) {
            Some(m) => m,
            None => return Ok(out),
        };
        if start_meta.file_type().is_symlink() {
            return Ok(out); // 不跟随根链接
        }
        if start_meta.is_file() {
            out.push(path);
        } else if start_meta.is_dir() {
            let mut stack = vec![path];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir)? {
                    let e = e?;
                    let p = e.path();
                    // 用 symlink_metadata：链接目录不进入，普通目录进入，普通文件收集
                    match meta_nofollow(&p) {
                        Some(m) if m.file_type().is_symlink() => {}
                        Some(m) if m.is_dir() => stack.push(p),
                        Some(_) => out.push(p),
                        None => {}
                    }
                }
            }
            out.sort();
        }
        Ok(out)
    }

    /// 读取文件的所有行（失败则报错）。
    pub fn read_lines(&self, remote_path: &str) -> Result<Vec<String>> {
        let data = self.read_file(remote_path)?;
        let text = String::from_utf8_lossy(&data);
        let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        // 文件以换行结尾时 lines() 不会多出空行，这里保持原样即可
        lines.shrink_to_fit();
        Ok(lines)
    }

    /// 读取文件最后 `n` 行（tail 初始输出）。
    pub fn tail_lines(&self, remote_path: &str, n: usize) -> Result<Vec<String>> {
        let mut lines = self.read_lines(remote_path)?;
        if lines.len() > n {
            lines = lines.split_off(lines.len() - n);
        }
        Ok(lines)
    }

    /// 从字节偏移 `offset` 读取到文件末尾，返回 `(新增字节, 新偏移)`。
    /// 若文件被截断（当前长度 < offset），则从头读取并返回新偏移（用于 tail follow 检测轮转）。
    pub fn read_from_offset(&self, remote_path: &str, offset: u64) -> Result<(Vec<u8>, u64)> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.resolve(remote_path)?;
        let mut f = std::fs::File::open(&path)
            .with_context(|| format!("open {}", remote_path))?;
        let len = f.metadata()?.len();
        let start = if len < offset { 0 } else { offset };
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let new_offset = start + buf.len() as u64;
        Ok((buf, new_offset))
    }

    /// 当前时间版本戳 `YYMMDDHHmm`（用于备份目录命名，定宽可直接按字典序排序）。
    fn version_stamp(&self) -> String {
        chrono::Local::now().format("%y%m%d%H%M").to_string()
    }

    /// 剪枝最旧的备份版本，仅保留 `backup_keep` 个。
    fn prune_backups(&self) -> Result<()> {
        let backup_root = self.backup_root();
        if !backup_root.is_dir() {
            return Ok(());
        }
        let mut versions: Vec<PathBuf> = std::fs::read_dir(&backup_root)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        versions.sort();
        if versions.len() > self.backup_keep {
            for old in &versions[..versions.len() - self.backup_keep] {
                let _ = std::fs::remove_dir_all(old);
            }
        }
        Ok(())
    }

    pub fn read_file(&self, remote_path: &str) -> Result<Vec<u8>> {
        let path = self.resolve(remote_path)?;
        std::fs::read(&path).with_context(|| format!("read {}", remote_path))
    }

    pub fn delete(&self, paths: &[String]) -> Result<()> {
        for p in paths {
            let path = self.resolve(p)?;
            tracing::debug!(requested = %p, resolved = %path.display(), "storage.delete");
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    pub fn copy(&self, src: &[String], dst: &str, keep: bool) -> Result<()> {
        let dst_path = self.resolve(dst)?;
        if src.len() == 1 {
            let sp = self.resolve(&src[0])?;
            let target = if dst_path.is_dir() || keep {
                dst_path.join(sp.file_name().unwrap_or_default())
            } else {
                dst_path
            };
            copy_recursive(&sp, &target)?;
        } else {
            std::fs::create_dir_all(&dst_path)?;
            for s in src {
                let sp = self.resolve(s)?;
                copy_recursive(&sp, &dst_path.join(sp.file_name().unwrap_or_default()))?;
            }
        }
        Ok(())
    }

    pub fn move_(&self, src: &[String], dst_dir: &str) -> Result<()> {
        let dst = self.resolve(dst_dir)?;
        std::fs::create_dir_all(&dst)?;
        for s in src {
            let sp = self.resolve(s)?;
            let target = dst.join(sp.file_name().unwrap_or_default());
            std::fs::rename(&sp, &target)?;
        }
        Ok(())
    }

    pub fn rename(&self, src: &str, new_name: &str) -> Result<()> {
        let sp = self.resolve(src)?;
        let parent = sp
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| anyhow::anyhow!("cannot rename root"))?;
        let target = parent.join(new_name);
        tracing::debug!(src = %src, target = %target.display(), "storage.rename");
        std::fs::rename(&sp, &target)?;
        Ok(())
    }
}

/// 若 `p` 是符号链接则删除它（不跟随）。用于写入前清理，避免写穿链接。
fn remove_if_symlink(p: &Path) {
    if std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(p);
    }
}

/// 取不跟随符号链接的元数据（`is_dir` 基于链接本身而非目标）。
fn meta_nofollow(p: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(p).ok()
}

/// 递归复制：不跟随符号链接（源是链接则跳过，目标是链接则先删再写），
/// 防止链接环导致无限递归、以及写穿链接到 root 之外。
fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    let m = match meta_nofollow(src) {
        Some(m) => m,
        None => return Ok(()), // 源不存在/不可读，跳过
    };
    if m.file_type().is_symlink() {
        return Ok(()); // 不复制、不跟随符号链接
    }
    if m.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_if_symlink(dst);
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// 取文件权限位（unix `mode & 0o777`；非 unix 返回 0）。
#[cfg(unix)]
fn mode_of(m: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn mode_of(_m: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(test)]
mod meta_dir_tests {
    //! 回归：断点续传暂存区与备份库必须落在 **meta 工作目录**，不得出现在部署根下。
    //!
    //! 背景（`RDEP_ROOT=/` 上传报 `Permission denied (os error 13)`）：
    //! 早期实现把暂存区/备份都建在 root 之下，部署根不可写时上传直接失败；
    //! 同时客户端 `ls /` 会看到 `.rdep-staging`、`backup` 这类内部目录。

    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rdep-meta-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn staging_and_backup_do_not_pollute_root() {
        let base = tmp("sep");
        let root = base.join("root");
        let meta = base.join("meta");
        std::fs::create_dir_all(&root).unwrap();

        let s = Storage::with_meta(root.clone(), meta.clone(), 10).expect("storage");

        // 写文件 + 触发一次备份（覆盖已有文件才会备份）
        // 用目录承载目标文件：rollback 的语义是「把某版本恢复到 remote_dir」（目录级）
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app").join("app.txt"), b"v0").unwrap();
        s.save_with_backup("/app/app.txt", b"v1").expect("save with backup");

        // 断点续传暂存
        s.stage_chunk(4242, 0, b"chunk").expect("stage chunk");

        // 部署根下不得出现内部目录
        assert!(
            !root.join(".rdep-staging").exists(),
            "暂存区不得建在部署根下"
        );
        assert!(!root.join("backup").exists(), "备份库不得建在部署根下");

        // 它们应该在 meta 下，且内容正确
        assert!(meta.join(".rdep-staging").join("4242").is_dir(), "暂存区应在 meta 下");
        assert!(meta.join("backup").is_dir(), "备份库应在 meta 下");
        assert_eq!(std::fs::read(root.join("app").join("app.txt")).unwrap(), b"v1");
        let versions = s.list_backup_versions().expect("list versions");
        assert_eq!(versions.len(), 1, "应产生一个备份版本");

        // ls 只看到真实目录（这是用户在客户端看到的 `/`）
        let listed = s
            .ls(&LsRequest { path: "/".into(), recursive: false })
            .expect("ls");
        let names: Vec<&str> = listed.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["app"], "客户端看到的 / 不应含内部目录: {names:?}");

        // 回滚仍可用（备份源在 meta，恢复到 root 下的目录）
        s.rollback("/app", &versions[0]).expect("rollback");
        assert_eq!(std::fs::read(root.join("app").join("app.txt")).unwrap(), b"v0");

        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    /// 断点续传暂存区 GC：大 TTL 保留新鲜暂存，TTL=0 全部回收。
    /// （不依赖 mtime 改写，用不同 TTL 覆盖「保留」与「回收」两条分支。）
    #[test]
    fn stale_staging_gc() {
        let root = std::env::temp_dir().join(format!("rdep-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let s = Storage::new(root.clone(), 10).expect("storage");

        // 两个刚创建的「新鲜」暂存目录
        s.stage_chunk(111, 0, b"a").expect("stage 111");
        s.stage_chunk(222, 0, b"b").expect("stage 222");
        let d111 = root.join(".rdep-staging").join("111");
        let d222 = root.join(".rdep-staging").join("222");
        assert!(d111.exists() && d222.exists());

        // 大 TTL（1 年）：都还「新鲜」，不应回收
        let n = s
            .cleanup_stale_staging(std::time::Duration::from_secs(365 * 24 * 3600))
            .expect("gc keep");
        assert_eq!(n, 0, "fresh staging must be kept under large TTL");
        assert!(d111.exists() && d222.exists());

        // TTL=0：所有已存在目录都视为过期，全回收
        std::thread::sleep(std::time::Duration::from_millis(5)); // 确保 age>0
        let n = s
            .cleanup_stale_staging(std::time::Duration::from_secs(0))
            .expect("gc reap");
        assert_eq!(n, 2, "all stale staging should be reaped at TTL=0");
        assert!(!d111.exists() && !d222.exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod preservation_tests {
    use super::*;

    fn tmp_storage(tag: &str) -> (Storage, PathBuf) {
        let root = std::env::temp_dir().join(format!("rdep-presv-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let meta = root.join("meta");
        let s = Storage::with_meta(root.clone(), meta, 10).expect("storage");
        (s, root)
    }

    /// 文件属性往返：保存内容 → set_mtime + apply_mode 后，read_file_meta 能取回
    /// 正确的模式与时间（对应「上传/编辑后保持文件属性、时间与原文件相同」）。
    #[test]
    fn mtime_and_mode_preserved_through_meta() {
        let (s, root) = tmp_storage("meta");
        let rel = "conf/app.yaml";
        let full = root.join("conf").join("app.yaml");
        // 直接落盘内容（绕开 resolve 路径限制，测试语义用真实文件）
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, b"version: 1").unwrap();

        let want_mtime: i64 = 1_700_000_000; // 固定秒级时间戳
        s.set_mtime(rel, want_mtime).expect("set mtime");

        #[cfg(unix)]
        s.apply_mode(&full, 0o640).expect("set mode");

        let (data, mode, mtime) = s.read_file_meta(rel).expect("read meta");
        assert_eq!(data, b"version: 1");
        assert_eq!(mtime, want_mtime, "mtime 应被 read_file_meta 取回");
        #[cfg(unix)]
        assert_eq!(mode & 0o777, 0o640, "mode 应被 read_file_meta 取回");

        // set_mtime(<=0) 应为 no-op（不报错、不改时间）
        s.set_mtime(rel, 0).expect("set mtime 0 is noop");
        let (_, _, mtime2) = s.read_file_meta(rel).expect("read meta again");
        assert_eq!(mtime2, want_mtime, "mtime<=0 不应改变已有时间");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// chmod：远端执行 chmod 应改变文件权限，且不影响内容。
    #[test]
    fn chmod_changes_mode() {
        let (s, root) = tmp_storage("chmod");
        let rel = "bin/run.sh";
        let full = root.join("bin").join("run.sh");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, b"#!/bin/sh\necho hi").unwrap();
        #[cfg(unix)]
        s.apply_mode(&full, 0o644).unwrap();

        s.chmod(rel, 0o755).expect("chmod");
        let (data, mode, _) = s.read_file_meta(rel).expect("read meta");
        assert_eq!(data, b"#!/bin/sh\necho hi", "chmod 不应改内容");
        #[cfg(unix)]
        assert_eq!(mode & 0o777, 0o755, "chmod 后应变为 0755");

        // mode=0 视为非法，应返回错误
        assert!(s.chmod(rel, 0).is_err(), "chmod 0 应被拒绝");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 编辑保存：save_with_backup 先备份原内容，再用新内容覆盖原文件名。
    /// 对应「编辑保存 = 先改名备份，再上传新内容成原文件名」。
    #[test]
    fn edit_save_backs_up_then_overwrites() {
        let (s, root) = tmp_storage("editbak");
        let rel = "app/config.txt";
        let full = root.join("app").join("config.txt");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, b"OLD CONTENT").unwrap();

        s.save_with_backup(rel, b"NEW CONTENT").expect("save with backup");

        // 原文件名落新内容
        assert_eq!(std::fs::read(&full).unwrap(), b"NEW CONTENT");
        // 备份库中保留了旧内容
        let backup_root = root.join("meta").join("backup");
        assert!(backup_root.is_dir(), "应生成备份目录");
        let mut found_old = false;
        for entry in std::fs::read_dir(&backup_root).unwrap().flatten() {
            let version_dir = entry.path();
            if version_dir.is_dir() {
                let bak = version_dir.join("app").join("config.txt");
                if bak.is_file() && std::fs::read(&bak).unwrap() == b"OLD CONTENT" {
                    found_old = true;
                }
            }
        }
        assert!(found_old, "备份中应保留原始 OLD CONTENT");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(all(test, unix))]
mod symlink_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// 符号链接安全：
    /// 1) 链接环 / 链接目录不导致无限递归（walk_files 只收集真实普通文件）；
    /// 2) 写入不「穿」过目标符号链接（save 替换链接为普通文件，不改链接指向的真实文件）。
    #[test]
    fn symlink_safety() {
        let root = std::env::temp_dir().join(format!("rdep-sl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        std::fs::write(root.join("d/real.txt"), b"real").unwrap();
        // 链接环：d/loop -> d
        symlink(root.join("d"), root.join("d/loop")).unwrap();
        // 一个指向真实文件的链接
        symlink(root.join("d/real.txt"), root.join("link.txt")).unwrap();

        let s = Storage::new(root.clone(), 10).unwrap();

        // walk_files：只收集真实普通文件；不进入链接目录（无环、含 real.txt、不含 loop 展开）
        let files = s.walk_files("/").unwrap();
        let names: Vec<String> = files.iter().map(|f| s.rel(f)).collect();
        assert!(names.contains(&"d/real.txt".to_string()), "find real: {:?}", names);
        assert!(
            !names.iter().any(|n| n.contains("loop")),
            "must not descend into symlinked dir (loop): {:?}",
            names
        );
        assert_eq!(names.iter().filter(|n| *n == "d/real.txt").count(), 1, "no dup: {:?}", names);

        // save 到一个「指向真实文件的链接」：应替换为普通文件，不写穿到 real.txt
        s.save(&s.resolve("/link.txt").unwrap(), b"overwritten").unwrap();
        let m = std::fs::symlink_metadata(root.join("link.txt")).unwrap();
        assert!(!m.file_type().is_symlink(), "link.txt should become a regular file");
        assert_eq!(std::fs::read(root.join("d/real.txt")).unwrap(), b"real", "real.txt must be untouched");
        assert_eq!(std::fs::read(root.join("link.txt")).unwrap(), b"overwritten");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod security_tests {
    //! 安全边界的**对抗性**测试。
    //!
    //! `resolve()` 是整个 service 的路径安全边界（所有文件操作都经由它），
    //! 但此前**没有任何针对它的攻击性用例**。这里补齐：
    //! - 各种形式的路径穿越（`..`、多重、混合、尾随）一律被拒
    //! - 正常路径（相对/绝对/带尾斜杠/`//`）仍可用
    //! - `rollback` 的备份源路径不得逃出 root（曾真实存在的任意目录读取漏洞）
    //! - `rollback` 的版本号注入（路径注入）

    use super::*;

    fn tmp_storage(tag: &str) -> (Storage, PathBuf) {
        let root = std::env::temp_dir().join(format!("rdep-sec-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let s = Storage::new(root.clone(), 10).expect("storage");
        let real_root = s.root.clone();
        (s, real_root)
    }

    /// 各种路径穿越写法必须全部被 `resolve()` 拒绝。
    #[test]
    fn resolve_rejects_traversal() {
        let (s, root) = tmp_storage("trav");
        let attacks = [
            "/..",
            "/../",
            "/../etc/passwd",
            "/a/../../etc/passwd",
            "/a/b/../../../etc/passwd",
            "..",
            "../etc",
            "/a/..",
            "/./../x",
            "/a/./../../x",
            "//../etc",
            "/a//..//..//etc",
        ];
        for a in attacks {
            let r = s.resolve(a);
            assert!(r.is_err(), "应拒绝路径穿越 {a:?}，却得到 {r:?}");
            // 双保险：即便未来 resolve 被改坏，也不会有结果落在 root 之外
            if let Ok(p) = &r {
                assert!(
                    p.starts_with(&root),
                    "{a:?} 解析到 {} 逃出了 root {root:?}",
                    p.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 正常路径必须仍能解析（防御不能误伤）。
    #[test]
    fn resolve_accepts_legit_paths() {
        let (s, root) = tmp_storage("ok");
        for p in ["/", "", "/a", "/a/b/c.txt", "a/b", "/a/", "/a//b"] {
            let r = s
                .resolve(p)
                .unwrap_or_else(|e| panic!("合法路径 {p:?} 不应被拒: {e}"));
            assert!(
                r.starts_with(&root),
                "{p:?} 解析结果 {r:?} 必须仍在 root {root:?} 内"
            );
        }
        assert_eq!(s.resolve("/").unwrap(), root);
        assert_eq!(s.resolve("/a/b.txt").unwrap(), root.join("a").join("b.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 回滚的**备份源路径**不得逃出 root。
    ///
    /// 回归测试：此前 `backup_dir` 由原始 `remote_dir` 字符串直接拼接，
    /// `remote_dir = "../../../../etc"` 可让 `copy_recursive` 把 root 之外的目录
    /// 复制进部署目录（进而可被下载）——即任意目录读取。
    #[test]
    fn rollback_source_cannot_escape_root() {
        let (s, root) = tmp_storage("rb");

        // 制造一个真实的备份版本目录（模拟一次自动备份）
        let ver = "2601071200";
        std::fs::create_dir_all(root.join("backup").join(ver)).unwrap();
        // 目标目录
        std::fs::create_dir_all(root.join("app")).unwrap();

        // 造一个 root 之外、攻击者可读的目录作为「受害者」目录
        let outside = root.parent().unwrap().join(format!(
            "rdep-sec-outside-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"TOP-SECRET").unwrap();

        // 计算能把 backup 源指向 outside 的相对穿越串
        let depth = root.components().count();
        let ups = "../".repeat(depth + 2);
        let evil = format!("{ups}{}", outside.file_name().unwrap().to_string_lossy());

        let r = s.rollback(&evil, ver);
        assert!(
            r.is_err(),
            "回滚源路径逃出 root 时必须报错，却成功了：{r:?}"
        );

        // 关键断言：受害者目录的内容绝不能出现在部署目录里
        let leaked = root.join("app").join("secret.txt");
        assert!(
            !leaked.exists(),
            "发生了越权读取：{} 不应存在",
            leaked.display()
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 回滚版本号只允许数字（防路径注入）。
    #[test]
    fn rollback_rejects_version_injection() {
        let (s, root) = tmp_storage("ver");
        std::fs::create_dir_all(root.join("backup")).unwrap();
        let bad_versions = [
            "../../etc",
            "..",
            "2601071200/../../..",
            "abc",
            "2601071200 ",
            "",
            "0x10",
            "2601071200;rm -rf /",
        ];
        for v in bad_versions {
            let r = s.rollback("/app", v);
            assert!(r.is_err(), "版本号 {v:?} 应被拒绝，却得到 {r:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 备份写入的路径同样不得逃出 root（save_with_backup 走 resolve，回归防护）。
    #[test]
    fn backup_write_cannot_escape_root() {
        let (s, root) = tmp_storage("bw");
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app").join("f.txt"), b"v0").unwrap();

        // 正常备份应落在 root/backup/<版本>/app/f.txt
        s.save_with_backup("/app/f.txt", b"v1").expect("backup should succeed");

        let ver_dirs: Vec<_> = std::fs::read_dir(root.join("backup"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(ver_dirs.len(), 1, "应恰好产生一个版本目录");
        let backed_up = ver_dirs[0].join("app").join("f.txt");
        assert!(backed_up.is_file(), "备份文件应存在于 {backed_up:?}");
        assert_eq!(std::fs::read(&backed_up).unwrap(), b"v0");
        // 目标被更新
        assert_eq!(std::fs::read(root.join("app/f.txt")).unwrap(), b"v1");

        let _ = std::fs::remove_dir_all(&root);
    }
}
