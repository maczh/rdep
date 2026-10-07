use std::path::PathBuf;

use anyhow::Result;

/// forwarder 运行配置（全部来自环境变量，便于 systemd / docker 部署）。
#[derive(Clone)]
pub struct ForwarderConfig {
    /// 接受 service 注册的监听地址。
    pub service_listen: String,
    /// 接受 client 会话的监听地址。
    pub client_listen: String,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub db_path: PathBuf,
    /// service / client 共用的中转密钥。
    pub relay_token: String,
    /// Web 管理后台监听地址（`Some` 时启动 axum 服务）。env `RDEP_FWD_WEB_LISTEN`，默认不启用。
    pub web_listen: Option<String>,
}

impl ForwarderConfig {
    /// 从环境变量加载：
    /// - `RDEP_FWD_SERVICE_LISTEN`（默认 `0.0.0.0:9444`）
    /// - `RDEP_FWD_CLIENT_LISTEN`（默认 `0.0.0.0:9443`）
    /// - `RDEP_FWD_CERT` / `RDEP_FWD_KEY`（默认 `certs/server.crt` / `certs/server.key`）
    /// - `RDEP_FWD_DB`（默认 `data/forwarder.db`）
    /// - `RDEP_RELAY_TOKEN`（默认 `rdep-relay-token`）
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let get = |k: &str, d: PathBuf| std::env::var(k).map(PathBuf::from).unwrap_or(d);
        Ok(Self {
            service_listen: std::env::var("RDEP_FWD_SERVICE_LISTEN")
                .unwrap_or_else(|_| "0.0.0.0:9444".to_string()),
            client_listen: std::env::var("RDEP_FWD_CLIENT_LISTEN")
                .unwrap_or_else(|_| "0.0.0.0:9443".to_string()),
            cert_path: get("RDEP_FWD_CERT", manifest.join("certs/server.crt")),
            key_path: get("RDEP_FWD_KEY", manifest.join("certs/server.key")),
            db_path: get("RDEP_FWD_DB", cwd.join("data/forwarder.db")),
            relay_token: std::env::var("RDEP_RELAY_TOKEN")
                .unwrap_or_else(|_| "rdep-relay-token".to_string()),
            web_listen: std::env::var("RDEP_FWD_WEB_LISTEN").ok(),
        })
    }
}
