//! 站点管理：连接配置的持久化（保存 / 载入 / 列表 / 删除）。
//!
//! 站点配置以 JSON 存放在用户配置目录（`$XDG_CONFIG_HOME/rdep/sites.json`，
//! Windows 为 `%APPDATA%\rdep\sites.json`，macOS 为 `~/Library/Application Support/rdep/`）。
//!
//! ## 密码存储
//! 密码**默认不落盘**：`Site::password` 仅在用户显式勾选「记住密码」时保存，
//! 且仅做**可逆混淆**（XOR + base64），**不是加密**——能读到该文件的人就能还原明文。
//! 这是有意的取舍：部署工具需要无人值守重连，但要真正安全应接入系统钥匙串
//! （macOS Keychain / Windows Credential Manager / libsecret），留作扩展点。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 站点使用的传输协议。
///
/// 原始需求要求 client 支持 `ftp | sftp | rdep` 三种：
/// - **rdep**：自有协议，功能最全（发布/备份/回滚/重启脚本、tail、grep、远程编辑、
///   断点续传、并发中转）。
/// - **sftp**：SSH 文件传输（russh 实现）。基础文件操作之外还支持目录同步、
///   断点续传、tail（follow）、grep 与远端编辑（经 SSH exec 通道）；但无
///   发布/回滚/重启脚本语义（那些依赖 rdep service）。
/// - **ftp**：通用协议，仅提供基础文件操作（浏览/上传/下载/增删改）。
///   通过 TCP 明文传输。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Protocol {
    /// rdep 自有协议（默认）。
    #[default]
    Rdep,
    /// FTP。
    Ftp,
    /// SFTP（SSH）。
    Sftp,
}

impl Protocol {
    /// 下拉框展示名。
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Rdep => "rdep（推荐，功能最全）",
            Protocol::Sftp => "SFTP（SSH）",
            Protocol::Ftp => "FTP（仅基础文件操作，明文）",
        }
    }

    /// 该协议是否支持「高级文件能力」：目录同步 / 断点续传 / tail / grep / 远端编辑。
    /// rdep 与 SFTP 均支持；GUI 据此放行工具窗口。
    pub fn supports_advanced(self) -> bool {
        matches!(self, Protocol::Rdep | Protocol::Sftp)
    }

    /// 该协议是否支持发布/回滚（依赖 rdep service 的备份与重启脚本语义）。
    pub fn supports_publish(self) -> bool {
        matches!(self, Protocol::Rdep)
    }
}

/// 一个站点（一条完整的连接配置）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Site {
    /// 站点名（列表展示、唯一标识）。
    pub name: String,
    /// 传输协议。缺省（老配置文件）视为 rdep。
    #[serde(default)]
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 已混淆的密码（未勾选「记住密码」时为空串）。
    pub password: String,
    /// CA 证书路径（空 = 使用系统信任 / 自签手动指定）。
    #[serde(default)]
    pub ca_cert: String,
    /// 是否经 forwarder 中转。
    #[serde(default)]
    pub use_forwarder: bool,
    /// forwarder 上的目标 service id。
    #[serde(default)]
    pub target_service_id: String,
    /// 中转密钥（已混淆）。
    #[serde(default)]
    pub relay_token: String,
    /// 最近一次连接成功后自动保存的远端目录（下次连接直接落到这里）。
    #[serde(default)]
    pub last_remote_dir: String,
    /// 该站点用 **API 令牌** 而口令认证；此时 `password` 字段存的是令牌明文（混淆后）。
    #[serde(default)]
    pub use_token: bool,
    // ---- FileZilla 风格站点字段（全部持久化到 sites.json） ----
    /// 登录类型：`Normal`（用户名+密码）/ `Key`（密钥，预留）/ `Ask`（每次询问）。
    /// 用字符串存储以兼容老配置（缺省视为 Normal）。
    #[serde(default)]
    pub login_type: String,
    /// 站点背景颜色（FileZilla 风格的标签颜色，CSS 十六进制如 `#1E90FF`）。
    #[serde(default)]
    pub background_color: String,
    /// 备注（FileZilla 的「注释」栏）。
    #[serde(default)]
    pub comment: String,
    /// 高级：默认本地目录（连接后本地面板落点）。
    #[serde(default)]
    pub default_local_dir: String,
    /// 高级：默认远端目录（连接后远端面板落点）。
    #[serde(default)]
    pub default_remote_dir: String,
    /// 传输设置：并发传输数（仅 UI 保存，未接入调度器）。
    #[serde(default)]
    pub concurrency: u8,
    /// 字符集：`Auto`（默认，跟随服务端）/ `UTF-8`（强制）。
    #[serde(default)]
    pub charset: String,
}

impl Site {
    /// 新建站点（给表单用）。
    pub fn blank() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8443,
            user: "admin".into(),
            login_type: "Normal".into(),
            concurrency: 2,
            charset: "Auto".into(),
            ..Default::default()
        }
    }

    /// 列表展示名（空名兜底为 host:port）。
    pub fn display(&self) -> String {
        if self.name.trim().is_empty() {
            format!("{}:{}", self.host, self.port)
        } else {
            self.name.clone()
        }
    }

    /// 取出明文密码（未保存则空串）。
    pub fn password_plain(&self) -> String {
        deobfuscate(&self.password)
    }

    /// 取出明文中转密钥。
    pub fn relay_token_plain(&self) -> String {
        deobfuscate(&self.relay_token)
    }
}

/// 站点文件（顶层 JSON 对象）。
#[derive(Debug, Default, Serialize, Deserialize)]
struct SitesFile {
    #[serde(default)]
    sites: Vec<Site>,
}

/// 站点仓库：负责读/写配置文件。
pub struct SiteStore {
    path: PathBuf,
}

/// 各平台通用的「配置根目录」（不含应用名子目录）。
///
/// 供站点文件（`rdep/sites.json`）与 UI 设置（`rdep/ui.json`）、
/// 已知主机指纹（`rdep/known_hosts.json`）等共同使用，保证同根可发现。
pub fn config_base_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

impl SiteStore {
    /// 按平台惯例定位配置文件路径。
    pub fn default_path() -> PathBuf {
        config_base_dir().join("rdep").join("sites.json")
    }

    /// 以默认路径创建。
    pub fn new_default() -> Self {
        Self {
            path: Self::default_path(),
        }
    }

    /// 以指定路径创建（测试用）。
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 读取全部站点；文件不存在返回空列表（首次使用正常）。
    pub fn load(&self) -> Result<Vec<Site>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read sites file {}", self.path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let f: SitesFile =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", self.path.display()))?;
        Ok(f.sites)
    }

    /// 全量覆盖写入（原子：先写 `.tmp` 再 rename）。
    pub fn save(&self, sites: &[Site]) -> Result<()> {
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p)
                .with_context(|| format!("create config dir {}", p.display()))?;
        }
        let f = SitesFile {
            sites: sites.to_vec(),
        };
        let body = serde_json::to_string_pretty(&f).context("serialize sites")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename into {}", self.path.display()))?;
        Ok(())
    }

    /// 按名字新增或覆盖（同名视为更新）。
    pub fn upsert(&self, site: Site) -> Result<()> {
        let mut all = self.load()?;
        if let Some(p) = all.iter_mut().find(|s| s.name == site.name) {
            *p = site;
        } else {
            all.push(site);
        }
        self.save(&all)
    }

    /// 按名字删除（不存在则忽略）。
    pub fn remove(&self, name: &str) -> Result<()> {
        let mut all = self.load()?;
        all.retain(|s| s.name != name);
        self.save(&all)
    }
}

// ---- 密码混淆（可逆，非加密）----
//
// 目的仅是「避免明文肉眼可见 / grep 得到」，**不提供**真实安全性。
// 真正的凭据保护应接入系统钥匙串。见模块文档。

const XOR_KEY: u8 = 0x5A;

/// 把明文密码/密钥转成可存储的混淆串（**非加密**，见模块文档）。
/// 供 GUI 层构造 `Site` 时使用。
pub fn obfuscate_for_storage(plain: &str) -> String {
    obfuscate(plain)
}

/// 从存储的混淆串还原明文。
pub fn plaintext_from_storage(encoded: &str) -> String {
    deobfuscate(encoded)
}

fn obfuscate(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let bytes: Vec<u8> = plain.bytes().map(|b| b ^ XOR_KEY).collect();
    base64_encode(&bytes)
}

fn deobfuscate(encoded: &str) -> String {
    if encoded.is_empty() {
        return String::new();
    }
    match base64_decode(encoded) {
        Some(bytes) => bytes
            .into_iter()
            .map(|b| (b ^ XOR_KEY) as char)
            .collect::<String>(),
        None => String::new(),
    }
}

/// 标准 base64 字母表编码（避免为一个小函数引入新依赖）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b0 = c[0] as u32;
        let b1 = *c.get(1).unwrap_or(&0) as u32;
        let b2 = *c.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 标准 base64 解码（忽略 `=` 填充；非法字符返回 None）。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> (SiteStore, PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "rdep-sites-{}-{}.json",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_file(&p);
        (SiteStore::with_path(p.clone()), p)
    }

    /// base64 编解码互逆（含 1/2/3 字节三种长度边界）。
    #[test]
    fn base64_roundtrip() {
        for case in [
            vec![],
            vec![0u8],
            vec![1, 2],
            vec![1, 2, 3],
            (0..=255u8).collect::<Vec<u8>>(),
        ] {
            let enc = base64_encode(&case);
            let dec = base64_decode(&enc).expect("decode");
            assert_eq!(dec, case, "roundtrip failed for len {}", case.len());
        }
    }

    /// 密码混淆可逆，且明文不出现在存储串里。
    #[test]
    fn obfuscation_roundtrip() {
        let p = "s3cret-p@ss";
        let ob = obfuscate(p);
        assert_ne!(ob, p, "obfuscated must differ from plaintext");
        assert!(!ob.contains(p), "plaintext must not appear verbatim");
        assert_eq!(deobfuscate(&ob), p);
        // 空串往返
        assert_eq!(deobfuscate(&obfuscate("")), "");
    }

    /// 站点文件：保存 → 载入 → 更新 → 删除 全链路。
    #[test]
    fn store_crud() {
        let (st, path) = tmp_store("crud");

        // 初始为空
        assert!(st.load().unwrap().is_empty());

        // 新增（密码混淆存储）
        st.upsert(Site {
            name: "prod".into(),
            protocol: Protocol::Rdep,
            host: "10.0.0.5".into(),
            port: 8443,
            user: "admin".into(),
            password: obfuscate("admin"),
            ca_cert: "/etc/rdep/ca.crt".into(),
            use_forwarder: true,
            target_service_id: "prod-1".into(),
            relay_token: obfuscate("tok"),
            last_remote_dir: "/opt/app".into(),
            use_token: false,
            ..Default::default()
        })
        .unwrap();

        let loaded = st.load().unwrap();
        assert_eq!(loaded.len(), 1);
        let s = &loaded[0];
        assert_eq!(s.display(), "prod");
        assert_eq!(s.password_plain(), "admin", "password should roundtrip");
        assert_eq!(s.relay_token_plain(), "tok");
        assert!(s.use_forwarder);
        assert_eq!(s.last_remote_dir, "/opt/app");

        // 确认磁盘上密码/密钥不是明文（按字段精确判断，不能用整串 contains——
        // 用户名恰好也叫 admin，会造成误判）
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("reparse persisted sites json");
        let pwd = parsed["sites"][0]["password"].as_str().expect("password field");
        let tok = parsed["sites"][0]["relay_token"].as_str().expect("relay_token field");
        assert_ne!(pwd, "admin", "password must not be plaintext on disk");
        assert_ne!(tok, "tok", "relay token must not be plaintext on disk");
        assert!(!pwd.contains("admin"), "password must not contain plaintext: {pwd}");
        assert!(pwd == obfuscate("admin"), "password must be the obfuscated form");

        // 同名更新（不新增条目）
        let mut updated = s.clone();
        updated.port = 9443;
        st.upsert(updated).unwrap();
        let loaded = st.load().unwrap();
        assert_eq!(loaded.len(), 1, "same name should update in place");
        assert_eq!(loaded[0].port, 9443);

        // 删除
        st.remove("prod").unwrap();
        assert!(st.load().unwrap().is_empty());

        let _ = std::fs::remove_file(&path);
    }

    /// 空名站点用 host:port 兜底展示。
    #[test]
    fn display_fallback() {
        let s = Site {
            name: "  ".into(),
            host: "h".into(),
            port: 1,
            ..Default::default()
        };
        assert_eq!(s.display(), "h:1");
    }
}
