use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::CertificateDer;

/// 从 PEM 文件加载证书与私钥，构造 rustls 服务端配置。
pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>> {
    let cert_file = std::fs::File::open(cert_path).context("open cert file")?;
    let key_file = std::fs::File::open(key_path).context("open key file")?;

    let certs: Vec<CertificateDer<'static>> = {
        let mut r = std::io::BufReader::new(cert_file);
        rustls_pemfile::certs(&mut r)
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("parse certificates")?
    };

    let key = {
        let mut r = std::io::BufReader::new(key_file);
        rustls_pemfile::private_key(&mut r)
            .context("parse private key")?
            .context("private key not found in file")?
    };

    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build server tls config")?;

    Ok(Arc::new(cfg))
}
