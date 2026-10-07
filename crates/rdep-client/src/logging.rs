//! 全局 debug 日志初始化。
//!
//! 输出去向（双写）：
//! - stderr：终端 / systemd 启动时直接可见；
//! - 日志文件：GUI 从桌面启动时 stderr 看不到，文件是唯一持久的排查依据。
//!
//! 级别由 `RUST_LOG` 控制（如 `RUST_LOG=trace` / `RUST_LOG=info`），
//! 缺省 `debug`——按运维要求，所有接口/按钮操作都输出完整 debug 日志。
//!
//! 文件位置：`<config_base_dir>/rdep/logs/rdep-client.log`（与 sites.json 同根）。
//! 超过 5 MB 滚动为 `.old`（只保留一代，避免无限增长）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// 单写多路 writer：同一行同时写 stderr 与日志文件。
struct Tee {
    file: Option<Mutex<std::fs::File>>,
    path: PathBuf,
}

impl Tee {
    /// 写入前惰性打开/重开文件：滚动后无需持有陈旧句柄。
    fn write_both(&mut self, buf: &[u8]) -> std::io::Result<()> {
        use std::io::ErrorKind;
        let mut f = match &self.file {
            Some(m) => m.lock().unwrap(),
            None => {
                let mut opts = OpenOptions::new();
                opts.create(true).append(true);
                match opts.open(&self.path) {
                    Ok(f) => self.file.get_or_insert(Mutex::new(f)).lock().unwrap(),
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        // 目录不存在：先建目录再试一次
                        if let Some(p) = self.path.parent() {
                            let _ = std::fs::create_dir_all(p);
                        }
                        match opts.open(&self.path) {
                            Ok(f) => self.file.get_or_insert(Mutex::new(f)).lock().unwrap(),
                            Err(_) => return Err(std::io::Error::new(ErrorKind::Other, "log file unavailable")),
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        f.write_all(buf)?;
        f.flush()
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        // 日志文件写失败（磁盘满/权限）不应影响 stderr 输出
        if let Err(e) = self.write_both(buf) {
            eprintln!("rdep-client: write log file failed: {e}");
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// 日志文件路径（与 sites.json 同根：`<config>/rdep/logs/rdep-client.log`）。
pub fn log_file_path() -> PathBuf {
    crate::sites::config_base_dir()
        .join("rdep")
        .join("logs")
        .join("rdep-client.log")
}

/// 初始化全局 tracing（进程内幂等：重复调用只初始化一次并返回日志路径）。
/// 返回日志文件路径，供启动日志与测试使用。
pub fn init() -> PathBuf {
    let path = log_file_path();
    // 滚动：旧文件 >5MB 则改名 .old（只留一代）
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 5 * 1024 * 1024 {
            let old = path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
    let tee = Tee {
        file: None,
        path: path.clone(),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(tee))
        .try_init();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 初始化后必须能真正落盘：GUI 从桌面启动时 stderr 不可见，文件是唯一证据。
    #[test]
    fn init_creates_log_file() {
        let p = init();
        assert!(p.ends_with("rdep-client.log"), "路径应指向日志文件: {p:?}");
        tracing::debug!("logging smoke test from unit test");
        assert!(p.exists(), "首次日志写入后文件应已创建: {}", p.display());
    }
}
