# Phase 8 — client 体验增强（Overview）

> 承接 Phase 2（站点管理）与 Phase 4（TAIL/GREP/EDIT），补齐 FileZilla 风格易用性与「属性保留」细节。
> 全部需求来自客户端改造清单：站点管理 UI 重构、tail/grep/edit 体验、上传下载属性保留、远程 chmod。

## 交付清单

| # | 需求 | 关键位置 | 状态 |
|---|------|----------|------|
| 1 | FileZilla 风格站点管理器（sftp/ftp/rdep，字段持久化） | `rdep-client/src/app.rs` `sites_window` + `site_tab_*` | ✅ |
| 2 | tail 自动滚到底 + 跟随新日志 + 关窗自动 stop follow | `tail_dialog` / `Event::TailLine` → `tail_view_dirty` | ✅ |
| 3 | grep 命中内容标红 | `render_grep_match`（`LayoutJob` 红色逐段） | ✅ |
| 4 | 编辑保存先备份原文件再覆盖原文件名 | `rdep-service/src/storage.rs` `save_with_backup` | ✅ |
| 5 | 上传/下载保留 mode + mtime | service `set_mtime`/`apply_mode`；`DownloadResponse{mode,mtime,sha256}`；client `apply_local_*` | ✅ |
| 6 | 右键「权限」→ 远程 chmod（rdep + SFTP） | rdep: `CmdType::Chmod`/`storage::chmod`/`Client::chmod`；SFTP: `SftpSession::set_metadata`/`chmod_attrs`；UI: `chmod_dialog` | ✅ |
| 7 | 站点「浏览…」按钮接入 rfd 系统文件框（xdg-portal，纯 Rust 无 GTK 依赖） | `open_local_dir_picker`（独立线程）+ `browse_rx` 通道 + `drain_events` 轮询回写 `site_default_local` | ✅ |

## 关键设计点

### 站点管理器（需求 1）
- 左栏「My Sites」可折叠树（`CollapsingHeader::default_open(true)`）+ 新建/删除；右栏四页签：
  - **General**：协议（sftp/ftp/rdep）、登录类型（Normal/Key file/Ask）、用户名/密码（Ask 时不显示）、CA 证书、rdep 中转块、背景色（含 `#RRGGBB` 预览色块）、备注。
  - **Advanced**：默认本地目录（「浏览…」经 rfd 调起系统文件选择框，选完回填路径；「用当前本地目录」回填当前 `local_dir`）、默认远程目录。
  - **Transfer Settings**：并发数 `DragValue` 1–16。
  - **Charset**：Auto / Force UTF-8。
- 切协议自动同步默认端口：`default_port_for_protocol` / `sync_port_to_protocol`（22/21/8443）。
- 所有新增字段 `#[serde(default)]`，持久化到 `sites.json`；改名保存时 `save_current_form_as_site`
  先删旧条目避免重名残留。

### tail（需求 2）
- 日志框 `TextEdit::interactive(false)` 只读，避免内层吞掉滚轮；
  `ui.scroll_to_cursor(Some(Align::BOTTOM))` 在 `tail_follow || tail_view_dirty` 时触发。
- `drain_events` 收到 `TailLine` 后置 `tail_view_dirty=true`，渲染后复位。
- 窗口关闭 `!open && tail_follow` → `do_stop_tail()` + `tail_follow=false` + 记「tail stopped」。

### grep 标红（需求 3）
- `render_grep_match(line, pattern, ignore_case)`：按命中子串切分 `LayoutJob`，命中段
  `TextFormat::simple(monospace, Color32::RED)`；非 ASCII 大小写折叠长度不一致时降级为纯文本，
  避免 panic。

### 编辑备份（需求 4）
- `save_with_backup(remote_path, data)`：目标存在时先 `copy` 到 `meta/backup/<版本戳>/<相对路径>`
  （`create_dir_all` 父级 + `prune_backups` 保留上限），再 `save` 覆盖原文件名。

### 属性保留（需求 5）
- 上传：service 端 `set_mtime`（`std::fs::set_times`，因 std 无 `set_modified`）与 `apply_mode` 还原权限/时间。
- 下载：`DownloadResponse` 携带 `mode:u32, mtime:i64, sha256:[u8;32]`，client 用
  `apply_local_mode` / `apply_local_mtime` 在本地还原。

### 远程 chmod（需求 6）
- 协议：`CmdType::Chmod`(=18) + `ChmodRequest{path:String, mode:u32}`。
- service：`session.rs` 解码后调 `storage::chmod`（mode=0 拒绝）。
- client：右键菜单「Permissions」打开 `chmod_dialog`，八进制 0–0o7777 校验（`parse_octal_mode`）
  后 `Client::chmod` → `do_simple(Chmod, ...)`。
- **rdep + SFTP 均支持**：
  - rdep：走自定义协议（service 端 `storage::chmod`）。
  - SFTP：经 SFTP `setstat`——高版 `SftpSession` 虽未直接暴露 `setstat`，但提供了等价的
    `set_metadata(path, FileAttributes{permissions:Some(mode),..})`（底层即 `SSH_FXP_SETSTAT`）。
    `chmod_attrs(mode)` 只设 `permissions`、其余字段留空，避免 `setstat` 误清空 size/mtime。
  - **FTP 仍不支持**（协议本身无标准 chmod，且本客户端未实现 `SITE CHMOD`）；点击时日志明确提示
    「chmod not supported over FTP...」。

## 测试

- `cargo test -p rdep-protocol -p rdep-service -p rdep-client`
  - lib：rdep-client 51 + rdep-protocol 5 + rdep-service 22 = **78 passed**
  - integration：**18 passed**（含 `tail_grep_edit_e2e`、`mode_preservation_e2e`、`site_saved_drives_connection_e2e`）
- 新增/增强单测：
  - `app::gui_smoke`：`site_manager_persists_new_fields`、`sites_window_renders_all_protocols_and_tabs`、
    `tail_dirty_flag_set_by_new_line_and_reset_after_render`、`grep_highlight_renders_with_pattern`、
    `chmod_gating_rdep_vs_ftp`、`parse_octal_mode_cases`、`remote_context_menu_opens_permissions`、
    `browse_picker_wires_result_into_site_default_local`（注入 `browse_rx` 通道结果，验证回写 `site_default_local`，不调用真实对话框）。
  - `storage::preservation_tests`：`mtime_and_mode_preserved_through_meta`、`chmod_changes_mode`、
    `edit_save_backs_up_then_overwrites`（用 `with_meta` 构造以贴合生产 `meta` 分离布局）。
- `cargo clippy --all-targets`：本阶段改动源文件 **无 warning / error**（测试文件有少量预存风格 warning，非本次引入）。

## 已知限制 / 后续
- chmod 已支持 **rdep + SFTP**；FTP 仍不支持（未实现 `SITE CHMOD`），点击时给出明确提示。
- 站点「浏览…」按钮已接入 **rfd**（Linux 走纯 Rust 的 xdg-portal / ashpd 后端，无需 GTK 系统开发包）。
  无显示/无 portal 环境（CI、headless）下 `pick_folder` 会返回 `None`，UI 保持原值、不报错。
