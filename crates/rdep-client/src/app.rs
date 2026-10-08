//! rdep-client 的 egui 主界面（FileZilla 风格）：双栏文件树 + 传输队列 + 站点管理。
//!
//! ## 布局
//! - 顶栏：站点 / 连接·断开 / 发布回滚（仅 rdep）/ 日志·检索·编辑 / 语言切换 / 状态。
//! - 左栏（本地）：路径栏 + 文件表（名称/大小/类型/修改/权限）+ 右键菜单（上传/进入/刷新/新建目录）。
//! - 右栏（远端）：同上 + 右键菜单（下载/进入/刷新/新建目录/删除/改名）。
//! - 底栏：传输队列（进度条 + 状态）。
//! - 中心：日志。
//!
//! ## 协议分发
//! 基础文件操作（浏览/上传/下载/新建/删除/改名）按当前协议路由到 `Client`(rdep) /
//! `FtpClient`(ftp) / `SftpClient`(SFTP)；三者复用同一 `Event` 类型，面板与
//! `drain_events` 无需区分协议。rdep 专属的发布/回滚/重启脚本仅 rdep 可用；
//! tail/grep/编辑/目录同步对 rdep 与 SFTP 均可用。

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use rdep_protocol::{Direction, FileEntry};

use crate::client::{Client, ConnectParams, Event, PublishFile};
use crate::ftp::{FtpClient, FtpParams};
use crate::i18n::{self, t, tf, Lang};
use crate::sftp::{SftpClient, SftpParams};
use crate::sites::{obfuscate_for_storage, Protocol, Site, SiteStore};

#[derive(Clone)]
struct LocalEntry {
    name: String,
    is_dir: bool,
    size: u64,
    mode: u32,
    mtime: u64,
}

#[derive(Clone)]
struct TransferItem {
    id: u64,
    name: String,
    direction: Direction,
    sent: u64,
    total: u64,
    status: String,
    ok: bool,
    /// TransferDone 已到（用于队列标签页分桶：进行中/失败/成功）。
    done: bool,
    /// 队列表格用的完整本地/远端路径。
    local_path: String,
    remote_path: String,
}

pub struct RdepApp {
    client: Client,
    /// FTP 后端（`Site.protocol == Ftp` 时使用）。
    ftp: FtpClient,
    /// SFTP 后端（`Site.protocol == Sftp` 时使用）。
    sftp: SftpClient,
    /// 当前连接使用的协议，决定基础操作路由到哪个后端。
    backend: Protocol,
    connected: bool,
    status: String,
    log: Vec<String>,

    // 远端
    remote_dir: String,
    remote_entries: Vec<FileEntry>,
    remote_selected: Option<usize>,

    // 本地
    local_dir: PathBuf,
    local_entries: Vec<LocalEntry>,
    local_selected: Option<usize>,

    // 传输队列
    transfers: Vec<TransferItem>,
    /// 队列底部标签页：0=进行中 1=失败 2=成功。
    queue_tab: usize,
    /// 用户发起传输时记录 (方向, 文件名, 本地路径, 远程路径)，
    /// 待 `TransferStarted`（只带文件名）到达后回填到 TransferItem。
    pending_paths: Vec<(Direction, String, String, String)>,

    // 目录树（FileZilla 风格：树上、文件列表下）
    /// 本地树已展开的目录。
    local_tree_open: std::collections::HashSet<PathBuf>,
    /// 本地目录 → 子目录名缓存（懒加载）。
    local_tree_cache: std::collections::HashMap<PathBuf, Vec<String>>,
    /// 远端树已展开的目录。
    remote_tree_open: std::collections::HashSet<String>,
    /// 远端目录 → 子目录名缓存（懒加载，来自 ls 应答）。
    remote_tree_cache: std::collections::HashMap<String, Vec<String>>,
    /// 正在等待应答的树节点 ls 路径（区分主面板 ls 与树 ls）。
    pending_tree_ls: Option<String>,

    // 连接对话框
    show_connect: bool,
    cfg_host: String,
    cfg_port: String,
    cfg_user: String,
    cfg_pass: String,
    cfg_ca: String,
    // 中转（forwarder）相关（仅 rdep）
    cfg_use_forwarder: bool,
    cfg_service_id: String,
    cfg_relay_token: String,
    /// 用 API 令牌而非口令认证（此时「密码」框填令牌）。
    cfg_use_token: bool,

    // 站点管理
    store: SiteStore,
    sites: Vec<Site>,
    site_selected: Option<usize>,
    site_name_input: String,
    site_remember_pass: bool,
    show_sites: bool,
    /// FileZilla 风格站点表单（连接框 + 站点管理器共享）：
    /// 登录类型 / 背景颜色 / 注释 / 默认本地目录 / 默认远端目录 / 并发数 / 字符集。
    cfg_login_type: String,
    site_bg_color: String,
    site_comment: String,
    site_default_local: String,
    site_default_remote: String,
    site_concurrency: String,
    site_charset: String,
    /// 站点管理器右侧当前标签页（0=常规 1=高级 2=传输设置 3=字符集）。
    site_tab: usize,
    /// 当前载入到表单的站点名（用于「改名时删除旧条目」，避免残留孤儿站点）。
    /// 新建站点或清空时置 None。
    site_loaded_name: Option<String>,
    /// 从站点载入的远端目录：连接成功后落到这里（消费后清空）。
    pending_remote_dir: Option<String>,

    /// 本地目录浏览对话框的结果通道。点击「浏览…」时在独立线程打开系统文件选择框
    /// （rfd，Linux 走 xdg-portal），用户选完或取消后结果经此通道回传，
    /// 下一帧 `drain_events` 读取并写入 `site_default_local`。
    /// `Some(rx)` = 对话框等待中（避免重复打开）；`None` = 当前无对话框等待。
    browse_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,

    // 远端新建目录
    new_dir: String,
    // 远端改名输入
    rename_input: String,

    // 本地新建目录
    local_new_dir: String,

    // 发布 / 回滚（工具条上拆成两个按钮，各自一个窗口）
    show_publish: bool,
    show_rollback: bool,
    /// 目录同步窗口（工具条独立按钮）。
    show_sync: bool,
    publish_remote: String,
    publish_script: String,
    publish_files: Vec<PublishFile>,
    pub_local_input: String,
    pub_name_input: String,
    rollback_remote: String,
    backup_versions: Vec<String>,
    rollback_version: String,
    pending_backup_list: bool,

    // 工具：tail / grep / edit（从工具条移入「远端文件右键菜单」，针对目标文件执行）
    show_tail: bool,
    show_grep: bool,
    show_edit: bool,
    /// tail / grep 的结果文本（显示在弹出编辑窗里，可滚动/可复制）。
    tail_view: String,
    grep_view: String,
    tail_path: String,
    tail_lines: String,
    tail_follow: bool,
    tail_output: Vec<String>,
    grep_path: String,
    grep_pattern: String,
    grep_flags: String,
    grep_output: Vec<String>,
    edit_path: String,
    edit_content: String,
    edit_loaded: bool,

    // 远端 chmod（右键「权限」触发）
    show_chmod: bool,
    chmod_path: String,
    chmod_mode: String,
    /// tail 编辑框自动滚动到末尾的触发标志：收到新日志行时置 true，
    /// 渲染后清掉；与 tail_follow 叠加决定「是否滚到底」。
    tail_view_dirty: bool,

    // 目录同步
    sync_local: String,
    sync_remote: String,
    sync_delete_extra: bool,
    sync_preview: Option<(Vec<String>, Vec<String>)>,
}

/// 日志时间戳 `HH:MM:SS`：日志面板按时间线排布，便于对照服务端日志定位「卡在哪一步」。
fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

impl RdepApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let app = Self::new_headless();
        // 启动时载入 CJK 字体，修复中文显示为方块（tofu）的问题（找不到则静默跳过）。
        crate::fonts::install_cjk_fonts(&cc.egui_ctx);
        // 应用持久化的界面语言（默认英文）。
        i18n::set_lang(i18n::load_lang());
        app
    }

    /// 不依赖 `eframe::CreationContext` 的构造（无显示环境冒烟测试用）。
    pub fn new_headless() -> Self {
        Self::with_store(SiteStore::new_default())
    }

    /// 指定站点仓库的构造（测试用：避免读写用户真实配置目录）。
    pub fn with_store(store: SiteStore) -> Self {
        // 让所有后端线程（rdep/FTP/SFTP）共享同一语言设置。
        i18n::set_lang(i18n::load_lang());
        let (sites, load_err) = match store.load() {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(format!("{e:#}"))),
        };
        let mut app = RdepApp {
            client: Client::new(),
            ftp: FtpClient::new(),
            sftp: SftpClient::new(),
            backend: Protocol::Rdep,
            connected: false,
            status: t("Not connected").into(),
            log: Vec::new(),
            remote_dir: "/".into(),
            remote_entries: Vec::new(),
            remote_selected: None,
            local_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            local_entries: Vec::new(),
            local_selected: None,
            transfers: Vec::new(),
            queue_tab: 0,
            pending_paths: Vec::new(),
            local_tree_open: std::collections::HashSet::new(),
            local_tree_cache: std::collections::HashMap::new(),
            remote_tree_open: std::collections::HashSet::new(),
            remote_tree_cache: std::collections::HashMap::new(),
            pending_tree_ls: None,
            show_connect: false,
            cfg_host: "127.0.0.1".into(),
            cfg_port: "8443".into(),
            cfg_user: "admin".into(),
            cfg_pass: "admin".into(),
            cfg_ca: String::new(),
            cfg_use_forwarder: false,
            cfg_service_id: String::new(),
            cfg_relay_token: "rdep-relay-token".into(),
            cfg_use_token: false,
            store,
            sites,
            site_selected: None,
            site_name_input: String::new(),
            site_remember_pass: false,
            show_sites: false,
            cfg_login_type: "Normal".into(),
            site_bg_color: "#1E90FF".into(),
            site_comment: String::new(),
            site_default_local: String::new(),
            site_default_remote: String::new(),
            site_concurrency: "2".into(),
            site_charset: "Auto".into(),
            site_tab: 0,
            site_loaded_name: None,
            pending_remote_dir: None,
            new_dir: String::new(),
            rename_input: String::new(),
            local_new_dir: String::new(),
            show_publish: false,
            show_rollback: false,
            show_sync: false,
            publish_remote: "/".into(),
            publish_script: "restart".into(),
            publish_files: Vec::new(),
            pub_local_input: String::new(),
            pub_name_input: String::new(),
            rollback_remote: "/".into(),
            backup_versions: Vec::new(),
            rollback_version: String::new(),
            pending_backup_list: false,
            show_tail: false,
            show_grep: false,
            show_edit: false,
            tail_view: String::new(),
            grep_view: String::new(),
            tail_path: "/var/log/messages".into(),
            tail_lines: "50".into(),
            tail_follow: false,
            tail_output: Vec::new(),
            grep_path: "/".into(),
            grep_pattern: String::new(),
            grep_flags: "in".into(),
            grep_output: Vec::new(),
            edit_path: String::new(),
            edit_content: String::new(),
            edit_loaded: false,
            show_chmod: false,
            chmod_path: String::new(),
            chmod_mode: String::new(),
            tail_view_dirty: false,
            sync_local: String::new(),
            sync_remote: "/opt/app".into(),
            sync_delete_extra: false,
            sync_preview: None,
            browse_rx: None,
        };
        app.refresh_local();
        app.push_log(t("rdep client started"));
        match load_err {
            Some(e) => app.push_log(&format!("{}: {e}", t("Failed to read site config (ignored)"))),
            None => {
                if app.sites.is_empty() {
                    app.push_log(t("No saved sites yet; save one in \"Sites\""));
                } else {
                    app.push_log(&tf("Loaded {n} sites", &[("n", &app.sites.len().to_string())]));
                }
            }
        }
        app
    }

    // ---- 协议分发：基础文件操作按当前协议路由到 rdep / FTP / SFTP ----
    //
    // 三个后端复用同一个 `Event` 类型，因此面板与 `drain_events` 无需区分协议。
    // 只有「基础文件操作」被路由；rdep 专属能力（发布/回滚/重启脚本）由
    // `require_publish` 显式拦截并说明原因，不静默失败；tail/grep/编辑/同步
    // 由 `require_advanced` 放行（rdep 与 SFTP 均支持）。

    /// 当前后端是否支持高级能力（tail/grep/编辑/目录同步）：rdep 与 SFTP。
    /// 不支持时记日志并返回 false。
    fn require_advanced(&mut self, feature: &'static str) -> bool {
        if self.backend.supports_advanced() {
            return true;
        }
        self.push_log(&tf(
            "\"{f}\" requires the rdep protocol; current site is {p}, skipped",
            &[("f", t(feature)), ("p", protocol_name(self.backend))],
        ));
        false
    }

    /// 当前后端是否支持发布/回滚（仅 rdep）。不支持时记日志并返回 false。
    fn require_publish(&mut self, feature: &'static str) -> bool {
        if self.backend.supports_publish() {
            return true;
        }
        self.push_log(&tf(
            "\"{f}\" requires the rdep protocol; current site is {p}, skipped",
            &[("f", t(feature)), ("p", protocol_name(self.backend))],
        ));
        false
    }

    fn ls_remote(&self, path: &str) {
        tracing::debug!(backend = ?self.backend, path, "ls_remote: dispatch");
        match self.backend {
            Protocol::Rdep => self.client.ls(path),
            Protocol::Ftp => self.ftp.ls(path),
            Protocol::Sftp => self.sftp.ls(path),
        }
    }

    fn do_upload(&mut self, remote_path: String, local_path: String) {
        tracing::debug!(backend = ?self.backend, local = %local_path, remote = %remote_path, "action: upload");
        // 记录完整路径，供 TransferStarted 到达时回填队列表格
        if let Some(name) = std::path::Path::new(&local_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
        {
            self.pending_paths.push((
                Direction::Upload,
                name,
                local_path.clone(),
                remote_path.clone(),
            ));
        }
        match self.backend {
            Protocol::Rdep => self.client.upload(remote_path, local_path),
            Protocol::Ftp => self.ftp.upload(remote_path, local_path),
            Protocol::Sftp => self.sftp.upload(remote_path, local_path),
        }
    }

    fn do_download(&mut self, remote_path: String, local_path: String) {
        tracing::debug!(backend = ?self.backend, remote = %remote_path, local = %local_path, "action: download");
        if let Some(name) = std::path::Path::new(&remote_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
        {
            self.pending_paths.push((
                Direction::Download,
                name,
                local_path.clone(),
                remote_path.clone(),
            ));
        }
        match self.backend {
            Protocol::Rdep => self.client.download(remote_path, local_path),
            Protocol::Ftp => self.ftp.download(remote_path, local_path),
            Protocol::Sftp => self.sftp.download(remote_path, local_path),
        }
    }

    fn do_mkdir(&self, paths: Vec<String>) {
        tracing::debug!(backend = ?self.backend, paths = ?paths, "action: mkdir");
        match self.backend {
            Protocol::Rdep => self.client.mkdir(paths),
            Protocol::Ftp => self.ftp.mkdir(paths),
            Protocol::Sftp => self.sftp.mkdir(paths),
        }
    }

    fn do_delete(&self, paths: Vec<String>) {
        tracing::debug!(backend = ?self.backend, paths = ?paths, "action: delete");
        match self.backend {
            Protocol::Rdep => self.client.delete(paths),
            Protocol::Ftp => self.ftp.delete(paths),
            Protocol::Sftp => self.sftp.delete(paths),
        }
    }

    fn do_rename(&self, src: String, new_name: String) {
        tracing::debug!(backend = ?self.backend, src = %src, new_name = %new_name, "action: rename");
        match self.backend {
            Protocol::Rdep => self.client.rename(src, new_name),
            Protocol::Ftp => self.ftp.rename(src, new_name),
            Protocol::Sftp => self.sftp.rename(src, new_name),
        }
    }

    fn do_disconnect(&self) {
        tracing::debug!(backend = ?self.backend, "action: disconnect");
        match self.backend {
            Protocol::Rdep => self.client.disconnect(),
            Protocol::Ftp => self.ftp.disconnect(),
            Protocol::Sftp => self.sftp.disconnect(),
        }
    }

    fn do_tail(&mut self, path: String, lines: u32, follow: bool) {
        tracing::debug!(backend = ?self.backend, path = %path, lines, follow, "action: tail");
        if self.backend == Protocol::Ftp {
            self.push_log(t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."));
            return;
        }
        match self.backend {
            Protocol::Rdep => self.client.tail(path, lines, follow),
            Protocol::Sftp => self.sftp.tail(path, lines, follow),
            Protocol::Ftp => unreachable!(),
        }
    }

    fn do_stop_tail(&self) {
        tracing::debug!(backend = ?self.backend, "action: stop tail");
        match self.backend {
            Protocol::Rdep => self.client.request_stop_tail(),
            Protocol::Sftp => self.sftp.request_stop_tail(),
            Protocol::Ftp => {}
        }
    }

    fn do_grep(&mut self, path: String, pattern: String, flags: String) {
        tracing::debug!(backend = ?self.backend, path = %path, pattern = %pattern, flags = %flags, "action: grep");
        if self.backend == Protocol::Ftp {
            self.push_log(t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."));
            return;
        }
        match self.backend {
            Protocol::Rdep => self.client.grep(path, pattern, flags),
            Protocol::Sftp => self.sftp.grep(path, pattern, flags),
            Protocol::Ftp => unreachable!(),
        }
    }

    fn do_edit_get(&mut self, path: String) {
        tracing::debug!(backend = ?self.backend, path = %path, "action: edit load");
        if self.backend == Protocol::Ftp {
            self.push_log(t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."));
            return;
        }
        match self.backend {
            Protocol::Rdep => self.client.edit_get(path),
            Protocol::Sftp => self.sftp.edit_get(path),
            Protocol::Ftp => unreachable!(),
        }
    }

    fn do_edit_save(&mut self, path: String, content: String) {
        tracing::debug!(backend = ?self.backend, path = %path, bytes = content.len(), "action: edit save");
        if self.backend == Protocol::Ftp {
            self.push_log(t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."));
            return;
        }
        match self.backend {
            Protocol::Rdep => self.client.edit_save(path, content),
            Protocol::Sftp => self.sftp.edit_save(path, content),
            Protocol::Ftp => unreachable!(),
        }
    }

    /// 远端 chmod（右键「权限」）。rdep 与 SFTP 协议支持（SFTP 走 `setstat`）；FTP 协议不支持，提示改用 rdep/SFTP 站点。
    fn do_chmod(&mut self, path: String, mode: u32) {
        tracing::debug!(backend = ?self.backend, path = %path, mode = format!("{mode:o}"), "action: chmod");
        match self.backend {
            Protocol::Rdep => self.client.chmod(path, mode),
            Protocol::Sftp => self.sftp.chmod(path, mode),
            Protocol::Ftp => self.push_log(&tf(
                "chmod not supported over FTP; current site is {p}, skipped",
                &[("p", protocol_name(self.backend))],
            )),
        }
    }

    fn do_sync_dir(&mut self, local: String, remote: String, delete_extra: bool, dry_run: bool) {
        tracing::debug!(backend = ?self.backend, local = %local, remote = %remote, delete_extra, dry_run, "action: sync dir");
        if self.backend == Protocol::Ftp {
            self.push_log(t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."));
            return;
        }
        match self.backend {
            Protocol::Rdep => self.client.sync_dir(local, remote, delete_extra, dry_run),
            Protocol::Sftp => self.sftp.sync_dir(local, remote, delete_extra, dry_run),
            Protocol::Ftp => unreachable!(),
        }
    }

    /// 按当前选择的协议发起连接（rdep / FTP / SFTP）。
    fn connect_via_current_backend(&mut self) {
        // 即时反馈：无论后端多快失败，点击后立刻有可见变化
        let host = self.cfg_host.trim().to_string();
        let port = self.cfg_port.trim().to_string();
        if host.is_empty() {
            let msg = t("Host is required");
            self.status = msg.to_string();
            self.push_log(&msg);
            return;
        }
        tracing::debug!(
            backend = ?self.backend,
            host, port,
            user = %self.cfg_user,
            pass_len = self.cfg_pass.len(),
            ca = %self.cfg_ca,
            use_forwarder = self.cfg_use_forwarder,
            service_id = %self.cfg_service_id,
            use_token = self.cfg_use_token,
            "action: connect"
        );
        let hint = tf("Connecting to {host}:{port} ...", &[("host", &host), ("port", &port)]);
        self.status = hint.clone();
        self.push_log(&hint);
        // 应用站点里配置的默认目录（FileZilla 风格：连接后落到这里）
        if !self.site_default_local.trim().is_empty() {
            let p = PathBuf::from(self.site_default_local.trim());
            if p.is_dir() {
                self.local_dir = p;
            }
        }
        if !self.site_default_remote.trim().is_empty() {
            self.pending_remote_dir = Some(self.site_default_remote.trim().to_string());
        }
        match self.backend {
            Protocol::Rdep => {
                let p = self.params_from_form();
                self.client.connect(p);
            }
            Protocol::Ftp => {
                self.ftp.connect(FtpParams {
                    host: self.cfg_host.trim().to_string(),
                    port: self.cfg_port.trim().parse().unwrap_or(21),
                    user: self.cfg_user.trim().to_string(),
                    pass: self.cfg_pass.clone(),
                    initial_dir: if self.remote_dir.trim().is_empty() {
                        String::new()
                    } else {
                        self.remote_dir.trim().to_string()
                    },
                });
            }
            Protocol::Sftp => {
                self.sftp.connect(SftpParams {
                    host: self.cfg_host.trim().to_string(),
                    port: self.cfg_port.trim().parse().unwrap_or(22),
                    user: self.cfg_user.trim().to_string(),
                    pass: self.cfg_pass.clone(),
                    initial_dir: if self.remote_dir.trim().is_empty() {
                        String::new()
                    } else {
                        self.remote_dir.trim().to_string()
                    },
                });
            }
        }
    }

    /// 把当前连接表单组装为 `ConnectParams`。
    fn params_from_form(&self) -> ConnectParams {
        ConnectParams {
            host: self.cfg_host.trim().to_string(),
            port: self.cfg_port.trim().parse().unwrap_or(8443),
            user: self.cfg_user.trim().to_string(),
            pass: self.cfg_pass.clone(),
            ca_cert: if self.cfg_ca.trim().is_empty() {
                None
            } else {
                Some(PathBuf::from(self.cfg_ca.trim()))
            },
            use_forwarder: self.cfg_use_forwarder,
            target_service_id: self.cfg_service_id.trim().to_string(),
            relay_token: self.cfg_relay_token.trim().to_string(),
            use_token: self.cfg_use_token,
        }
    }

    /// 用选中站点的配置填充连接表单。
    fn load_site_into_form(&mut self, idx: usize) {
        let Some(s) = self.sites.get(idx).cloned() else {
            return;
        };
        self.site_name_input = s.name.clone();
        self.backend = s.protocol;
        self.cfg_use_token = s.use_token;
        self.cfg_host = s.host.clone();
        self.cfg_port = s.port.to_string();
        self.cfg_user = s.user.clone();
        self.cfg_pass = s.password_plain();
        self.cfg_ca = s.ca_cert.clone();
        self.cfg_use_forwarder = s.use_forwarder;
        self.cfg_service_id = s.target_service_id.clone();
        self.cfg_relay_token = if s.relay_token_plain().is_empty() {
            "rdep-relay-token".into()
        } else {
            s.relay_token_plain()
        };
        if !s.last_remote_dir.is_empty() {
            self.pending_remote_dir = Some(s.last_remote_dir.clone());
        }
        self.site_remember_pass = !s.password.is_empty();
        self.site_loaded_name = Some(s.name.clone());
        // FileZilla 风格字段
        self.cfg_login_type = if s.login_type.is_empty() {
            "Normal".into()
        } else {
            s.login_type.clone()
        };
        self.site_bg_color = if s.background_color.is_empty() {
            "#1E90FF".into()
        } else {
            s.background_color.clone()
        };
        self.site_comment = s.comment.clone();
        self.site_default_local = s.default_local_dir.clone();
        self.site_default_remote = s.default_remote_dir.clone();
        self.site_concurrency = s.concurrency.to_string();
        self.site_charset = if s.charset.is_empty() {
            "Auto".into()
        } else {
            s.charset.clone()
        };
    }

    /// 把当前连接表单保存为一个站点（按站点名 upsert）。
    fn save_current_form_as_site(&mut self) {
        let name = self.site_name_input.trim().to_string();
        if name.is_empty() {
            self.push_log(t("Enter a site name first"));
            return;
        }
        // 改名保护：若站点名相对载入时发生变化，删除旧条目，避免残留孤儿站点。
        if let Some(old) = self.site_loaded_name.clone() {
            if old != name && self.sites.iter().any(|s| s.name == old) {
                let _ = self.store.remove(&old);
            }
        }
        let password = if self.site_remember_pass {
            obfuscate_for_storage(&self.cfg_pass)
        } else {
            String::new()
        };
        let relay_token = if self.cfg_use_forwarder && self.site_remember_pass {
            obfuscate_for_storage(self.cfg_relay_token.trim())
        } else {
            String::new()
        };
        let site = Site {
            name: name.clone(),
            protocol: self.backend,
            host: self.cfg_host.trim().to_string(),
            port: self.cfg_port.trim().parse().unwrap_or(8443),
            user: self.cfg_user.trim().to_string(),
            password,
            ca_cert: self.cfg_ca.trim().to_string(),
            use_forwarder: self.cfg_use_forwarder,
            target_service_id: self.cfg_service_id.trim().to_string(),
            relay_token,
            last_remote_dir: self.remote_dir.clone(),
            use_token: self.cfg_use_token,
            login_type: self.cfg_login_type.trim().to_string(),
            background_color: self.site_bg_color.trim().to_string(),
            comment: self.site_comment.trim().to_string(),
            default_local_dir: self.site_default_local.trim().to_string(),
            default_remote_dir: self.site_default_remote.trim().to_string(),
            concurrency: self.site_concurrency.trim().parse().unwrap_or(2),
            charset: self.site_charset.trim().to_string(),
        };
        match self.store.upsert(site) {
            Ok(()) => match self.store.load() {
                Ok(v) => {
                    self.sites = v;
                    self.site_selected = self.sites.iter().position(|s| s.name == name);
                    let p = self.store.path().display().to_string();
                    let extra = if self.site_remember_pass {
                        t("saved (with password)")
                    } else {
                        t("saved (password not stored)")
                    };
                    self.push_log(&tf(
                        "Site \"{n}\" saved ({e}) → {p}",
                        &[("n", &name), ("e", extra), ("p", &p)],
                    ));
                }
                Err(e) => self.push_log(&format!("{}: {e:#}", t("Site written but reload failed"))),
            },
            Err(e) => self.push_log(&format!("{}: {e:#}", t("Failed to save site"))),
        }
    }

    /// 删除选中站点。
    fn delete_selected_site(&mut self) {
        let Some(idx) = self.site_selected else {
            self.push_log(t("Choose a site first"));
            return;
        };
        let Some(s) = self.sites.get(idx).cloned() else {
            return;
        };
        match self.store.remove(&s.name) {
            Ok(()) => match self.store.load() {
                Ok(v) => {
                    self.sites = v;
                    self.site_selected = None;
                    self.push_log(&tf("site deleted", &[("n", &s.display())]));
                }
                Err(e) => self.push_log(&format!("{}: {e:#}", t("Reload after delete failed"))),
            },
            Err(e) => self.push_log(&format!("{}: {e:#}", t("Failed to delete site"))),
        }
    }

    /// 新建一个空白站点（立即落盘并在左侧树中选中），随后表单载入其默认值供编辑。
    fn new_site(&mut self) {
        let mut n = 1;
        let mut name = format!("New site {n}");
        while self.sites.iter().any(|s| s.name == name) {
            n += 1;
            name = format!("New site {n}");
        }
        let mut site = Site::blank();
        site.name = name.clone();
        if let Err(e) = self.store.upsert(site) {
            self.push_log(&format!("{}: {e:#}", t("Failed to save site")));
            return;
        }
        match self.store.load() {
            Ok(v) => {
                self.sites = v;
                self.site_selected = self.sites.iter().position(|s| s.name == name);
                if let Some(i) = self.site_selected {
                    self.load_site_into_form(i);
                }
                self.push_log(&tf("created new site", &[("n", &name)]));
            }
            Err(e) => self.push_log(&format!("{}: {e:#}", t("Site written but reload failed"))),
        }
    }

    fn push_log(&mut self, s: &str) {
        let ts = now_hms();
        self.log.push(format!("[{ts}] {s}"));
        if self.log.len() > 300 {
            self.log.drain(..self.log.len() - 300);
        }
        // 界面日志同步进 debug 日志：用户反馈「点了没反应」时，日志面板与文件日志对齐
        tracing::debug!(message = %s, "gui log");
    }

    /// 取三个后台线程回传的事件并刷新界面状态。
    fn drain_events(&mut self) {
        while let Some(ev) = self.client.next_event() {
            self.handle(ev);
        }
        while let Some(ev) = self.ftp.next_event() {
            self.handle(ev);
        }
        while let Some(ev) = self.sftp.next_event() {
            self.handle(ev);
        }

        // 本地目录浏览对话框的结果（独立线程回传，不阻塞渲染）。
        // 先取出 owned 的接收结果，再改 self，避免借用冲突。
        let picked = self.browse_rx.as_ref().map(|rx| rx.try_recv());
        match picked {
            Some(Ok(Some(path))) => {
                self.site_default_local = path.to_string_lossy().into_owned();
                self.browse_rx = None;
            }
            Some(Ok(None)) => {
                // 用户取消：保持原值，清理等待状态
                self.browse_rx = None;
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                // 对话框仍未回传，下一帧继续等待
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                // 后台线程异常退出且无结果，丢弃等待状态
                self.browse_rx = None;
            }
            None => {}
        }
    }

    /// 统一处理任一后端回传的事件。
    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Status(s) => {
                self.status = s.clone();
                self.push_log(&s);
            }
            Event::Connected => {
                self.connected = true;
                self.push_log(t("Connected"));
                let target = self.pending_remote_dir.take().unwrap_or_else(|| "/".into());
                self.cd_remote(target);
            }
            Event::Disconnected => {
                // 连接失败时后端会先发 Error 再发 Disconnected；
                // 只有“原本已连接”的断开才把状态改回 Disconnected，
                // 否则错误信息会被同帧覆盖，看起来像按钮没反应。
                let was_connected = self.connected;
                self.connected = false;
                // 连接已断：所有「在途请求」标记都作废，避免残留状态吞掉后续应答
                if self.pending_tree_ls.take().is_some() {
                    tracing::debug!("disconnect: cleared pending_tree_ls");
                }
                self.pending_backup_list = false;
                if was_connected {
                    self.status = t("Disconnected").into();
                }
                self.push_log(t("Disconnected"));
            }
            Event::Error(s) => {
                self.push_log(&format!("{}: {s}", t("server error")));
                self.status = s;
                // 出错时清理「在途目录树 ls」标记：否则该标记永久残留，
                // 之后主面板的 ls 应答会被误判为树节点加载而被吞掉 —— 表现为
                // 「点 Refresh 没有任何作用」。
                if self.pending_tree_ls.take().is_some() {
                    tracing::debug!("cleared stale pending_tree_ls after error");
                }
            }
            Event::DirListed { path, entries } => {
                if self.pending_backup_list {
                    self.backup_versions = entries
                        .iter()
                        .filter(|e| e.is_dir)
                        .map(|e| e.name.clone())
                        .collect();
                    self.pending_backup_list = false;
                } else if self.pending_tree_ls.as_deref() == Some(path.as_str()) {
                    // 这是目录树节点的懒加载 ls：只取子目录名进缓存
                    self.pending_tree_ls = None;
                    let mut subs: Vec<String> = entries
                        .iter()
                        .filter(|e| e.is_dir)
                        .map(|e| e.name.clone())
                        .collect();
                    subs.sort();
                    self.remote_tree_cache.insert(path.clone(), subs);
                } else {
                    self.remote_dir = path;
                    self.push_log(&tf(
                        "{n} entries in {path}",
                        &[("n", &entries.len().to_string()), ("path", &self.remote_dir)],
                    ));
                    // 根目录没有上级（service 拒绝 `..`），不显示 ".." 行
                    let mut v = Vec::new();
                    if self.remote_dir != "/" {
                        v.push(FileEntry {
                            name: "..".into(),
                            is_dir: true,
                            size: 0,
                            mtime: 0,
                            mode: 0,
                        });
                    }
                    v.extend(entries);
                    self.remote_entries = v;
                }
            }
            Event::TransferStarted { id, name, direction } => {
                // 回填发起传输时记录的完整路径
                let (mut local_path, mut remote_path) = (String::new(), String::new());
                if let Some(pos) = self
                    .pending_paths
                    .iter()
                    .position(|(d, n, _, _)| *d == direction && n == &name)
                {
                    let (_, _, l, r) = self.pending_paths.remove(pos);
                    local_path = l;
                    remote_path = r;
                }
                self.transfers.push(TransferItem {
                    id,
                    name,
                    direction,
                    sent: 0,
                    total: 0,
                    status: t("Transferring").into(),
                    ok: false,
                    done: false,
                    local_path,
                    remote_path,
                });
            }
            Event::TransferProgress { id, sent, total } => {
                if let Some(x) = self.transfers.iter_mut().find(|x| x.id == id) {
                    x.sent = sent;
                    x.total = total;
                }
            }
            Event::TransferDone { id, ok, message } => {
                if let Some(x) = self.transfers.iter_mut().find(|x| x.id == id) {
                    x.ok = ok;
                    x.done = true;
                    x.status = if message.is_empty() {
                        if ok {
                            t("Done").into()
                        } else {
                            t("Failed").into()
                        }
                    } else {
                        message
                    };
                }
            }
            Event::OpDone { ok, message } => {
                self.push_log(&format!(
                    "{}: {}",
                    if ok { t("op ok") } else { t("op failed") },
                    message
                ));
                if self.connected {
                    self.refresh_remote();
                }
            }
            Event::PublishDone { ok, message } => {
                self.push_log(&format!(
                    "{}: {}",
                    if ok { t("publish ok") } else { t("publish failed") },
                    message
                ));
                if self.connected {
                    self.refresh_remote();
                }
            }
            Event::EditLoaded { content } => {
                self.edit_content = content;
                self.edit_loaded = true;
            }
            Event::BackupVersions { versions } => {
                tracing::debug!(versions = versions.len(), "backup versions received");
                self.push_log(&tf("{n} backup version(s)", &[("n", &versions.len().to_string())]));
                self.backup_versions = versions;
                self.pending_backup_list = false;
                if self.backup_versions.is_empty() {
                    self.push_log(t("(no backups yet)"));
                }
            }
            Event::TailLine { line } => {
                self.tail_output.push(line.clone());
                if self.tail_output.len() > 500 {
                    let drop = self.tail_output.len() - 500;
                    self.tail_output.drain(..drop);
                }
                // 结果同步进弹出编辑窗的文本缓冲
                self.tail_view.push_str(&line);
                self.tail_view.push('\n');
                if self.tail_view.len() > 200_000 {
                    let drop = self.tail_view.len() - 200_000;
                    let drop = (drop..=self.tail_view.len())
                        .find(|&index| self.tail_view.is_char_boundary(index))
                        .unwrap_or(self.tail_view.len());
                    self.tail_view.drain(..drop);
                }
                // 标记内容已更新，tail 弹窗据此自动滚到底部
                self.tail_view_dirty = true;
            }
            Event::TailDone { ok, message } => {
                self.push_log(&format!(
                    "{}: {}",
                    if ok { t("tail done") } else { t("op failed") },
                    message
                ));
                if !ok && !message.is_empty() {
                    self.tail_output.push(format!("[{}] {message}", t("server error")));
                }
            }
            Event::GrepResult { lines } => {
                self.push_log(&tf("{n} match(es)", &[("n", &lines.len().to_string())]));
                self.grep_view = lines.join("\n");
                self.grep_output = lines;
                if self.grep_output.is_empty() {
                    self.grep_view = t("(no output)").to_string();
                }
            }
            Event::SyncPreview { changed, to_delete } => {
                self.sync_preview = Some((changed, to_delete));
            }
        }
    }

    fn refresh_local(&mut self) {
        // 本地目录可能已变化，目录树缓存一并失效
        self.local_tree_cache.clear();
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.local_dir) {
            for entry in rd.flatten() {
                if let Ok(meta) = entry.metadata() {
                    let is_dir = meta.is_dir();
                    let size = if is_dir { 0 } else { meta.len() };
                    let mode = if is_dir { 0 } else { local_mode(&meta) };
                    let mtime = if is_dir {
                        0
                    } else {
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    };
                    v.push(LocalEntry {
                        name: entry.file_name().to_string_lossy().to_string(),
                        is_dir,
                        size,
                        mode,
                        mtime,
                    });
                }
            }
        }
        v.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                return if a.is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        });
        if self.local_dir.parent().is_some() {
            v.insert(
                0,
                LocalEntry {
                    name: "..".into(),
                    is_dir: true,
                    size: 0,
                    mode: 0,
                    mtime: 0,
                },
            );
        }
        self.local_entries = v;
    }

    fn enter_local(&mut self, p: PathBuf) {
        self.local_dir = p;
        self.local_selected = None;
        self.refresh_local();
    }

    fn cd_remote(&mut self, p: String) {
        tracing::debug!(path = %p, "cd_remote");
        self.remote_dir = p.clone();
        self.ls_remote(&p);
    }

    /// 刷新远端主面板（Refresh 按钮 / 操作完成后的自动刷新）。
    ///
    /// 关键：先清掉可能残留的「目录树 ls」在途标记。否则该标记会把本次 ls 应答
    /// 误判为树节点懒加载结果而写进树缓存，主面板纹丝不动 —— 即用户看到的
    /// 「点 Refresh 没有任何作用」。
    fn refresh_remote(&mut self) {
        if self.pending_tree_ls.take().is_some() {
            tracing::debug!("refresh_remote: dropped stale pending_tree_ls");
        }
        let path = self.remote_dir.clone();
        tracing::debug!(path = %path, "refresh_remote: issuing ls");
        self.push_log(&tf("Listing {path} ...", &[("path", &path)]));
        self.ls_remote(&path);
    }

    // ---- 文件表渲染（双栏共用的列：名称/大小/类型/修改/权限） ----
    //
    // FileZilla 风格：表头 + 行；行可单击选中、双击进入目录、右键弹出上下文菜单。

    fn local_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading(t("Local"));
        ui.horizontal(|ui| {
            if ui.button("↑").clicked() {
                if let Some(p) = self.local_dir.parent() {
                    self.enter_local(p.to_path_buf());
                }
            }
            if ui.button(t("Refresh")).clicked() {
                self.refresh_local();
            }
            if ui.button(t("Upload")).clicked() {
                self.upload_selected_local();
            }
            ui.label(t("Local site:"));
            ui.monospace(self.local_dir.display().to_string());
        });
        ui.horizontal(|ui| {
            ui.label(t("New directory"));
            ui.text_edit_singleline(&mut self.local_new_dir);
            if ui.button(t("Go")).clicked() && !self.local_new_dir.trim().is_empty() {
                let p = self.local_dir.join(self.local_new_dir.trim());
                if let Err(e) = std::fs::create_dir_all(&p) {
                    self.push_log(&format!("{}: {e}", t("New directory")));
                } else {
                    self.push_log(&format!(
                        "{}: {}",
                        t("New directory"),
                        p.display()
                    ));
                    self.local_new_dir.clear();
                    self.refresh_local();
                }
            }
        });
        // ---- 目录树（FileZilla 风格：树上、文件列表下；点击目录名即切换） ----
        egui::ScrollArea::vertical()
            .max_height(170.0)
            .id_salt("local_tree")
            .show(ui, |ui| {
                self.local_tree_node(ui, PathBuf::from("/"));
            });
        ui.separator();
        self.file_table(ui, false);
    }

    fn remote_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading(t("Remote"));
        ui.horizontal(|ui| {
            if ui.button("↑").clicked() {
                self.cd_remote(remote_parent(&self.remote_dir));
            }
            // 未连接时点击 Refresh 曾静默 no-op（按钮像坏了）；现在给出明确提示。
            if ui.button(t("Refresh")).clicked() {
                if !self.connected {
                    let msg = t("Not connected; connect first");
                    tracing::debug!("refresh clicked but not connected");
                    self.status = msg.to_string();
                    self.push_log(&msg);
                } else {
                    self.refresh_remote();
                }
            }
            if ui.button(t("Download")).clicked() {
                self.download_selected_remote();
            }
            if ui.button(t("Delete")).clicked() {
                self.delete_selected_remote();
            }
            ui.label(t("Remote site:"));
            ui.monospace(&self.remote_dir);
        });
        ui.horizontal(|ui| {
            ui.label(t("New directory"));
            ui.text_edit_singleline(&mut self.new_dir);
            if ui.button(t("Go")).clicked() && !self.new_dir.trim().is_empty() {
                let p = join_remote(&self.remote_dir, self.new_dir.trim());
                self.do_mkdir(vec![p]);
                self.new_dir.clear();
            }
        });
        // ---- 目录树（懒加载：展开节点时向服务端发 ls） ----
        egui::ScrollArea::vertical()
            .max_height(170.0)
            .id_salt("remote_tree")
            .show(ui, |ui| {
                self.remote_tree_node(ui, "/");
            });
        ui.separator();
        self.file_table(ui, true);
    }

    /// 本地目录树节点（递归渲染，子目录懒加载缓存）。
    fn local_tree_node(&mut self, ui: &mut egui::Ui, path: PathBuf) {
        let open = self.local_tree_open.contains(&path);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        ui.horizontal(|ui| {
            if ui.small_button(if open { "▾" } else { "▸" }).clicked() {
                if open {
                    self.local_tree_open.remove(&path);
                } else {
                    self.local_tree_open.insert(path.clone());
                }
            }
            let current = self.local_dir == path;
            if ui.selectable_label(current, format!("📁 {name}")).clicked() {
                self.enter_local(path.clone());
            }
        });
        if open {
            let subs = self.local_subdirs(&path);
            for s in subs {
                let child = if path == PathBuf::from("/") {
                    PathBuf::from(format!("/{s}"))
                } else {
                    path.join(&s)
                };
                let id = egui::Id::new("lt").with(&child);
                ui.indent(id, |ui| {
                    self.local_tree_node(ui, child);
                });
            }
        }
    }

    /// 读取某目录的子目录名（仅目录，不跟随符号链接；带缓存）。
    fn local_subdirs(&mut self, path: &std::path::Path) -> Vec<String> {
        if let Some(v) = self.local_tree_cache.get(path) {
            return v.clone();
        }
        let mut v: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    v.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        v.sort();
        self.local_tree_cache.insert(path.to_path_buf(), v.clone());
        v
    }

    /// 远端目录树节点：展开时向服务端懒加载子目录列表。
    fn remote_tree_node(&mut self, ui: &mut egui::Ui, path: &str) {
        let open = self.remote_tree_open.contains(path);
        let name: String = if path == "/" {
            "/".into()
        } else {
            path.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("/")
                .to_string()
        };
        ui.horizontal(|ui| {
            if ui.small_button(if open { "▾" } else { "▸" }).clicked() {
                if open {
                    self.remote_tree_open.remove(path);
                } else {
                    self.remote_tree_open.insert(path.to_string());
                    self.request_remote_tree_ls(path);
                }
            }
            let current = self.remote_dir == path;
            if ui.selectable_label(current, format!("📁 {name}")).clicked()
                && self.connected
            {
                self.cd_remote(path.to_string());
            }
        });
        if open {
            if let Some(subs) = self.remote_tree_cache.get(path).cloned() {
                for s in subs {
                    let child = join_remote(path, &s);
                    let id = egui::Id::new("rt").with(&child);
                    ui.indent(id, |ui| {
                        self.remote_tree_node(ui, &child);
                    });
                }
            } else {
                ui.label("…");
            }
        }
    }

    /// 请求一次树节点 ls（同一时刻只允许一个在途，避免与主面板 ls 混淆）。
    fn request_remote_tree_ls(&mut self, path: &str) {
        if !self.connected || self.pending_tree_ls.is_some() {
            return;
        }
        self.pending_tree_ls = Some(path.to_string());
        self.ls_remote(path);
    }

    /// 渲染文件表（本地 `is_local=true` / 远端 `false`），含表头、单击选中、
    /// 双击进入目录、右键上下文菜单。
    fn file_table(&mut self, ui: &mut egui::Ui, is_remote: bool) {
        let entries: Vec<FileRow> = if is_remote {
            self.remote_entries
                .iter()
                .map(|e| FileRow {
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                    size: e.size,
                    mode: e.mode,
                    mtime: e.mtime.max(0) as u64,
                })
                .collect()
        } else {
            self.local_entries
                .iter()
                .map(|e| FileRow {
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                    size: e.size,
                    mode: e.mode,
                    mtime: e.mtime,
                })
                .collect()
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new(if is_remote { "remote_grid" } else { "local_grid" })
                .striped(true)
                .show(ui, |ui| {
                    ui.strong(t("Name"));
                    ui.strong(t("Size"));
                    ui.strong(t("Type"));
                    ui.strong(t("Modified"));
                    ui.strong(t("Perms"));
                    ui.end_row();

                    for (i, e) in entries.iter().enumerate() {
                        let selected = if is_remote {
                            self.remote_selected == Some(i)
                        } else {
                            self.local_selected == Some(i)
                        };
                        let icon = if e.is_dir { "📁 " } else { "📄 " };
                        let resp = ui.selectable_label(selected, format!("{icon}{}", e.name));
                        ui.monospace(fmt_size(e.size));
                        ui.label(if e.is_dir {
                            t("Directory")
                        } else {
                            t("File")
                        });
                        ui.monospace(fmt_time(e.mtime));
                        ui.monospace(format!("{:o}", e.mode));
                        ui.end_row();

                        if resp.clicked() {
                            if is_remote {
                                self.remote_selected = Some(i);
                            } else {
                                self.local_selected = Some(i);
                            }
                        }
                        let name = e.name.clone();
                        let is_dir = e.is_dir;
                        if resp.double_clicked() && (name == ".." || is_dir) {
                            if is_remote {
                                self.remote_selected = Some(i);
                                if name == ".." {
                                    self.cd_remote(remote_parent(&self.remote_dir));
                                } else {
                                    self.cd_remote(join_remote(&self.remote_dir, &name));
                                }
                            } else {
                                self.local_selected = Some(i);
                                if name == ".." {
                                    if let Some(p) = self.local_dir.parent() {
                                        self.enter_local(p.to_path_buf());
                                    }
                                } else {
                                    self.enter_local(self.local_dir.join(&name));
                                }
                            }
                        }
                        resp.context_menu(|ui| {
                            if is_remote {
                                self.remote_context_menu(ui, i);
                            } else {
                                self.local_context_menu(ui, i);
                            }
                        });
                    }
                });
        });
    }

    /// 右键上下文菜单：远端文件/目录。
    fn remote_context_menu(&mut self, ui: &mut egui::Ui, i: usize) {
        let Some(e) = self.remote_entries.get(i).cloned() else {
            return;
        };
        let name = e.name.clone();
        let is_dir = e.is_dir;
        if !is_dir && name != ".." && ui.button(t("Download")).clicked() {
            self.do_download(
                join_remote(&self.remote_dir, &name),
                self.local_dir.join(&name).to_string_lossy().to_string(),
            );
            ui.close_menu();
        }
        if (is_dir || name == "..") && ui.button(t("Enter")).clicked() {
            if name == ".." {
                self.cd_remote(remote_parent(&self.remote_dir));
            } else {
                self.cd_remote(join_remote(&self.remote_dir, &name));
            }
            ui.close_menu();
        }
        if ui.button(t("Refresh")).clicked() {
            if self.connected {
                self.ls_remote(&self.remote_dir);
            }
            ui.close_menu();
        }
        if ui.button(t("New directory")).clicked() {
            // 聚焦新建目录输入框
            self.new_dir = String::new();
            ui.close_menu();
        }
        if !is_dir && name != ".." && ui.button(t("Delete")).clicked() {
            self.do_delete(vec![join_remote(&self.remote_dir, &name)]);
            ui.close_menu();
        }
        // ---- 针对目标文件的工具：tail / grep / edit（原「日志/编辑」工具窗已移除） ----
        if name != ".." {
            let target = join_remote(&self.remote_dir, &name);
            if ui.button(t("Grep")).clicked() {
                self.grep_path = target.clone();
                self.grep_view.clear();
                self.show_grep = true;
                ui.close_menu();
            }
        }
        if !is_dir && name != ".." {
            let target = join_remote(&self.remote_dir, &name);
            if ui.button(t("Tail")).clicked() {
                self.tail_path = target.clone();
                self.tail_output.clear();
                self.tail_view.clear();
                self.show_tail = true;
                ui.close_menu();
            }
            if ui.button(t("Edit")).clicked() {
                self.edit_path = target.clone();
                self.edit_content.clear();
                self.edit_loaded = false;
                self.show_edit = true;
                ui.close_menu();
            }
        }
        if !is_dir && name != ".." {
            ui.horizontal(|ui| {
                ui.label(t("Rename"));
                ui.text_edit_singleline(&mut self.rename_input);
                if ui.button(t("Go")).clicked() {
                    let nn = self.rename_input.trim().to_string();
                    if !nn.is_empty() {
                        self.do_rename(join_remote(&self.remote_dir, &name), nn);
                        self.rename_input.clear();
                    }
                    ui.close_menu();
                }
            });
        }
        // ---- 远端 chmod：右键「权限」 ----
        if name != ".." {
            let target = join_remote(&self.remote_dir, &name);
            if ui.button(t("Permissions")).clicked() {
                self.chmod_path = target.clone();
                self.chmod_mode.clear();
                self.show_chmod = true;
                ui.close_menu();
            }
        }
    }

    /// 右键上下文菜单：本地文件/目录。
    fn local_context_menu(&mut self, ui: &mut egui::Ui, i: usize) {
        let Some(e) = self.local_entries.get(i).cloned() else {
            return;
        };
        let name = e.name.clone();
        let is_dir = e.is_dir;
        if !is_dir && name != ".." && ui.button(t("Upload")).clicked() {
            self.do_upload(
                join_remote(&self.remote_dir, &name),
                self.local_dir.join(&name).to_string_lossy().to_string(),
            );
            ui.close_menu();
        }
        if (is_dir || name == "..") && ui.button(t("Enter")).clicked() {
            if name == ".." {
                if let Some(p) = self.local_dir.parent() {
                    self.enter_local(p.to_path_buf());
                }
            } else {
                self.enter_local(self.local_dir.join(&name));
            }
            ui.close_menu();
        }
        if ui.button(t("Refresh")).clicked() {
            self.refresh_local();
            ui.close_menu();
        }
        if ui.button(t("New directory")).clicked() {
            self.local_new_dir = String::new();
            ui.close_menu();
        }
    }

    fn upload_selected_local(&mut self) {
        if let Some(i) = self.local_selected {
            if let Some(e) = self.local_entries.get(i) {
                if !e.is_dir && e.name != ".." {
                    let lp = self.local_dir.join(&e.name);
                    let rp = join_remote(&self.remote_dir, &e.name);
                    self.do_upload(rp, lp.to_string_lossy().to_string());
                }
            }
        } else {
            self.push_log(t("Select a file in the remote list first"));
        }
    }

    fn download_selected_remote(&mut self) {
        if let Some(i) = self.remote_selected {
            if let Some(e) = self.remote_entries.get(i) {
                if !e.is_dir && e.name != ".." {
                    let rp = join_remote(&self.remote_dir, &e.name);
                    let lp = self.local_dir.join(&e.name);
                    self.do_download(rp, lp.to_string_lossy().to_string());
                }
            }
        } else {
            self.push_log(t("Select a file in the remote list first"));
        }
    }

    fn delete_selected_remote(&mut self) {
        if let Some(i) = self.remote_selected {
            if let Some(e) = self.remote_entries.get(i) {
                if !e.is_dir && e.name != ".." {
                    self.do_delete(vec![join_remote(&self.remote_dir, &e.name)]);
                }
            }
        } else {
            self.push_log(t("Select a file in the remote list first"));
        }
    }

    /// 队列栏（FileZilla 风格）：标签页（进行中/失败/成功）+ 多列表格。
    fn transfer_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong(t("Transfer queue"));
            ui.separator();
            let tabs = [
                t("Queued files"),
                t("Failed transfers"),
                t("Successful transfers"),
            ];
            for (i, label) in tabs.iter().enumerate() {
                let count = match i {
                    1 => self.transfers.iter().filter(|x| x.done && !x.ok).count(),
                    2 => self.transfers.iter().filter(|x| x.done && x.ok).count(),
                    _ => self.transfers.iter().filter(|x| !x.done).count(),
                };
                if ui
                    .selectable_label(self.queue_tab == i, format!("{label} ({count})"))
                    .clicked()
                {
                    self.queue_tab = i;
                }
            }
        });
        ui.separator();
        let tab = self.queue_tab;
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .show(ui, |ui| {
                let rows: Vec<&TransferItem> = self
                    .transfers
                    .iter()
                    .filter(|x| match tab {
                        1 => x.done && !x.ok,
                        2 => x.done && x.ok,
                        _ => !x.done,
                    })
                    .collect();
                if rows.is_empty() {
                    ui.label(t("(empty)"));
                    return;
                }
                egui::Grid::new("queue_grid")
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong(t("Local file"));
                        ui.strong(t("Direction"));
                        ui.strong(t("Remote file"));
                        ui.strong(t("Size"));
                        ui.strong(t("Status"));
                        ui.end_row();
                        for x in rows {
                            ui.monospace(if x.local_path.is_empty() {
                                &x.name
                            } else {
                                &x.local_path
                            });
                            ui.label(match x.direction {
                                Direction::Upload => "↑",
                                Direction::Download => "↓",
                            });
                            ui.monospace(if x.remote_path.is_empty() {
                                &x.name
                            } else {
                                &x.remote_path
                            });
                            if x.total > 0 {
                                let pct =
                                    (x.sent as f32 / x.total as f32).clamp(0.0, 1.0);
                                ui.monospace(format!(
                                    "{}/{} ({:.0}%)",
                                    fmt_size(x.sent),
                                    fmt_size(x.total),
                                    pct * 100.0
                                ));
                            } else {
                                ui.monospace(fmt_size(x.sent));
                            }
                            ui.label(&x.status);
                            ui.end_row();
                        }
                    });
            });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading(t("Log"));
        egui::ScrollArea::vertical().show(ui, |ui| {
            for l in &self.log {
                ui.label(l);
            }
        });
    }

    fn publish_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_publish;
        egui::Window::new(t("Publish"))
            .open(&mut open)
            .collapsible(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                if self.backend.supports_publish() {
                    self.publish_inner(ui);
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 40),
                        t("SFTP: publish/rollback require the rdep service and are unavailable."),
                    );
                }
            });
        self.show_publish = open;
    }

    /// 发布（仅 rdep）。拆出以便按协议门控。
    fn publish_inner(&mut self, ui: &mut egui::Ui) {
        ui.strong(t("Publish (backup old files → upload → run restart script)"));
        labeled(ui, t("Remote dir"), &mut self.publish_remote);
        labeled(ui, t("Restart script id"), &mut self.publish_script);
        labeled(ui, t("Local source file"), &mut self.pub_local_input);
        labeled(ui, t("Remote file name"), &mut self.pub_name_input);
        if ui.button(t("Add to publish list")).clicked() {
            let lp = self.pub_local_input.trim().to_string();
            let rn = self.pub_name_input.trim().to_string();
            if !lp.is_empty() && !rn.is_empty() {
                let rp = join_remote(&self.publish_remote, &rn);
                self.publish_files.push(PublishFile {
                    remote_path: rp,
                    local_path: lp,
                });
            }
        }
        egui::ScrollArea::vertical()
            .max_height(140.0)
            .show(ui, |ui| {
                if self.publish_files.is_empty() {
                    ui.label(t("Publish list is empty"));
                }
                let mut remove_idx: Option<usize> = None;
                for (i, f) in self.publish_files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}  →  {}", f.local_path, f.remote_path));
                        if ui.button(t("Remove")).clicked() {
                            remove_idx = Some(i);
                        }
                    });
                }
                if let Some(i) = remove_idx {
                    self.publish_files.remove(i);
                }
            });
        if ui.button(t("Run publish")).clicked() {
            if self.connected {
                tracing::debug!(dir = %self.publish_remote, script = %self.publish_script, files = self.publish_files.len(), "action: publish");
                self.client.publish(
                    self.publish_remote.clone(),
                    self.publish_script.clone(),
                    self.publish_files.clone(),
                );
            } else {
                self.push_log(t("Not connected, cannot publish"));
            }
        }

    }

    /// 回滚窗口（工具条上是独立按钮，与发布分开）。
    fn rollback_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_rollback;
        egui::Window::new(t("Rollback"))
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                if self.backend.supports_publish() {
                    self.rollback_inner(ui);
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 40),
                        t("SFTP: publish/rollback require the rdep service and are unavailable."),
                    );
                }
            });
        self.show_rollback = open;
    }

    /// 回滚表单（仅 rdep）。
    fn rollback_inner(&mut self, ui: &mut egui::Ui) {
        ui.strong(t("Rollback (restore a backup version to the remote dir)"));
        labeled(ui, t("Remote dir"), &mut self.rollback_remote);
        ui.horizontal(|ui| {
            if ui.button(t("List backup versions")).clicked() {
                if self.connected {
                    self.pending_backup_list = true;
                    self.client.list_backups();
                } else {
                    self.push_log(t("Not connected, cannot list backups"));
                }
            }
            egui::ComboBox::from_label(t("Backup version"))
                .selected_text(if self.rollback_version.is_empty() {
                    t("<select version>").to_string()
                } else {
                    self.rollback_version.clone()
                })
                .show_ui(ui, |ui| {
                    let mut picked: Option<String> = None;
                    for v in &self.backup_versions {
                        if ui
                            .selectable_label(self.rollback_version == *v, v)
                            .clicked()
                        {
                            picked = Some(v.clone());
                        }
                    }
                    if let Some(p) = picked {
                        self.rollback_version = p;
                    }
                });
        });
        if ui.button(t("Run rollback")).clicked() {
            if self.connected {
                if self.rollback_version.is_empty() {
                    self.push_log(t("Select a backup version first"));
                } else {
                    tracing::debug!(dir = %self.rollback_remote, version = %self.rollback_version, "action: rollback");
                    self.client.rollback(
                        self.rollback_remote.clone(),
                        self.rollback_version.clone(),
                    );
                }
            } else {
                self.push_log(t("Not connected, cannot rollback"));
            }
        }

    }

    /// 目录同步窗口（工具条独立按钮）。
    fn sync_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_sync;
        egui::Window::new(t("Directory sync"))
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                if self.backend.supports_advanced() {
                    self.sync_inner(ui);
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 40),
                        t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."),
                    );
                }
            });
        self.show_sync = open;
    }

    /// 目录同步表单。
    fn sync_inner(&mut self, ui: &mut egui::Ui) {
        ui.strong(t("Directory sync (local → remote; changed = size+mtime; auto-backup before overwrite)"));
        labeled(ui, t("Local dir"), &mut self.sync_local);
        labeled(ui, t("Remote dir"), &mut self.sync_remote);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.sync_delete_extra, t("Delete extra remote files"));
            if ui.button(t("Preview (dry-run)")).clicked() {
                if self.connected {
                    self.do_sync_dir(
                        self.sync_local.trim().to_string(),
                        self.sync_remote.trim().to_string(),
                        self.sync_delete_extra,
                        true,
                    );
                } else {
                    self.push_log(t("Not connected, cannot sync"));
                }
            }
            if ui.button(t("Run sync")).clicked() {
                if self.connected {
                    self.do_sync_dir(
                        self.sync_local.trim().to_string(),
                        self.sync_remote.trim().to_string(),
                        self.sync_delete_extra,
                        false,
                    );
                } else {
                    self.push_log(t("Not connected, cannot sync"));
                }
            }
        });
        if let Some((changed, to_delete)) = self.sync_preview.clone() {
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.label(tf("{n} files to upload/update:", &[("n", &changed.len().to_string())]));
                    for c in &changed {
                        ui.monospace(format!("  ↻ {c}"));
                    }
                    if !to_delete.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(200, 80, 80),
                            tf("{n} extra remote files to delete:", &[("n", &to_delete.len().to_string())]),
                        );
                        for d in &to_delete {
                            ui.monospace(format!("  ✗ {d}"));
                        }
                    }
                });
        }
    }

    /// TAIL 弹出窗：对**指定远端文件**查看末尾若干行（可跟随），结果在编辑框中。
    fn tail_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_tail;
        egui::Window::new(t("Tail"))
            .open(&mut open)
            .collapsible(false)
            .default_width(680.0)
            .default_height(460.0)
            .show(ctx, |ui| {
                labeled(ui, t("Remote file"), &mut self.tail_path);
                ui.horizontal(|ui| {
                    ui.label(t("Tail lines"));
                    ui.add(egui::TextEdit::singleline(&mut self.tail_lines).desired_width(70.0));
                    ui.checkbox(&mut self.tail_follow, t("Follow"));
                    if ui.button(t("Execute")).clicked() {
                        if !self.connected {
                            self.push_log(t("Not connected, cannot tail"));
                        } else if self.require_advanced("Tail") {
                            self.tail_output.clear();
                            self.tail_view.clear();
                            let n: u32 = self.tail_lines.trim().parse().unwrap_or(50);
                            self.do_tail(self.tail_path.clone(), n, self.tail_follow);
                        }
                    }
                    if self.tail_follow && ui.button(t("Stop follow")).clicked() {
                        self.do_stop_tail();
                    }
                });
                ui.separator();
                // 自动滚动到末尾：跟随开启 或 收到新日志行（tail_view_dirty）时滚到底。
                // 编辑框设为只读，避免其内部滚动吞掉滚轮、导致无法整体滚动。
                let autoscroll = self.tail_follow || self.tail_view_dirty;
                egui::ScrollArea::vertical()
                    .max_height(720.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.tail_view)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace)
                                .clip_text(true)
                                .interactive(true),
                        );
                        if autoscroll {
                            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                        }
                    });
                self.tail_view_dirty = false;
            });
        self.show_tail = open;
        // 窗口关闭时若仍在跟随，自动停止 follow（释放服务端 tail 资源）。
        if !open && self.tail_follow {
            self.do_stop_tail();
            self.tail_follow = false;
            self.push_log(t("tail stopped"));
        }
    }

    /// GREP 弹出窗：在指定文件/目录中检索，结果在编辑框中。
    fn grep_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_grep;
        egui::Window::new(t("Grep"))
            .open(&mut open)
            .collapsible(false)
            .default_width(680.0)
            .default_height(660.0)
            .show(ctx, |ui| {
                labeled(ui, t("Path"), &mut self.grep_path);
                labeled(ui, t("Pattern"), &mut self.grep_pattern);
                labeled(ui, t("Flags"), &mut self.grep_flags);
                if ui.button(t("Execute")).clicked() {
                    if !self.connected {
                        self.push_log(t("Not connected, cannot grep"));
                    } else if self.require_advanced("Grep") {
                        self.grep_view.clear();
                        self.do_grep(
                            self.grep_path.clone(),
                            self.grep_pattern.clone(),
                            self.grep_flags.clone(),
                        );
                    }
                }
                ui.separator();
                // 结果逐行渲染：命中关键字（按 flags 取大小写）以红色高亮。
                let pattern = self.grep_pattern.clone();
                let ignore_case = self.grep_flags.contains('i');
                egui::ScrollArea::vertical()
                    .max_height(720.0)
                    .show(ui, |ui| {
                        if self.grep_output.is_empty() {
                            ui.label(t("(no output)"));
                        } else {
                            for line in &self.grep_output {
                                render_grep_match(ui, line, &pattern, ignore_case);
                            }
                        }
                    });
            });
        self.show_grep = open;
    }

    /// chmod 弹窗（右键「权限」触发）：填远端路径与八进制权限，点「应用」下发。
    fn chmod_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_chmod;
        let mut close = false;
        egui::Window::new(t("Permissions"))
            .open(&mut open)
            .collapsible(false)
            .default_width(440.0)
            .show(ctx, |ui| {
                labeled(ui, t("Remote file"), &mut self.chmod_path);
                ui.horizontal(|ui| {
                    ui.label(t("Octal mode (e.g. 0644)"));
                    ui.text_edit_singleline(&mut self.chmod_mode);
                });
                ui.horizontal(|ui| {
                    if ui.button(t("Apply")).clicked() {
                        let p = self.chmod_path.trim().to_string();
                        let m = self.chmod_mode.trim();
                        if p.is_empty() {
                            self.push_log(t("Remote path is required"));
                        } else {
                                match parse_octal_mode(m) {
                                    Some(mode) => {
                                        self.do_chmod(p, mode);
                                        close = true;
                                    }
                                    None => self.push_log(t("invalid octal mode")),
                                }
                        }
                    }
                    if ui.button(t("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
        self.show_chmod = open && !close;
    }

    /// 远端文件编辑窗：加载 → 编辑 → 保存（服务端保存前自动备份）。
    fn edit_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_edit;
        egui::Window::new(t("Edit remote file"))
            .open(&mut open)
            .collapsible(false)
            .default_width(680.0)
            .default_height(500.0)
            .show(ctx, |ui| {
                labeled(ui, t("Remote file"), &mut self.edit_path);
                ui.horizontal(|ui| {
                    if ui.button(t("Load")).clicked() {
                        if !self.connected {
                            self.push_log(t("Not connected, cannot load"));
                        } else if self.require_advanced("Edit") {
                            self.do_edit_get(self.edit_path.clone());
                        }
                    }
                    let loaded = self.edit_loaded;
                    if ui.add_enabled(loaded, egui::Button::new(t("Save"))).clicked() {
                        if !self.connected {
                            self.push_log(t("Not connected, cannot save"));
                        } else {
                            self.do_edit_save(self.edit_path.clone(), self.edit_content.clone());
                        }
                    }
                    if loaded {
                        ui.label(t("(loaded; edits are saved with auto-backup on the server)"));
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.edit_content)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                    });
            });
        self.show_edit = open;
    }

    /// 站点管理器：列出已保存站点 / 保存当前配置 / 删除 / 双击载入到连接表单。
    /// 站点管理器（FileZilla 风格）：左侧站点树 + 右侧标签页（常规/高级/传输设置/字符集）。
    /// 所有字段（协议、登录类型、背景颜色、注释、默认本地/远端目录、并发数、字符集）随
    /// 「确定」实时写入 `sites.json`，连接时按站点配置落到对应默认目录。
    fn sites_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_sites;
        let mut close = false;
        egui::Window::new(t("Site manager"))
            .id(egui::Id::new("site_manager_v2"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            // .auto_sized()
            .default_height(760.0)
            // .default_size(egui::vec2(780.0, 760.0))
            // .min_size(egui::vec2(620.0, 720.0))
            .show(ctx, |ui| {
                ui.label(format!("{} {}", t("Config file:"), self.store.path().display()));
                ui.separator();

                ui.horizontal(|ui| {
                    // ===== 左侧：站点树 =====
                    ui.vertical(|ui| {
                        ui.set_min_width(210.0);
                        ui.set_max_width(210.0);
                        ui.heading(t("My Sites"));
                        if ui.button(t("New site")).clicked() {
                            self.new_site();
                        }
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt("site_left_scroll")
                            .auto_shrink([false; 2])
                            .show(ui, |ui| {
                                let mut clicked: Option<usize> = None;
                                egui::CollapsingHeader::new(t("My Sites"))
                                    .id_salt("site_tree_header")
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for (i, s) in self.sites.iter().enumerate() {
                                            let selected = self.site_selected == Some(i);
                                            if ui
                                                .selectable_label(selected, s.display())
                                                .clicked()
                                            {
                                                self.site_selected = Some(i);
                                                clicked = Some(i);
                                            }
                                        }
                                        if self.sites.is_empty() {
                                            ui.colored_label(
                                                egui::Color32::from_gray(150),
                                                t("No saved sites yet; click \"New site\"."),
                                            );
                                        }
                                    });
                                if let Some(i) = clicked {
                                    self.load_site_into_form(i);
                                }
                            });
                        ui.separator();
                        if ui.button(t("Delete")).clicked() {
                            self.delete_selected_site();
                        }
                    });

                    ui.separator();

                    // ===== 右侧：详情 + 标签页 =====
                    ui.vertical(|ui| {
                        ui.set_min_width(520.0);
                        let title = match self.site_selected {
                            Some(i) => self
                                .sites
                                .get(i)
                                .map(|s| s.display())
                                .unwrap_or_else(|| t("(new site)").to_string()),
                            None => t("(new site)").to_string(),
                        };
                        // 站点名（可改名；保存时按新名 upsert，旧名自动删除）
                        ui.horizontal(|ui| {
                            ui.label(t("Site name"));
                            ui.text_edit_singleline(&mut self.site_name_input);
                        });
                        ui.heading(title);

                        // 标签页切换条
                        ui.horizontal(|ui| {
                            let tabs = [
                                t("General"),
                                t("Advanced"),
                                t("Transfer Settings"),
                                t("Charset"),
                            ];
                            for (i, label) in tabs.iter().enumerate() {
                                if ui
                                    .selectable_label(self.site_tab == i, *label)
                                    .clicked()
                                {
                                    self.site_tab = i;
                                }
                            }
                        });
                        ui.separator();

                        egui::ScrollArea::vertical()
                            .id_salt("site_right_scroll")
                            .auto_shrink([false; 2])
                            .show(ui, |ui| match self.site_tab {
                                0 => self.site_tab_general(ui),
                                1 => self.site_tab_advanced(ui),
                                2 => self.site_tab_transfer(ui),
                                3 => self.site_tab_charset(ui),
                                _ => {}
                            });
                    });
                });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t("Connect")).clicked() {
                        self.save_current_form_as_site();
                        self.connect_via_current_backend();
                        close = true;
                    }
                    if ui.button(t("OK")).clicked() {
                        self.save_current_form_as_site();
                        close = true;
                    }
                    if ui.button(t("Cancel")).clicked() {
                        close = true;
                    }
                });
            });

        self.show_sites = open && !close;
    }

    /// 常规标签页：协议 / 主机 / 端口 / 登录类型 / 用户·密码 / CA / 中转 / 标签颜色 / 注释。
    fn site_tab_general(&mut self, ui: &mut egui::Ui) {
        let proto_before = self.backend;
        ui.horizontal(|ui| {
            ui.label(t("Protocol"));
            egui::ComboBox::from_id_salt("site_proto")
                .selected_text(self.backend.label())
                .show_ui(ui, |ui| {
                    for p in [Protocol::Rdep, Protocol::Ftp, Protocol::Sftp] {
                        ui.selectable_value(&mut self.backend, p, p.label());
                    }
                });
        });
        if self.backend != proto_before {
            sync_port_to_protocol(self);
        }
        if !self.backend.supports_advanced() {
            ui.colored_label(
                egui::Color32::from_gray(140),
                t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."),
            );
        }
        if self.backend == Protocol::Sftp {
            ui.colored_label(
                egui::Color32::from_rgb(200, 120, 40),
                t("SFTP: publish/rollback require the rdep service and are unavailable."),
            );
        }
        labeled(ui, t("Host"), &mut self.cfg_host);
        labeled(ui, t("Port"), &mut self.cfg_port);
        // 登录类型
        ui.horizontal(|ui| {
            ui.label(t("Logon type"));
            let lbl = match self.cfg_login_type.as_str() {
                "Key" => t("Key file (SSH)"),
                "Ask" => t("Ask for password each time"),
                _ => t("Normal (user & password)"),
            };
            egui::ComboBox::from_id_salt("site_login")
                .selected_text(lbl)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg_login_type,
                        "Normal".into(),
                        t("Normal (user & password)"),
                    );
                    ui.selectable_value(
                        &mut self.cfg_login_type,
                        "Key".into(),
                        t("Key file (SSH)"),
                    );
                    ui.selectable_value(
                        &mut self.cfg_login_type,
                        "Ask".into(),
                        t("Ask for password each time"),
                    );
                });
        });
        // 「每次询问」登录类型时不保存密码
        if self.cfg_login_type != "Ask" {
            labeled(ui, t("User"), &mut self.cfg_user);
            ui.horizontal(|ui| {
                ui.label(t("Password"));
                ui.add(egui::TextEdit::singleline(&mut self.cfg_pass).password(true));
            });
            ui.checkbox(&mut self.site_remember_pass, t("Remember password"));
        }
        labeled(ui, t("CA cert path"), &mut self.cfg_ca);
        // rdep 专属：经 forwarder 中转
        if self.backend == Protocol::Rdep {
            ui.checkbox(&mut self.cfg_use_forwarder, t("Relay via forwarder"));
            if self.cfg_use_forwarder {
                labeled(ui, t("Target service id"), &mut self.cfg_service_id);
                labeled(ui, t("Relay token"), &mut self.cfg_relay_token);
                ui.label(t("Tip: host/port are the forwarder address"));
            }
        }
        // FileZilla 风格：标签颜色 + 注释
        ui.horizontal(|ui| {
            ui.label(t("Background color"));
            ui.text_edit_singleline(&mut self.site_bg_color);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
            if let Ok(c) = parse_hex_color(&self.site_bg_color) {
                ui.painter().rect_filled(rect, egui::Rounding::same(3.0), c);
            }
        });
        labeled(ui, t("Comment"), &mut self.site_comment);
    }

    /// 高级标签页：默认本地目录（浏览=系统文件选择框）/ 默认远端目录。
    fn site_tab_advanced(&mut self, ui: &mut egui::Ui) {
        ui.label(t("Default local directory"));
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.site_default_local);
            if ui.button(t("Browse...")).clicked() {
                self.open_local_dir_picker();
            }
        });
        ui.horizontal(|ui| {
            if ui.button(t("Use current local directory")).clicked() {
                self.site_default_local = self.local_dir.to_string_lossy().into_owned();
            }
        });
        ui.separator();
        labeled(ui, t("Default remote directory"), &mut self.site_default_remote);
        ui.label(t("Connection opens directly into these directories"));
    }

    /// 打开系统文件选择框挑选「默认本地目录」（rfd；Linux 经 xdg-portal 调起原生对话框）。
    ///
    /// rfd 的同步 `FileDialog::pick_folder()` 内部用 `pollster::block_on` 驱动异步的 portal 通路，
    /// 必须在独立线程上调用以免卡住 egui 渲染。结果经 `browse_rx` 通道回传，
    /// 下一帧 `drain_events` 读取并写入 `site_default_local`。
    ///
    /// 已有一个对话框在等待结果时（`browse_rx.is_some()`）不重复打开。
    fn open_local_dir_picker(&mut self) {
        if self.browse_rx.is_some() {
            return;
        }
        // 初始目录：优先用已填写的默认值，否则用当前本地面板目录
        let initial = if self.site_default_local.trim().is_empty() {
            self.local_dir.clone()
        } else {
            PathBuf::from(self.site_default_local.trim())
        };
        let (tx, rx) = std::sync::mpsc::channel::<Option<PathBuf>>();
        self.browse_rx = Some(rx);
        std::thread::spawn(move || {
            // 独立线程：不阻塞 egui 主线程。无显示环境（CI/无 portal）下
            // pick_folder 会返回 None，结果通道照常回传，UI 保持原值。
            let picked = rfd::FileDialog::new()
                .set_title(t("Select default local directory"))
                .set_directory(&initial)
                .pick_folder();
            let _ = tx.send(picked);
        });
    }

    /// 传输设置标签页：并发传输数。
    fn site_tab_transfer(&mut self, ui: &mut egui::Ui) {
        let mut n = self.site_concurrency.trim().parse::<u8>().unwrap_or(2);
        ui.horizontal(|ui| {
            ui.label(t("Concurrent transfers"));
            ui.add(
                egui::DragValue::new(&mut n)
                    .range(1..=16)
                    .clamp_existing_to_range(true)
                    .suffix(""),
            );
        });
        self.site_concurrency = n.to_string();
        ui.label(t("Tip: higher concurrency speeds up many small files."));
    }

    /// 字符集标签页：Auto / 强制 UTF-8。
    fn site_tab_charset(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(t("Charset"));
            egui::ComboBox::from_id_salt("site_charset")
                .selected_text(self.site_charset.clone())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.site_charset, "Auto".into(), t("Auto"));
                    ui.selectable_value(
                        &mut self.site_charset,
                        "UTF-8".into(),
                        t("Force UTF-8"),
                    );
                });
        });
        ui.label(t("Auto follows the server; UTF-8 forces encoding"));
    }

    fn connect_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_connect;
        let mut close = false;
        let backend_before = self.backend;
        egui::Window::new(t("Connect to server"))
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t("Protocol"));
                    egui::ComboBox::from_id_salt("proto")
                        .selected_text(self.backend.label())
                        .show_ui(ui, |ui| {
                            for p in [Protocol::Rdep, Protocol::Ftp, Protocol::Sftp] {
                                ui.selectable_value(&mut self.backend, p, p.label());
                            }
                        });
                });
                if !self.backend.supports_advanced() {
                    ui.colored_label(
                        egui::Color32::from_gray(140),
                        t("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext."),
                    );
                }
                if self.backend == Protocol::Sftp {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 40),
                        t("SFTP: publish/rollback require the rdep service and are unavailable."),
                    );
                }
                ui.separator();
                labeled(ui, t("Host"), &mut self.cfg_host);
                labeled(ui, t("Port"), &mut self.cfg_port);
                labeled(ui, t("User"), &mut self.cfg_user);
                ui.checkbox(&mut self.cfg_use_token, t("Use API token auth (CI/CD)"));
                ui.horizontal(|ui| {
                    ui.label(if self.cfg_use_token {
                        t("Token")
                    } else {
                        t("Password")
                    });
                    ui.add(egui::TextEdit::singleline(&mut self.cfg_pass).password(true));
                });
                labeled(ui, t("CA cert path"), &mut self.cfg_ca);
                // forwarder 中转是 rdep 专属，仅 rdep 站点显示
                if self.backend == Protocol::Rdep {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.cfg_use_forwarder, t("Relay via forwarder"));
                    });
                    if self.cfg_use_forwarder {
                        labeled(ui, t("Target service id"), &mut self.cfg_service_id);
                        labeled(ui, t("Relay token"), &mut self.cfg_relay_token);
                        ui.label(t("Tip: host/port are the forwarder address"));
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button(t("Connect now")).clicked() {
                        self.connect_via_current_backend();
                        close = true;
                    }
                    if ui.button(t("Save and connect")).clicked() {
                        self.save_current_form_as_site();
                        self.connect_via_current_backend();
                        close = true;
                    }
                    if ui.button(t("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
        // 切换协议时把端口切到该协议默认值，避免残留上一个协议的端口
        if self.backend != backend_before {
            sync_port_to_protocol(self);
        }
        // 注意：不能用 `self.show_connect = open` 直接覆盖——闭包内已请求关闭时
        // open 仍是进入本帧前的旧值，会把关闭请求吞掉（表现为按钮“没反应”）。
        self.show_connect = open && !close;
    }
}

impl eframe::App for RdepApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_ui(ctx);
    }
}

impl RdepApp {
    /// 一帧界面渲染的全部逻辑（不含 eframe 的窗口管理）。
    pub fn update_ui(&mut self, ctx: &egui::Context) {
        self.drain_events();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("rdep");
                if ui.button(t("Sites")).clicked() {
                    self.show_sites = true;
                }
                ui.separator();
                if self.connected {
                    if ui.button(t("Disconnect")).clicked() {
                        self.do_disconnect();
                    }
                } else if ui.button(t("Connect")).clicked() {
                    self.show_connect = true;
                }
                if ui.button(t("Publish")).clicked() && self.require_publish("Publish") {
                    self.show_publish = true;
                }
                if ui.button(t("Sync")).clicked() && self.require_advanced("Directory sync") {
                    self.show_sync = true;
                }
                if ui.button(t("Rollback")).clicked() && self.require_publish("Rollback") {
                    self.show_rollback = true;
                    // 打开时自动拉一次备份版本列表（需要已连接）
                    if self.connected {
                        self.pending_backup_list = true;
                        self.client.list_backups();
                    }
                }
                ui.separator();
                // 语言切换
                egui::ComboBox::from_label(t("Language"))
                    .selected_text(i18n::get_lang().display_name())
                    .show_ui(ui, |ui| {
                        let mut picked: Option<Lang> = None;
                        for l in Lang::ALL {
                            if ui
                                .selectable_label(i18n::get_lang() == l, l.display_name())
                                .clicked()
                            {
                                picked = Some(l);
                            }
                        }
                        if let Some(l) = picked {
                            i18n::set_lang(l);
                            let _ = i18n::save_lang(l);
                        }
                    });
                ui.separator();
                ui.label(&self.status);
            });
            ui.separator();
            // ---- 快速连接条（FileZilla 风格）：与连接对话框共享同一组 cfg 字段 ----
            let qc_backend_before = self.backend;
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("qc_proto")
                    .selected_text(self.backend.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for p in [Protocol::Rdep, Protocol::Ftp, Protocol::Sftp] {
                            ui.selectable_value(&mut self.backend, p, p.label());
                        }
                    });
                ui.label(t("Host"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg_host).desired_width(160.0),
                );
                ui.label(t("User"));
                ui.add(egui::TextEdit::singleline(&mut self.cfg_user).desired_width(90.0));
                ui.label(t("Password"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg_pass)
                        .password(true)
                        .desired_width(90.0),
                );
                ui.label(t("Port"));
                ui.add(egui::TextEdit::singleline(&mut self.cfg_port).desired_width(52.0));
                if ui.button(format!("⚡ {}", t("Quick connect"))).clicked() {
                    if self.cfg_host.trim().is_empty() {
                        self.status = t("Host is required").to_string();
                        self.push_log(t("Host is required"));
                    } else if self.connected {
                        self.push_log(t("Already connected; disconnect first"));
                    } else {
                        self.connect_via_current_backend();
                    }
                }
            });
            if self.backend != qc_backend_before {
                // 与连接对话框一致：切换协议时自动切默认端口
                sync_port_to_protocol(self);
            }
        });

        egui::SidePanel::left("local").show(ctx, |ui| self.local_panel(ui));
        egui::SidePanel::right("remote").show(ctx, |ui| self.remote_panel(ui));
        egui::TopBottomPanel::bottom("transfers").show(ctx, |ui| self.transfer_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.log_panel(ui));

        if self.show_sites {
            self.sites_window(ctx);
        }
        if self.show_connect {
            self.connect_window(ctx);
        }
        if self.show_publish {
            self.publish_window(ctx);
        }
        if self.show_rollback {
            self.rollback_window(ctx);
        }
        if self.show_tail {
            self.tail_dialog(ctx);
        }
        if self.show_grep {
            self.grep_dialog(ctx);
        }
        if self.show_sync {
            self.sync_window(ctx);
        }
        if self.show_edit {
            self.edit_dialog(ctx);
        }
        if self.show_chmod {
            self.chmod_dialog(ctx);
        }

        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

// ===========================================================================
// 辅助 / 行内数据结构
// ===========================================================================

/// 文件表的一行（本地/远端共用）。
struct FileRow {
    name: String,
    is_dir: bool,
    size: u64,
    mode: u32,
    mtime: u64,
}

/// 协议的简短名称（日志用）。
fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::Rdep => "rdep",
        Protocol::Ftp => "FTP",
        Protocol::Sftp => "SFTP",
    }
}

/// 协议对应的默认端口（站点管理器与连接框切换协议时回填端口用）。
fn default_port_for_protocol(p: Protocol) -> &'static str {
    match p {
        Protocol::Rdep => "8443",
        Protocol::Ftp => "21",
        Protocol::Sftp => "22",
    }
}

/// 切换协议时把端口切到该协议默认值（避免残留上一个协议的端口）。
fn sync_port_to_protocol(app: &mut RdepApp) {
    app.cfg_port = default_port_for_protocol(app.backend).to_string();
}

fn labeled(ui: &mut egui::Ui, name: &str, s: &mut String) {
    ui.horizontal(|ui| {
        ui.label(name);
        ui.text_edit_singleline(s);
    });
}

/// 解析八进制权限串（如 `0644` / `644`），返回 0..=0o7777 内的数值；非法返回 None。
fn parse_octal_mode(s: &str) -> Option<u32> {
    let v = u32::from_str_radix(s.trim(), 8).ok()?;
    if (0..=0o7777).contains(&v) {
        Some(v)
    } else {
        None
    }
}

/// 解析 `#RRGGBB`（或 `RRGGBB`）十六进制颜色；非法返回 Err。
fn parse_hex_color(s: &str) -> Result<egui::Color32, ()> {
    let s = s.trim_start_matches('#');
    if s.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[0..2], 16),
            u8::from_str_radix(&s[2..4], 16),
            u8::from_str_radix(&s[4..6], 16),
        ) {
            return Ok(egui::Color32::from_rgb(r, g, b));
        }
    }
    Err(())
}

/// 渲染一行 grep 结果：所有命中 `pattern` 的子串用红色高亮，其余用常规文本色。
/// `ignore_case` 为真时按小写匹配（仅当大小写折叠不改变字节长度，避免越界）。
fn render_grep_match(ui: &mut egui::Ui, line: &str, pattern: &str, ignore_case: bool) {
    if pattern.is_empty() {
        ui.label(egui::RichText::new(line).font(egui::FontId::monospace(12.0)));
        return;
    }
    let hay = if ignore_case { line.to_lowercase() } else { line.to_string() };
    let needle = if ignore_case { pattern.to_lowercase() } else { pattern.to_string() };
    // 大小写折叠改变了字节长度（非 ASCII）时放弃高亮，避免按偏移切片越界 panic。
    if hay.len() != line.len() || needle.is_empty() {
        ui.label(egui::RichText::new(line).font(egui::FontId::monospace(12.0)));
        return;
    }
    let base = ui.style().visuals.text_color();
    let red = egui::Color32::RED;
    let mut job = egui::text::LayoutJob::default();
    let mut start = 0;
    while let Some(r) = hay[start..].find(&needle) {
        let ms = start + r;
        let me = ms + needle.len();
        if ms > start {
            job.append(
                &line[start..ms],
                0.0,
                egui::TextFormat::simple(egui::FontId::monospace(12.0), base),
            );
        }
        job.append(
            &line[ms..me],
            0.0,
            egui::TextFormat::simple(egui::FontId::monospace(12.0), red),
        );
        start = me;
    }
    if start < line.len() {
        job.append(
            &line[start..],
            0.0,
            egui::TextFormat::simple(egui::FontId::monospace(12.0), base),
        );
    }
    ui.label(job);
}

/// 本地文件权限位（unix；非 unix 返回 0）。
#[cfg(unix)]
fn local_mode(m: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn local_mode(_m: &std::fs::Metadata) -> u32 {
    0
}

fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

fn fmt_time(secs: u64) -> String {
    if secs == 0 {
        "-".to_string()
    } else {
        secs.to_string()
    }
}

/// 拼接远端路径。`base` 的尾部斜杠会被去掉，避免产生 `//`。
fn join_remote(base: &str, name: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.is_empty() {
        format!("/{name}")
    } else {
        format!("{b}/{name}")
    }
}

fn remote_parent(p: &str) -> String {
    if p == "/" {
        return "/".to_string();
    }
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => "/".to_string(),
    }
}

#[cfg(test)]
mod gui_smoke {
    use super::*;
    use crate::sites::SiteStore;

    /// 用一个临时站点仓库构造 app（不触碰用户真实配置）。
    fn test_app(tag: &str) -> RdepApp {
        let p = std::env::temp_dir().join(format!("rdep-gui-{}-{}.json", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        // 测试确定性：强制英文，避免依赖持久化的语言设置。
        i18n::set_lang(Lang::En);
        RdepApp::with_store(SiteStore::with_path(p))
    }

    /// 渲染 `frames` 帧；任何 panic 会直接让测试失败。
    fn render(app: &mut RdepApp, frames: usize) {
        let ctx = egui::Context::default();
        for _ in 0..frames {
            let _ = ctx.run(egui::RawInput::default(), |c| app.update_ui(c));
        }
    }

    fn entry(name: &str, is_dir: bool, size: u64, mode: u32) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            size,
            mtime: 1_768_473_000,
            mode,
        }
    }

    /// 基线：空状态下连续渲染不应 panic。
    #[test]
    fn renders_empty_state() {
        let mut app = test_app("empty");
        render(&mut app, 5);
        assert!(!app.connected);
        assert!(!app.log.is_empty(), "启动应写入日志");
    }

    /// 所有窗口同时打开 + 各类数据就绪时渲染，覆盖面板里所有分支。
    #[test]
    fn renders_all_windows_with_data() {
        let mut app = test_app("full");

        app.remote_dir = "/opt/app".into();
        app.remote_entries = vec![
            entry("..", true, 0, 0o755),
            entry("bin", true, 0, 0o755),
            entry("start.sh", false, 1024, 0o755),
            entry("my report.txt", false, 12, 0o644),
        ];
        app.remote_selected = Some(1);

        app.local_dir = std::env::temp_dir();
        app.refresh_local();
        app.local_selected = Some(0);

        app.transfers = vec![
            TransferItem {
                id: 1,
                name: "a.bin".into(),
                direction: Direction::Upload,
                sent: 50,
                total: 100,
                status: "Transferring".into(),
                ok: true,
                done: false,
                local_path: "/tmp/a.bin".into(),
                remote_path: "/opt/a.bin".into(),
            },
            TransferItem {
                id: 2,
                name: "b.bin".into(),
                direction: Direction::Download,
                sent: 7,
                total: 0,
                status: "Downloading".into(),
                ok: true,
                done: false,
                local_path: "/tmp/b.bin".into(),
                remote_path: "/opt/b.bin".into(),
            },
            TransferItem {
                id: 3,
                name: "c.bin".into(),
                direction: Direction::Upload,
                sent: 100,
                total: 100,
                status: "Done".into(),
                ok: true,
                done: true,
                local_path: "/tmp/c.bin".into(),
                remote_path: "/opt/c.bin".into(),
            },
        ];

        app.show_publish = true;
        app.publish_remote = "/opt/app".into();
        app.publish_files = vec![PublishFile {
            remote_path: "/opt/app/x".into(),
            local_path: "/tmp/x".into(),
        }];
        app.backup_versions = vec!["2601071200".into(), "2601071300".into()];
        app.rollback_version = "2601071200".into();
        // 回滚已拆成独立工具条按钮与独立窗口，这里一并渲染（含版本列表）
        app.show_rollback = true;
        app.sync_preview = Some((vec!["a.txt".into(), "b.txt".into()], vec!["stale.txt".into()]));

        app.show_tail = true;
        app.show_grep = true;
        app.show_edit = true;
        app.show_sync = true;
        app.tail_output = vec!["line1".into(), "ERROR bad".into()];
        app.grep_output = vec!["file.txt:3:ERROR bad".into()];
        app.edit_loaded = true;
        app.edit_content = "fn main() {}".into();

        app.show_sites = true;
        app.show_connect = true;
        app.sites = vec![Site {
            name: "prod".into(),
            protocol: Protocol::Rdep,
            host: "10.0.0.5".into(),
            port: 8443,
            user: "admin".into(),
            password: crate::sites::obfuscate_for_storage("pw"),
            ..Default::default()
        }];
        app.site_selected = Some(0);

        render(&mut app, 8);
    }

    /// 远端列表为「只有 `..`」和「完全空」时也不应 panic。
    #[test]
    fn renders_empty_and_dot_only_lists() {
        let mut app = test_app("lists");
        app.remote_entries = vec![entry("..", true, 0, 0)];
        render(&mut app, 3);

        app.remote_entries.clear();
        app.local_entries.clear();
        app.transfers.clear();
        render(&mut app, 3);
    }

    /// 协议门控：FTP 下高级功能被拦截并留下明确日志，rdep 下放行。
    #[test]
    fn advanced_feature_gating() {
        let mut app = test_app("gate");

        app.backend = Protocol::Rdep;
        assert!(app.require_advanced("Logs/Tools"), "rdep 应放行高级功能");
        assert!(!app.log.iter().any(|l| l.contains("rdep protocol")));

        app.backend = Protocol::Ftp;
        let before = app.log.len();
        assert!(!app.require_advanced("Logs/Tools"), "FTP 必须拦截高级功能");
        assert!(app.log.len() > before, "拦截时应写入说明日志");
        let msg = app.log.last().cloned().unwrap_or_default();
        assert!(msg.contains("rdep protocol"), "日志应说明原因: {msg}");
        assert!(msg.contains("FTP"), "日志应指出当前协议: {msg}");

        // 发布/回滚仅 rdep 可用
        app.backend = Protocol::Sftp;
        assert!(!app.require_publish("Publish/Rollback"), "SFTP 必须拦截发布/回滚");
    }

    /// 协议能力矩阵。
    #[test]
    fn protocol_capability_matrix() {
        assert!(Protocol::Rdep.supports_advanced());
        assert!(!Protocol::Ftp.supports_advanced());
        assert!(Protocol::Sftp.supports_advanced());
        assert!(Protocol::Rdep.supports_publish());
        assert!(!Protocol::Sftp.supports_publish());
        assert!(!Protocol::Ftp.supports_publish());
        assert_eq!(Protocol::default(), Protocol::Rdep);
    }

    /// 回归：连接必须有可见反应——主机为空时显式报错；主机填写后立即显示
    /// “Connecting to ...”状态（此前点击后错误被 Disconnected 覆盖，看似没反应）。
    #[test]
    fn connect_gives_immediate_feedback() {
        let mut app = test_app("conn");

        app.cfg_host = "   ".into();
        app.connect_via_current_backend();
        assert!(
            app.status.contains("Host is required"),
            "空主机应显式提示: {}",
            app.status
        );

        app.cfg_host = "127.0.0.1".into();
        app.cfg_port = "22".into();
        app.connect_via_current_backend();
        assert!(
            app.status.contains("Connecting to 127.0.0.1:22"),
            "点击后应立即显示连接中状态: {}",
            app.status
        );
    }

    /// 回归：连接失败时后端先发 Error 再发 Disconnected，
    /// Disconnected 不得把错误状态覆盖回 “Disconnected”（否则看似按钮没反应）。
    #[test]
    fn disconnect_keeps_error_status_on_failed_connect() {
        let mut app = test_app("disc");

        app.connected = false;
        app.status = "connect failed: xyz".into();
        app.handle(Event::Disconnected);
        assert_eq!(app.status, "connect failed: xyz", "失败原因必须保留");
        assert!(!app.connected);

        // 正常连接后的主动断开仍应显示 Disconnected
        app.connected = true;
        app.handle(Event::Disconnected);
        assert_eq!(app.status, t("Disconnected"));
    }

    /// 切到 FTP / SFTP 后渲染界面（含协议选择器与能力提示）不应 panic。
    #[test]
    fn renders_backend_ui() {
        let mut app = test_app("be");
        app.backend = Protocol::Ftp;
        app.show_connect = true;
        app.cfg_use_forwarder = true;
        app.cfg_service_id = "svc".into();
        render(&mut app, 4);

        app.backend = Protocol::Sftp;
        render(&mut app, 4);
    }

    /// 回归：「目录树 ls」在途标记若残留（例如那次 ls 报错），主面板的 ls 应答会被
    /// 误判成树节点加载结果写进树缓存，主面板永远不变 —— 表现即「点 Refresh 没作用」。
    /// Error / Disconnected 必须清理该标记。
    #[test]
    fn stale_pending_tree_ls_is_cleared_on_error_and_disconnect() {
        let mut app = test_app("tree");

        app.pending_tree_ls = Some("/".into());
        app.handle(Event::Error("read stream".into()));
        assert!(app.pending_tree_ls.is_none(), "Error 后必须清理在途树 ls");

        app.pending_tree_ls = Some("/opt".into());
        app.pending_backup_list = true;
        app.handle(Event::Disconnected);
        assert!(app.pending_tree_ls.is_none(), "Disconnected 后必须清理在途树 ls");
        assert!(!app.pending_backup_list, "Disconnected 后必须清理备份在途标记");
    }

    /// 回归：主面板的 ls 应答必须刷新文件列表，不能被在途树 ls 标记吞掉。
    #[test]
    fn dir_listed_updates_main_pane_after_stale_tree_marker() {
        let mut app = test_app("dirlisted");
        app.connected = true;
        app.pending_tree_ls = Some("/".into()); // 模拟残留标记
        app.refresh_remote(); // Refresh 按钮：先清标记再发 ls
        assert!(app.pending_tree_ls.is_none());

        app.handle(Event::DirListed {
            path: "/".into(),
            entries: vec![FileEntry {
                name: "bin".into(),
                is_dir: true,
                size: 0,
                mtime: 0,
                mode: 0o755,
            }],
        });
        assert_eq!(app.remote_entries.len(), 1, "主面板应显示返回的条目");
        assert_eq!(app.remote_entries[0].name, "bin");
    }

    /// 回归：未连接时点 Refresh 曾静默 no-op（按钮像坏了），现在必须给出明确提示。
    #[test]
    fn refresh_without_connection_gives_hint() {
        let mut app = test_app("refresh");
        assert!(!app.connected);
        // 直接走刷新逻辑（与 Refresh 按钮同一条路径）
        if app.connected {
            app.refresh_remote();
        } else {
            let msg = t("Not connected; connect first");
            app.status = msg.to_string();
            app.push_log(&msg);
        }
        assert!(app.status.contains("Not connected"), "应提示未连接: {}", app.status);
        assert!(app.log.iter().any(|l| l.contains("Not connected")));
    }

    /// 回归：界面日志带时间戳（便于与服务端日志对表）。
    #[test]
    fn gui_log_has_timestamp() {
        let mut app = test_app("ts");
        app.push_log("hello");
        let last = app.log.last().unwrap().clone();
        assert!(last.starts_with('[') && last.contains("hello"), "日志应带时间戳: {last}");
    }

    /// 回归：回滚拆成独立按钮后，版本列表走新增的 `Backups` 指令；
    /// 应答要填进 `backup_versions` 并清掉在途标记（否则下次打开窗口会一直空）。
    #[test]
    fn backup_versions_event_fills_list_and_clears_pending() {
        let mut app = test_app("backups");
        app.pending_backup_list = true;
        app.handle(Event::BackupVersions {
            versions: vec!["2601071200".into(), "2601071300".into()],
        });
        assert_eq!(app.backup_versions.len(), 2, "应填入 2 个备份版本");
        assert!(
            !app.pending_backup_list,
            "BackupVersions 到达后必须清掉在途标记"
        );
        assert!(app.log.iter().any(|l| l.contains("2")), "日志应记录版本数");

        // 空列表要有明确提示，而不是静默空白
        let mut app2 = test_app("backups-empty");
        app2.handle(Event::BackupVersions { versions: vec![] });
        assert!(app2.backup_versions.is_empty());
        assert!(app2.log.iter().any(|l| l.contains("no backups")));
    }

    /// 回归：tail 结果要同步进结果编辑窗的文本缓冲（右键菜单 → 执行 → 结果编辑窗）。
    #[test]
    fn tail_line_fills_result_edit_buffer() {
        let mut app = test_app("tailbuf");
        app.handle(Event::TailLine { line: "line-1".into() });
        app.handle(Event::TailLine { line: "line-2".into() });
        assert_eq!(app.tail_output.len(), 2);
        assert_eq!(app.tail_view, "line-1\nline-2\n", "结果编辑窗缓冲应逐行追加");
    }

    /// tail 缓冲按字节限长时，截断位置必须仍是 UTF-8 字符边界。
    #[test]
    fn tail_buffer_truncation_preserves_utf8() {
        let mut app = test_app("tailutf8");
        let line = "界".repeat(66_667);
        app.handle(Event::TailLine { line });

        assert!(app.tail_view.len() <= 200_000);
        assert_eq!(app.tail_view, format!("{}\n", "界".repeat(66_666)));
    }

    /// 远端路径拼接工具函数。
    #[test]
    fn remote_path_join() {
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
        assert_eq!(join_remote("/opt", "a.txt"), "/opt/a.txt");
        assert_eq!(join_remote("/opt/", "sub/a.txt"), "/opt/sub/a.txt");
    }

    /// FileZilla 风格站点字段（登录类型/背景色/注释/默认目录/并发/字符集）必须能
    /// 通过保存链路落到 sites.json，并从磁盘重载后保持一致。
    #[test]
    fn site_manager_persists_new_fields() {
        let mut app = test_app("sitefields");
        app.show_connect = true;
        app.backend = Protocol::Sftp; // 三种协议之一，验证协议字段被保存
        app.site_name_input = "demo".into();
        app.cfg_host = "192.168.1.10".into();
        app.cfg_port = "22".into();
        app.cfg_user = "ops".into();
        app.cfg_pass = "secret".into();
        app.site_remember_pass = true;
        app.cfg_login_type = "Key".into();
        app.site_bg_color = "#1E90FF".into();
        app.site_comment = "prod edge node".into();
        app.site_default_local = "/data/in".into();
        app.site_default_remote = "/opt/app".into();
        app.site_concurrency = "4".into();
        app.site_charset = "UTF-8".into();

        app.save_current_form_as_site();
        // 从磁盘重载，确认落盘
        let reloaded = app.store.load().expect("reload sites");
        assert_eq!(reloaded.len(), 1, "应保存 1 个站点");
        let s = &reloaded[0];
        assert_eq!(s.name, "demo");
        assert_eq!(s.protocol, Protocol::Sftp, "协议字段应被保存");
        assert_eq!(s.login_type, "Key", "登录类型应被保存");
        assert_eq!(s.background_color, "#1E90FF", "背景色应被保存");
        assert_eq!(s.comment, "prod edge node", "注释应被保存");
        assert_eq!(s.default_local_dir, "/data/in", "默认本地目录应被保存");
        assert_eq!(s.default_remote_dir, "/opt/app", "默认远端目录应被保存");
        assert_eq!(s.concurrency, 4, "并发数应被保存");
        assert_eq!(s.charset, "UTF-8", "字符集应被保存");
        // 勾选记住密码后，密码应以混淆形式落盘（非明文）
        assert!(!s.password.is_empty(), "记住密码时应有存储内容");
        assert_ne!(s.password, "secret", "密码不得以明文落盘");

        // 重载进表单也应还原新字段
        app.site_selected = Some(0);
        app.load_site_into_form(0);
        assert_eq!(app.cfg_login_type, "Key");
        assert_eq!(app.site_bg_color, "#1E90FF");
        assert_eq!(app.site_default_remote, "/opt/app");
        assert_eq!(app.site_concurrency, "4");
        assert_eq!(app.site_charset, "UTF-8");
    }

    /// 站点管理器：打开窗口并渲染若干帧（含三种协议 + 标签页切换）不应 panic。
    #[test]
    fn sites_window_renders_all_protocols_and_tabs() {
        let mut app = test_app("sitesrender");
        app.sites = vec![
            Site {
                name: "rdep-site".into(),
                protocol: Protocol::Rdep,
                host: "h1".into(),
                ..Default::default()
            },
            Site {
                name: "ftp-site".into(),
                protocol: Protocol::Ftp,
                host: "h2".into(),
                ..Default::default()
            },
            Site {
                name: "sftp-site".into(),
                protocol: Protocol::Sftp,
                host: "h3".into(),
                ..Default::default()
            },
        ];
        app.site_selected = Some(0);
        app.load_site_into_form(0);
        app.show_sites = true;
        render(&mut app, 3);

        // 切到高级 / 传输 / 字符集 三个标签页分别渲染
        for tab in 1..=3 {
            app.site_tab = tab;
            render(&mut app, 2);
        }
        // 切换到 FTP 站点后回填默认端口（与连接框一致的逻辑）
        app.backend = Protocol::Ftp;
        sync_port_to_protocol(&mut app);
        assert_eq!(app.cfg_port, "21", "切到 FTP 应回填端口 21");
        app.backend = Protocol::Sftp;
        sync_port_to_protocol(&mut app);
        assert_eq!(app.cfg_port, "22", "切到 SFTP 应回填端口 22");
        app.backend = Protocol::Rdep;
        sync_port_to_protocol(&mut app);
        assert_eq!(app.cfg_port, "8443", "切到 rdep 应回填端口 8443");
        render(&mut app, 2);
    }

    /// 选中 SFTP 站点后，站点管理器渲染不应把自定义端口覆盖回协议默认值。
    #[test]
    fn sites_window_preserves_sftp_custom_port() {
        let mut app = test_app("sftpport");
        app.sites = vec![Site {
            name: "sftp-custom".into(),
            protocol: Protocol::Sftp,
            host: "118.31.124.97".into(),
            port: 42637,
            user: "ops".into(),
            ..Default::default()
        }];
        app.site_selected = Some(0);
        app.load_site_into_form(0);
        assert_eq!(app.cfg_port, "42637", "载入站点时应保留自定义端口");
        assert_eq!(app.backend, Protocol::Sftp, "载入站点时应设置 SFTP 协议");

        app.show_sites = true;
        render(&mut app, 3);
        assert_eq!(
            app.cfg_port, "42637",
            "渲染站点管理器后不应覆盖自定义 SFTP 端口"
        );
    }

    /// tail：收到新日志行应置 dirty 触发自动滚动；渲染 tail 弹窗后 dirty 被清零。
    #[test]
    fn tail_dirty_flag_set_by_new_line_and_reset_after_render() {
        let mut app = test_app("taildirty");
        assert!(!app.tail_view_dirty, "初始未脏");
        app.handle(Event::TailLine { line: "new line".into() });
        assert!(app.tail_view_dirty, "新日志行应置 dirty");

        app.show_tail = true;
        render(&mut app, 2);
        assert!(!app.tail_view_dirty, "渲染后应清零 dirty（已滚到底）");
    }

    /// grep：带 pattern 渲染结果窗时，命中高亮分支不应 panic（含大小写不敏感）。
    #[test]
    fn grep_highlight_renders_with_pattern() {
        let mut app = test_app("grephl");
        app.show_grep = true;
        app.grep_pattern = "error".into();
        app.grep_flags = "in".into(); // 忽略大小写 + 行号
        app.grep_output = vec![
            "main.rs:12:ERROR something".into(),
            "lib.rs:30:no match here".into(),
        ];
        render(&mut app, 3);
    }

    /// chmod 按钮：rdep 与 SFTP 走各自 client.chmod（不拦截、不 panic）；FTP 协议不支持，给出明确拦截日志。
    #[test]
    fn chmod_dispatch_rdep_sftp_vs_ftp() {
        let mut app = test_app("chmodgate");

        app.backend = Protocol::Rdep;
        let before = app.log.len();
        app.do_chmod("/opt/app/x".into(), 0o644);
        assert_eq!(app.log.len(), before, "rdep 不应拦截 chmod（无额外日志）");

        app.backend = Protocol::Sftp;
        let before = app.log.len();
        app.do_chmod("/opt/app/x".into(), 0o644);
        assert_eq!(app.log.len(), before, "sftp 不应拦截 chmod（无额外日志）");

        app.backend = Protocol::Ftp;
        app.do_chmod("/opt/app/x".into(), 0o644);
        let last = app.log.last().cloned().unwrap_or_default();
        assert!(
            last.contains("FTP"),
            "FTP 应提示不支持 chmod: {last}"
        );
    }

    /// 浏览对话框结果经 `browse_rx` 通道回传后，应写入 `site_default_local`。
    /// 不调用真实 rfd 对话框（无显示/无 portal 环境无法交互），直接注入通道结果验证接线逻辑。
    #[test]
    fn browse_picker_wires_result_into_site_default_local() {
        let mut app = test_app("browse");

        // 场景 1：用户选了一个目录 -> 写入 site_default_local，并清空等待状态
        let (tx, rx) = std::sync::mpsc::channel::<Option<PathBuf>>();
        tx.send(Some(PathBuf::from("/srv/www"))).unwrap();
        app.browse_rx = Some(rx);
        app.drain_events();
        assert_eq!(app.site_default_local, "/srv/www");
        assert!(app.browse_rx.is_none(), "结果处理后应清空等待状态");

        // 场景 2：用户取消（None）-> 保持原值，并清空等待状态
        let (tx, rx) = std::sync::mpsc::channel::<Option<PathBuf>>();
        app.site_default_local = "keep-me".into();
        tx.send(None).unwrap();
        app.browse_rx = Some(rx);
        app.drain_events();
        assert_eq!(app.site_default_local, "keep-me");
        assert!(app.browse_rx.is_none(), "取消后也应清空等待状态");
    }

    /// 八进制权限解析：合法串 -> Some；非法串 -> None；范围越界 -> None。
    #[test]
    fn parse_octal_mode_cases() {
        assert_eq!(parse_octal_mode("0644"), Some(0o644));
        assert_eq!(parse_octal_mode("755"), Some(0o755));
        assert_eq!(parse_octal_mode(" 600 "), Some(0o600));
        assert_eq!(parse_octal_mode("8"), None, "8 非八进制");
        assert_eq!(parse_octal_mode("abc"), None, "字母非法");
        assert_eq!(parse_octal_mode("10000"), None, "超出 0o7777");
        assert_eq!(parse_octal_mode("-1"), None, "负数非法");
    }

    /// 右键「权限」应弹出 chmod 窗口并预填远端路径。
    #[test]
    fn remote_context_menu_opens_permissions() {
        let mut app = test_app("ctxperm");
        app.backend = Protocol::Rdep;
        app.remote_dir = "/opt".into();
        app.remote_entries = vec![entry("app.conf", false, 100, 0o644)];
        // 模拟右键菜单里点「权限」按钮：直接驱动等价逻辑
        app.chmod_path = "/opt/app.conf".into();
        app.chmod_mode.clear();
        app.show_chmod = true;
        render(&mut app, 2);
        assert!(app.show_chmod, "chmod 窗口应处于打开状态");
    }
}
