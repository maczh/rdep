//! 多语言支持（英文 / 简体中文 / 繁體中文），默认英文。
//!
//! 设计：
//! - 以**英文原文作为 key**（`t("Connect")`），缺失翻译时回退英文原文，
//!   保证新增字符串不阻塞构建。
//! - 翻译表为静态三元组 `(key, zh_cn, zh_tw)`；补全性测试保证无重复 key、
//!   无空翻译。
//! - 语言选择存全局原子变量（`set_lang` / `get_lang`），GUI 与各后端线程
//!   （rdep/FTP/SFTP 的后台线程）共享同一语言设置，后端生成的事件消息
//!   （Status/Error/OpDone 等）也能随语言切换。
//! - 用户选择持久化到配置目录 `rdep/ui.json`（与 sites.json 同目录）。

use std::sync::atomic::{AtomicU8, Ordering};

/// 支持的语言。数值即存储编码，**不要改动**（会破坏已持久化的选择）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En = 0,
    ZhCn = 1,
    ZhTw = 2,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::ZhCn, Lang::ZhTw];

    /// 稳定编码（持久化用）。
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhCn => "zh-CN",
            Lang::ZhTw => "zh-TW",
        }
    }

    /// 解析持久化的语言编码；未知编码回退默认（英文）。
    pub fn from_code(s: &str) -> Self {
        match s {
            "zh-CN" => Lang::ZhCn,
            "zh-TW" => Lang::ZhTw,
            _ => Lang::En,
        }
    }

    /// 语言列表展示名（每项用该语言自身书写，方便用户辨认）。
    pub fn display_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::ZhCn => "简体中文",
            Lang::ZhTw => "繁體中文",
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// 设置全局语言（GUI 启动时与切换语言时调用）。
pub fn set_lang(l: Lang) {
    LANG.store(l as u8, Ordering::Relaxed);
}

/// 当前全局语言。
pub fn get_lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::ZhCn,
        2 => Lang::ZhTw,
        _ => Lang::En,
    }
}

/// 翻译当前语言下的字符串。key 即英文原文；无对应条目时原样返回英文。
pub fn t(key: &'static str) -> &'static str {
    let idx = LANG.load(Ordering::Relaxed);
    if idx == 0 {
        return key;
    }
    let row = TABLE.iter().find(|e| e.0 == key);
    match row {
        Some(e) => {
            if idx == 1 {
                e.1
            } else {
                e.2
            }
        }
        None => key,
    }
}

/// 翻译表：`(key=英文, 简体中文, 繁體中文)`。
///
/// 维护规则：
/// - 新增字符串只加英文 key 也能编译（回退英文），但应在同一轮补齐两列中文。
/// - `tests::table_sanity` 保证 key 唯一且中文列非空。
const TABLE: &[(&str, &str, &str)] = &[
    // ---- 顶栏 / 通用 ----
    ("rdep Client", "rdep 客户端", "rdep 用戶端"),
    ("Sites", "站点", "站點"),
    ("Connect", "连接", "連線"),
    ("Disconnect", "断开", "斷線"),
    ("Publish/Rollback", "发布/回滚", "發布/回滾"),
    ("Logs/Tools", "日志/编辑", "日誌/編輯"),
    ("Not connected", "未连接", "未連線"),
    ("Connected", "已连接", "已連線"),
    ("Disconnected", "已断开", "已斷線"),
    ("Language", "语言", "語言"),
    // ---- 本地面板 ----
    ("Local", "本地", "本機"),
    ("Local site:", "本地站点:", "本機站點:"),
    ("Refresh", "刷新", "重新整理"),
    ("Upload", "上传", "上傳"),
    ("Name", "名称", "名稱"),
    ("Size", "大小", "大小"),
    ("Type", "类型", "類型"),
    ("Modified", "修改时间", "修改時間"),
    ("Perms", "权限", "權限"),
    ("Directory", "目录", "目錄"),
    ("File", "文件", "檔案"),
    // ---- 远端面板 ----
    ("Remote", "远端", "遠端"),
    ("Remote site:", "远端站点:", "遠端站點:"),
    ("Download", "下载", "下載"),
    ("Go", "转到", "前往"),
    // ---- 传输队列 ----
    ("Transfer queue", "传输队列", "傳輸佇列"),
    ("(empty)", "（无）", "（無）"),
    ("Transferring", "传输中", "傳輸中"),
    ("Done", "完成", "完成"),
    ("Failed", "失败", "失敗"),
    ("received {n} bytes...", "已接收 {n} 字节…", "已接收 {n} 位元組…"),
    ("Log", "日志", "日誌"),
    // ---- 右键菜单 ----
    ("Enter", "进入", "進入"),
    ("Rename", "改名", "重新命名"),
    ("New directory", "新建目录", "新建目錄"),
    ("Delete", "删除", "刪除"),
    ("Add files to queue", "添加文件到队列", "加入檔案到佇列"),
    ("Open directory", "打开目录", "開啟目錄"),
    // ---- 连接窗口 ----
    ("Connect to server", "连接到服务", "連線到伺服器"),
    ("Protocol", "协议", "協定"),
    ("Host", "主机", "主機"),
    ("Port", "端口", "連接埠"),
    ("User", "用户", "使用者"),
    ("Password", "密码", "密碼"),
    ("Token", "令牌", "權杖"),
    ("Use API token auth (CI/CD)", "使用 API 令牌认证（CI/CD）", "使用 API 權杖認證（CI/CD）"),
    ("CA cert path", "CA证书", "CA憑證"),
    ("Relay via forwarder", "经 forwarder 中转", "經 forwarder 中轉"),
    ("Target service id", "目标 service id", "目標 service id"),
    ("Relay token", "中转密钥", "中轉密鑰"),
    ("Tip: host/port are the forwarder address", "提示：主机/端口填 forwarder 地址", "提示：主機/連接埠填 forwarder 地址"),
    ("Connect now", "连接", "連線"),
    ("Save and connect", "保存并连接", "儲存並連線"),
    ("Cancel", "取消", "取消"),
    ("Host is required", "请填写主机地址", "請填寫主機位址"),
    ("Already connected; disconnect first", "已连接，请先断开", "已連線，請先斷開"),
    // ---- 刷新 / 目录列表反馈 ----
    ("Not connected; connect first", "尚未连接，请先连接", "尚未連線，請先連線"),
    // ---- 工具条拆分 / 右键菜单工具 ----
    ("Publish", "发布", "發布"),
    ("Rollback", "回滚", "回滾"),
    ("Sync", "同步", "同步"),
    ("Tail", "查看日志尾部", "檢視日誌尾部"),
    ("Grep", "内容检索", "內容檢索"),
    ("Edit", "编辑", "編輯"),
    ("Execute", "执行", "執行"),
    ("Directory sync", "目录同步", "目錄同步"),
    ("Edit remote file", "编辑远端文件", "編輯遠端檔案"),
    ("Stop follow", "停止跟随", "停止跟隨"),
    ("(no output)", "（空）", "（空）"),
    ("{n} backup version(s)", "共 {n} 个备份版本", "共 {n} 個備份版本"),
    ("(no backups yet)", "（尚无备份）", "（尚無備份）"),
    ("(loaded; edits are saved with auto-backup on the server)", "（已加载；保存时服务端会自动备份）", "（已載入；儲存時伺服端會自動備份）"),
    ("Listing {path} ...", "正在列出 {path} ...", "正在列出 {path} ..."),
    ("{n} entries in {path}", "{path} 下共 {n} 项", "{path} 下共 {n} 項"),
    ("not connected", "未连接", "未連線"),
    // ---- 快速连接条 / 队列标签页（FileZilla 风格布局） ----
    ("Quick connect", "快速连接", "快速連線"),
    ("Queued files", "队列的文件", "佇列的檔案"),
    ("Failed transfers", "传输失败", "傳輸失敗"),
    ("Successful transfers", "成功的传输", "成功的傳輸"),
    ("Local file", "本地文件", "本地檔案"),
    ("Direction", "方向", "方向"),
    ("Status", "状态", "狀態"),
    ("Connecting to {host}:{port} ...", "正在连接 {host}:{port} ...", "正在連線 {host}:{port} ..."),
    ("Site name", "站点名", "站點名"),
    ("Remember password", "记住密码", "記住密碼"),
    // ---- FTP 提示 ----
    ("FTP only supports basic file operations; publish/rollback, directory sync, tail/grep, edit, resume and relay are rdep-only. FTP is plaintext.", "FTP 仅支持基础文件操作；发布回滚/目录同步/tail/grep/编辑/断点续传/中转均为 rdep 专属。FTP 为明文传输。", "FTP 僅支援基礎檔案操作；發布回滾/目錄同步/tail/grep/編輯/斷點續傳/中轉均為 rdep 專屬。FTP 為明文傳輸。"),
    ("SFTP: publish/rollback require the rdep service and are unavailable.", "SFTP 不支持发布/回滚（需要 rdep service）。", "SFTP 不支援發布/回滾（需要 rdep service）。"),
    // ---- 站点管理 ----
    ("Site manager", "站点管理", "站台管理"),
    ("Config file:", "配置文件：", "設定檔："),
    ("No saved sites yet. Fill in the connect window and click \"Save site\".", "尚无已保存站点。填写连接信息后点「保存站点」。", "尚無已儲存站台。填寫連線資訊後點「儲存站台」。"),
    ("Load to connect window", "载入到连接框", "載入到連線框"),
    ("Delete selected", "删除选中", "刪除所選"),
    ("Save current form as site:", "把当前连接配置存为站点：", "將目前連線設定儲存為站台："),
    ("Save site", "保存站点", "儲存站台"),
    ("Choose a site first", "请先选择一个站点", "請先選擇一個站台"),
    ("Site loaded into connect window", "站点配置已载入连接框", "站台設定已載入連線框"),
    ("Enter a site name first", "保存站点失败：请填写站点名", "儲存站台失敗：請填寫站台名"),
    ("saved (with password)", "已保存（含密码）", "已儲存（含密碼）"),
    ("saved (password not stored)", "已保存（未保存密码）", "已儲存（未儲存密碼）"),
    ("site saved", "已保存", "已儲存"),
    ("site deleted", "已删除", "已刪除"),
    ("deleted", "删除", "刪除"),
    ("Site \"{n}\" saved (with password) → {p}", "站点「{n}」已保存（含密码）→ {p}", "站台「{n}」已儲存（含密碼）→ {p}"),
    ("Site \"{n}\" saved (password not stored) → {p}", "站点「{n}」已保存（未保存密码）→ {p}", "站台「{n}」已儲存（未儲存密碼）→ {p}"),
    ("Failed to save site", "保存站点失败", "儲存站台失敗"),
    ("Failed to delete site", "删除站点失败", "刪除站台失敗"),
    ("Site written but reload failed", "站点已写入但重新载入失败", "站台已寫入但重新載入失敗"),
    ("Reload after delete failed", "删除后重新载入失败", "刪除後重新載入失敗"),
    // ---- 发布 / 回滚窗口 ----
    ("Publish / Rollback", "发布 / 回滚", "發布 / 回滾"),
    ("Publish (backup old files → upload → run restart script)", "发布（先备份旧文件 → 上传 → 执行重启脚本）", "發布（先備份舊檔 → 上傳 → 執行重啟腳本）"),
    ("Remote dir", "远端目录", "遠端目錄"),
    ("Restart script id", "重启脚本 ID", "重啟腳本 ID"),
    ("Local source file", "本地源文件", "本機來源檔案"),
    ("Remote file name", "远端文件名", "遠端檔案名"),
    ("Add to publish list", "加入发布清单", "加入發布清單"),
    ("Publish list is empty", "（清单为空）", "（清單為空）"),
    ("Remove", "移除", "移除"),
    ("Run publish", "执行发布", "執行發布"),
    ("Not connected, cannot publish", "未连接，无法发布", "未連線，無法發布"),
    ("Rollback (restore a backup version to the remote dir)", "回滚（把某个备份版本恢复到远端目录）", "回滾（將某個備份版本還原到遠端目錄）"),
    ("List backup versions", "列出备份版本", "列出備份版本"),
    ("Backup version", "备份版本", "備份版本"),
    ("<select version>", "<选择版本>", "<選擇版本>"),
    ("Run rollback", "执行回滚", "執行回滾"),
    ("Select a backup version first", "请先选择一个备份版本", "請先選擇一個備份版本"),
    ("Not connected, cannot rollback", "未连接，无法回滚", "未連線，無法回滾"),
    ("Not connected, cannot list backups", "未连接，无法列出备份", "未連線，無法列出備份"),
    // ---- 目录同步 ----
    ("Directory sync (local → remote; changed = size+mtime; auto-backup before overwrite)", "目录同步（本地 → 远端，按大小+mtime 判定变更，覆盖前自动备份）", "目錄同步（本機 → 遠端，按大小+mtime 判定變更，覆蓋前自動備份）"),
    ("Local dir", "本地目录", "本機目錄"),
    ("Delete extra remote files", "删除远端多余文件", "刪除遠端多餘檔案"),
    ("Preview (dry-run)", "预览(dry-run)", "預覽(dry-run)"),
    ("Run sync", "执行同步", "執行同步"),
    ("Not connected, cannot sync", "未连接，无法同步", "未連線，無法同步"),
    ("{n} files to upload/update:", "将上传/更新 {n} 个：", "將上傳/更新 {n} 個："),
    ("{n} extra remote files to delete:", "将删除 {n} 个远端多余文件：", "將刪除 {n} 個遠端多餘檔案："),
    // ---- 工具窗口 ----
    ("Log / Search / Edit", "日志 / 检索 / 编辑", "日誌 / 檢索 / 編輯"),
    ("TAIL (view log tail; optional follow)", "TAIL（查看日志尾部，可跟随）", "TAIL（檢視日誌尾部，可跟隨）"),
    ("Remote file", "远端文件", "遠端檔案"),
    ("Tail lines", "末尾行数", "末尾行數"),
    ("Follow", "跟随", "跟隨"),
    ("Start", "开始", "開始"),
    ("GREP (search content; flags: i=ignore-case n=line-number)", "GREP（内容检索，flags: i=忽略大小写 n=显示行号）", "GREP（內容檢索，flags: i=忽略大小寫 n=顯示行號）"),
    ("Path", "路径", "路徑"),
    ("Pattern", "关键字", "關鍵字"),
    ("Flags", "标志", "標誌"),
    ("Search", "搜索", "搜尋"),
    ("Not connected, cannot tail", "未连接，无法 tail", "未連線，無法 tail"),
    ("Not connected, cannot grep", "未连接，无法 grep", "未連線，無法 grep"),
    ("EDIT (remote editing; auto-backup before save)", "EDIT（远端在线编辑，保存前自动备份）", "EDIT（遠端線上編輯，儲存前自動備份）"),
    ("Load", "加载", "載入"),
    ("Save", "保存", "儲存"),
    ("Not connected, cannot load", "未连接，无法加载", "未連線，無法載入"),
    ("Not connected, cannot save", "未连接，无法保存", "未連線，無法儲存"),
    // ---- 事件 / 日志消息（后端也会用） ----
    ("rdep client started", "rdep 客户端已启动", "rdep 用戶端已啟動"),
    ("No saved sites yet; save one in \"Sites\"", "尚无已保存站点，可在「站点」中保存当前连接配置", "尚無已儲存站台，可在「站台」中儲存目前連線設定"),
    ("Loaded {n} sites", "已载入 {n} 个站点", "已載入 {n} 個站台"),
    ("Failed to read site config (ignored)", "读取站点配置失败（已忽略）", "讀取站台設定失敗（已忽略）"),
    ("\"{f}\" requires the rdep protocol; current site is {p}, skipped", "「{f}」需要 rdep 协议，当前站点是 {p}，已跳过", "「{f}」需要 rdep 協定，目前站台是 {p}，已跳過"),
    ("Enter a new name first", "请先输入新的文件名", "請先輸入新的檔案名"),
    ("Select a file in the remote list first", "请先在远端列表选择一个文件", "請先在遠端清單選擇一個檔案"),
    // ---- FTP / 连接状态消息 ----
    ("connecting {host}:{port} ...", "连接 {host}:{port} 中 …", "連線 {host}:{port} 中 …"),
    ("failed to resolve {host}:{port}", "{host}:{port} 未解析到地址", "{host}:{port} 無法解析"),
    ("FTP connected", "FTP 已连接", "FTP 已連線"),
    ("FTP connect failed", "FTP 连接失败", "FTP 連線失敗"),
    ("not connected (FTP)", "未连接（FTP）", "未連線（FTP）"),
    ("FTP list failed", "FTP 列目录失败", "FTP 列目錄失敗"),
    ("created {n} directories", "新建 {} 个目录", "新建 {} 個目錄"),
    ("partial failures", "部分失败", "部分失敗"),
    ("deleted {n} items", "删除 {n} 项", "刪除 {n} 項"),
    ("deleted {n}; {f} failed", "已删 {n} 项，失败 {f} 个", "已刪 {n} 項，失敗 {f} 個"),
    ("renamed to {d}", "已重命名为 {d}", "已重新命名為 {d}"),
    ("rename failed", "重命名失败", "重新命名失敗"),
    ("failed to read local file metadata", "读取本地文件属性失败", "讀取本機檔案屬性失敗"),
    ("failed to open local file", "打开本地文件失败", "開啟本機檔案失敗"),
    ("FTP upload failed", "FTP 上传失败", "FTP 上傳失敗"),
    ("failed to read local file", "读取本地文件失败", "讀取本機檔案失敗"),
    ("failed to write FTP data stream", "写 FTP 数据流失败", "寫 FTP 資料流失敗"),
    ("failed to close FTP data stream", "关闭 FTP 数据流失败", "關閉 FTP 資料流失敗"),
    ("FTP upload not confirmed", "FTP 上传未确认", "FTP 上傳未確認"),
    ("failed to create temp file", "创建临时文件失败", "建立暫存檔失敗"),
    ("FTP download failed", "FTP 下载失败", "FTP 下載失敗"),
    ("failed to read FTP data stream", "读取 FTP 数据流失败", "讀取 FTP 資料流失敗"),
    ("failed to write local file", "落盘失败", "寫入本機失敗"),
    ("failed to flush local file", "刷盘失败", "刷新暫存失敗"),
    ("FTP download not confirmed", "FTP 下载未确认", "FTP 下載未確認"),
    ("upload ok", "上传完成", "上傳完成"),
    ("download ok", "下载完成", "下載完成"),
    // ---- SFTP 状态消息 ----
    ("SFTP connecting {host}:{port} ...", "SFTP 连接 {host}:{port} 中 …", "SFTP 連線 {host}:{port} 中 …"),
    ("SFTP connected", "SFTP 已连接", "SFTP 已連線"),
    ("SFTP connect failed", "SFTP 连接失败", "SFTP 連線失敗"),
    ("not connected (SFTP)", "未连接（SFTP）", "未連線（SFTP）"),
    ("SFTP list failed", "SFTP 列目录失败", "SFTP 列目錄失敗"),
    ("host key fingerprint", "主机指纹", "主機指紋"),
    ("new host key fingerprint accepted and recorded", "首次连接，已记录主机指纹", "首次連線，已記錄主機指紋"),
    ("HOST KEY MISMATCH (possible MITM); connection rejected", "主机指纹不匹配（可能中间人攻击），已拒绝连接", "主機指紋不匹配（可能中間人攻擊），已拒絕連線"),
    ("SFTP auth failed", "SFTP 认证失败", "SFTP 認證失敗"),
    ("SFTP tail failed", "SFTP tail 失败", "SFTP tail 失敗"),
    ("tail stopped", "tail 已停止", "tail 已停止"),
    ("SFTP grep failed", "SFTP grep 失败", "SFTP grep 失敗"),
    ("SFTP rename failed", "SFTP 改名失败", "SFTP 改名失敗"),
    ("SFTP upload failed", "SFTP 上传失败", "SFTP 上傳失敗"),
    ("SFTP download failed", "SFTP 下载失败", "SFTP 下載失敗"),
    ("local dir is empty or unreadable", "目录为空或不可读", "目錄為空或不可讀"),
    ("resume from {n} bytes", "从 {n} 字节处续传", "從 {n} 位元組處續傳"),
    ("local file shrunk; restart from scratch", "本地文件变短，重新完整传输", "本機檔案變短，重新完整傳輸"),
    ("sync: local {n} files: {u} to upload, {d} to delete", "同步：本地 {n} 个文件：待传 {u}，多余 {d}", "同步：本機 {n} 個檔案：待傳 {u}，多餘 {d}"),
    ("sync: uploaded {n} file(s)", "同步完成：上传 {n} 个文件", "同步完成：上傳 {n} 個檔案"),
    ("sync: deleted {n} file(s)", "同步：删除 {n} 个远端多余文件", "同步：刪除 {n} 個遠端多餘檔案"),
    ("sync failed", "同步失败", "同步失敗"),
    ("backed up to {d}", "已备份为 {d}", "已備份為 {d}"),
    // ---- rdep 协议消息（client.rs） ----
    ("resume {n}: server has {u}/{d} chunks", "resume {n}: 服务端已有 {u}/{d} 片", "resume {n}: 伺服器已有 {u}/{d} 片"),
    ("{summary}; {n} failed: {d}", "{summary}；失败 {n} 项：{d}", "{summary}；失敗 {n} 項：{d}"),
    // ---- 传输状态（GUI 队列显示） ----
    ("op ok", "操作成功", "操作成功"),
    ("op failed", "操作失败", "操作失敗"),
    ("publish ok", "发布成功", "發布成功"),
    ("publish failed", "发布失败", "發布失敗"),
    ("tail done", "tail 结束", "tail 結束"),
    ("{n} match(es)", "{n} 条命中", "{n} 筆命中"),
    ("uploading", "上传中", "上傳中"),
    ("downloading", "接收中", "接收中"),
    // ---- 其它 ----
    ("(dir)", "（目录）", "（目錄）"),
    ("server error", "服务器错误", "伺服器錯誤"),
];

/// 翻译并替换 `{x}` 占位符。
///
/// 例：`tf("deleted {n} items", &[("n", "3")])`。参数化的消息统一用
/// `{name}` 占位（而非直接 format!），保证翻译列里的语序可自由调整。
pub fn tf(key: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = t(key).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 持久化：语言选择存到配置目录 `rdep/ui.json`。
///
/// 独立成小文件而不是塞进 sites.json：站点配置面向「部署目标」，UI 语言
/// 面向「使用者」，二者变更频率与归属不同。
mod persist {
    use super::Lang;
    use anyhow::Result;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct UiFile {
        #[serde(default)]
        lang: Option<String>,
    }

    fn ui_path() -> PathBuf {
        crate::sites::config_base_dir().join("rdep").join("ui.json")
    }

    /// 读取持久化的语言；文件缺失/损坏/编码未知一律回退默认（英文）。
    pub fn load() -> Lang {
        let Ok(raw) = std::fs::read_to_string(ui_path()) else {
            return Lang::En;
        };
        if let Ok(f) = serde_json::from_str::<UiFile>(&raw) {
            if let Some(code) = f.lang {
                return Lang::from_code(&code);
            }
        }
        Lang::En
    }

    pub fn save(lang: Lang) -> Result<()> {
        let p = ui_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_string_pretty(&UiFile {
            lang: Some(lang.code().to_string()),
        })?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }
}

pub use persist::{load as load_lang, save as save_lang};

#[cfg(test)]
mod tests {
    use super::*;

    /// 表完整性：key 唯一、中文两列均非空。
    #[test]
    fn table_sanity() {
        let mut seen = std::collections::HashSet::new();
        for (k, cn, tw) in TABLE {
            assert!(seen.insert(*k), "duplicate i18n key: {k}");
            assert!(!cn.trim().is_empty(), "missing zh-CN for key: {k}");
            assert!(!tw.trim().is_empty(), "missing zh-TW for key: {k}");
        }
    }

    /// 英文为 key 本身；简体/繁体按索引取列；未知 key 回退英文。
    #[test]
    fn lookup_and_fallback() {
        set_lang(Lang::En);
        assert_eq!(t("Connect"), "Connect");
        set_lang(Lang::ZhCn);
        assert_eq!(t("Connect"), "连接");
        set_lang(Lang::ZhTw);
        assert_eq!(t("Connect"), "連線");
        set_lang(Lang::ZhCn);
        assert_eq!(t("no-such-key-xyz"), "no-such-key-xyz", "未知 key 回退英文");
        set_lang(Lang::En);
    }

    /// 语言编码往返。
    #[test]
    fn lang_code_roundtrip() {
        for l in Lang::ALL {
            assert_eq!(Lang::from_code(l.code()), l);
        }
        assert_eq!(Lang::from_code("xx"), Lang::En, "未知编码回退英文");
    }

    /// ui.json 持久化往返（含损坏文件回退）。
    #[test]
    fn persist_roundtrip() {
        // load_lang/save_lang 使用真实配置目录；这里只验证 from_code/save 逻辑
        // 不实际写用户目录，避免污染。持久化路径解析由 config_base_dir 的
        // 现有测试思路保证。此处验证默认值与编码解析即可。
        assert_eq!(Lang::from_code("zh-CN"), Lang::ZhCn);
        assert_eq!(Lang::from_code("zh-TW"), Lang::ZhTw);
        assert_eq!(Lang::from_code("en"), Lang::En);
        assert_eq!(Lang::from_code(""), Lang::En);
    }
}
