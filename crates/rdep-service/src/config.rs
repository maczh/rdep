use std::path::PathBuf;

/// 服务端运行配置。
#[derive(Clone)]
pub struct ServiceConfig {
    pub listen_addr: String,
    pub root_dir: PathBuf,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub db_path: PathBuf,
    /// 重启脚本目录：发布完成后按 `restart_script_id` 查找并执行。
    pub scripts_dir: PathBuf,
    /// Web 管理后台监听地址（`Some` 时启动 axum 服务）。env `RDEP_WEB_LISTEN`，默认不启用。
    pub web_listen: Option<String>,
    /// 是否向 forwarder 注册（中转模式）。
    pub use_forwarder: bool,
    pub forwarder_host: String,
    pub forwarder_port: u16,
    /// forwarder 的 CA 证书（用于 TLS 校验）。
    pub forwarder_ca: PathBuf,
    /// 中转密钥（与 forwarder 一致）。
    pub relay_token: String,
    /// 本 service 在 forwarder 上的唯一 id / 展示标签。
    pub service_id: String,
    pub service_label: String,
    /// 中转模式下同时维持的常驻注册连接数（= 可并发服务多少 client）。env `RDEP_MAX_SESSIONS`，默认 4。
    pub max_sessions: usize,
    /// 断点续传暂存目录保留时长（小时）；超过则视为中断上传并回收。env `RDEP_STAGING_TTL_HOURS`，默认 24。
    pub staging_ttl_hours: u64,
}

impl ServiceConfig {
    /// 从环境变量加载，缺省给相对路径（相对当前工作目录）。
    ///
    /// - `RDEP_LISTEN` 监听地址（默认 `0.0.0.0:8443`）
    /// - `RDEP_ROOT`   远程根目录（默认 `/`）
    /// - `RDEP_CERT`   服务端证书 PEM（默认 `certs/server.crt`）
    /// - `RDEP_KEY`    服务端私钥 PEM（默认 `certs/server.key`）
    /// - `RDEP_DB`     SQLite 路径（默认 `data/rdep.db`）
    pub fn load() -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()?;
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let get = |k: &str, d: PathBuf| std::env::var(k).map(PathBuf::from).unwrap_or(d);
        Ok(Self {
            listen_addr: std::env::var("RDEP_LISTEN").unwrap_or_else(|_| "0.0.0.0:8443".to_string()),
            root_dir: get("RDEP_ROOT", cwd.join("/")),
            cert_path: get("RDEP_CERT", manifest.join("certs/server.crt")),
            key_path: get("RDEP_KEY", manifest.join("certs/server.key")),
            db_path: get("RDEP_DB", cwd.join("data/rdep.db")),
            scripts_dir: get("RDEP_SCRIPTS", cwd.join("data/scripts")),
            web_listen: std::env::var("RDEP_WEB_LISTEN").ok(),
            use_forwarder: std::env::var("RDEP_USE_FORWARDER")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            forwarder_host: std::env::var("RDEP_FWD_HOST").unwrap_or_default(),
            forwarder_port: std::env::var("RDEP_FWD_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9444),
            forwarder_ca: get("RDEP_FWD_CA", manifest.join("certs/server.crt")),
            relay_token: std::env::var("RDEP_RELAY_TOKEN")
                .unwrap_or_else(|_| "rdep-relay-token".to_string()),
            service_id: std::env::var("RDEP_SERVICE_ID").unwrap_or_default(),
            service_label: std::env::var("RDEP_SERVICE_LABEL").unwrap_or_default(),
            max_sessions: std::env::var("RDEP_MAX_SESSIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4)
                .max(1),
            staging_ttl_hours: std::env::var("RDEP_STAGING_TTL_HOURS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(24),
        })
    }
}
