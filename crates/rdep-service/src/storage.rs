use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rdep_protocol::{FileEntry, LsRequest, LsResponse};

/// 远程文件系统访问层：所有路径都被约束在 `root` 之内（防 `..` 越权）。
pub struct Storage {
    root: PathBuf,
    /// 保留的备份版本数（超过则剪枝最旧的）。
    backup_keep: usize,
}

impl Storage {
    pub fn new(root: PathBuf, backup_keep: usize) -> Result<Self> {
        std::fs::create_dir_all(&root).context("create root dir")?;
        let root = std::fs::canonicalize(&root).context("canonicalize root dir")?;
        Ok(Self {
            root,
            backup_keep: backup_keep.max(1),
        })
    }

    /// 生效的远程根目录（canonicalize 之后）——日志/排障用：
    /// 「客户端看到的 `/` 到底是什么目录」这一疑问的唯一权威答案。
    pub fn root_display(&self) -> String {
        self.root.display().to_string()
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

    // ---- 断点续传暂存区 ----
    // 每个 transfer_id 一个目录，内含 `<index>.chunk` 文件；「已收片集合」= 现存
    // 的 chunk 文件，无需额外位图持久化，天然支持跨连接/跨会话续传。

    fn staging_dir(&self, transfer_id: u64) -> PathBuf {
        self.root.join(".rdep-staging").join(transfer_id.to_string())
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
        let base = self.root.join(".rdep-staging");
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
            let backup_path = self.root.join("backup").join(&version).join(&rel);
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

    /// 列出已有的备份版本（版本号字符串，升序）。版本号即 `backup/` 下的目录名。
    pub fn list_backup_versions(&self) -> Result<Vec<String>> {
        let backup_root = self.root.join("backup");
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
        let backup_dir = self.root.join("backup").join(version).join(rel);
        // 兜底：拼接结果必须仍在 root 内（防止将来新增拼接方式时重蹈覆辙）。
        if !backup_dir.starts_with(&self.root) {
            anyhow::bail!("path escapes root: {}", remote_dir);
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
        let backup_root = self.root.join("backup");
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
