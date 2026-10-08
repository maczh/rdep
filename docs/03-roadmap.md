# rdep 实施路线图（分阶段）

> 原则：先协议后业务、先 service 后 client、中转与 Web 管理后置。每阶段可独立验证。
>
> **状态：Phase 0–7 已全部完成并验证；Phase 8（client 体验增强）已交付。** 每阶段交付说明见 `docs/phase{N}-overview.md`；
> 快速上手见根目录 `README.md`，部署产物见 `deploy/`。

## Phase 0 — 地基：workspace + rdep-protocol
- 初始化 Cargo workspace、rust-toolchain、.gitignore。
- 实现 `rdep-protocol`：帧编解码、postcard+zstd(+base64) 管线、CmdType 全量定义、分片/校验/断点续传结构、错误码。
- 单元自测：编码→解码还原、分片合并校验、base64 开关。
- **产出**：可单测通过的协议库，三端共用基础。
- **验证**：`cargo test -p rdep-protocol` 全绿。

## Phase 1 — rdep-service 核心文件能力（CLI 先行）
- TLS 监听（rustls），协议分发骨架。
- 实现：AUTH、LS、MKDIR(多级)、UPLOAD(纯上传)、DOWNLOAD、DELETE、COPY、MOVE、RENAME。
- SQLite：用户/认证方式、项目/远程目录映射。
- 先以简单 CLI 自测脚本驱动（暂不 GUI）。
- **产出**：service 能完成基础文件管理。
- **验证**：用临时测试 client（或脚本）跑通上述指令。

## Phase 2 — rdep-client 基础 UI + 连接
- egui 双栏：本地树/列表、远程树/列表、传输列表、站点管理。
- 通过 rdep-protocol 连接 service，接通 Phase 1 的全部指令。
- 本地文件浏览用异步线程，远程用协议。
- **产出**：可用 GUI 做站点管理 + 基础文件传迁。
- **验证**：连 Phase 1 service，完成一次上传/下载/改名/删除。

## Phase 3 — 发布 / 回滚 全链路
- service：PUBLISH（建 `backup/<ts>/`、先备份后覆盖、调重启脚本、最大备份 prune）、ROLLBACK（恢复 + 重启）。
- client：发布列表 UI、重启脚本配置页、历史版本选择回滚。
- **产出**：核心差异化能力可用。
- **验证**：发布→改文件→发布→回滚到旧版本，服务按预期重启。

## Phase 4 — 运维增强：TAIL / GREP / EDIT
- service：TAIL 流推送（限流、STOP）、GREP、EDIT（下载临时→保存覆盖并自动备份）。
- client：tail 窗口（实时刷新）、grep 结果页、内置文本编辑器。
- **验证**：tail 实时性、grep 结果正确、edit 保存后远程变更 + 备份生成。

## Phase 5 — rdep-forwarder 中转
- forwarder：service 注册长连接 + 断线重连、client 会话路由、零解析透传、僵尸回收。
- service：启动向 forwarder 注册（可配 ip/端口、自动重连）。
- client：中转开关 + forwarder 地址，端到端经公网中转打通 Phase 1–4。
- **验证**：client 经 forwarder 连内网 service 完成一次发布。

## Phase 6 — Web 管理界面
- service Web：admin/admin 登录、用户/项目/备份/重启脚本管理、备份列表与回滚触发。
- forwarder Web：admin/admin 登录、注册 service 列表、流量/审计查看。
- **产出**：无 GUI 也能后台管理。

## Phase 7 — 部署与交付
- systemd unit、Dockerfile、docker-compose、Windows install.ps1/bat、Win 服务注册。
- 发布构建脚本（多平台 client 打包）。
- 用户文档 / 快速上手。

## 依赖与里程碑建议
```
Phase0 ─► Phase1 ─► Phase2 ─► Phase3 ─► Phase4
                       │
                       └─► Phase5(forwarder) ─► Phase6(Web) ─► Phase7(部署)
```
- 最短可用路径（MVP）：Phase 0→1→2→3（直连即可发布/回滚）。
- 公网场景追加：Phase 5。
- 产品化追加：Phase 4、6、7。

## Phase 8 — client 体验增强（已交付）
承接 Phase 2/4 的站点管理与运维能力，补齐 FileZilla 风格的易用性与属性保留细节。
对应需求：站点管理 UI 重构、tail/grep/edit 体验、上传下载属性保留、远程 chmod。

1. **FileZilla 风格站点管理器**（`rdep-client::app::sites_window`）
   - 左侧「My Sites」可折叠站点树 + 新建/删除；右侧四页签：General / Advanced / Transfer Settings / Charset。
   - 支持 sftp / ftp / rdep 三类协议；切换协议自动同步默认端口（22 / 21 / 8443）。
   - 新增字段全部持久化到本地 `sites.json`：`login_type`（Normal/Key file/Ask）、
     `background_color`、`comment`、`default_local_dir`、`default_remote_dir`、`concurrency`（1–16）、`charset`（Auto/UTF-8）。
   - 改名保存时若站点名变化会先删除旧条目（`save_current_form_as_site` 改名保护）。

2. **tail 对话框体验**（`tail_dialog`）
   - 日志编辑框只读 + 自动滚动到底部（`scroll_to_cursor(BOTTOM)`）。
   - 收到新日志行（`Event::TailLine`）置 `tail_view_dirty`，渲染后复位，保证跟进最新内容。
   - 窗口关闭（`!open && tail_follow`）自动执行 stop follow 并记日志「tail stopped」。

3. **grep 结果标红**（`render_grep_match`）
   - 匹配子串用红色 `Color32::RED` 高亮（等宽字体 LayoutJob 逐段拼接）；大小写忽略开关安全处理非 ASCII。

4. **编辑保存 = 先备份后覆盖**（`rdep-service::storage::save_with_backup`）
   - 编辑保存时先把原文件复制进 `meta/backup/<版本戳>/<相对路径>`，再用新内容覆盖原文件名（满足「先改名备份，再上传新内容成原文件名」）。

5. **上传/下载保留文件属性与时间**
   - 上传：`set_mtime`（`std::fs::set_times`）+ `apply_mode` 保留 mtime 与权限位。
   - 下载：`DownloadResponse{mode, mtime, sha256}` 带回属性，client 侧 `apply_local_mode`/`apply_local_mtime` 还原。

6. **右键「权限」→ 远程 chmod（rdep only）**
   - 远程文件右键菜单新增「Permissions」按钮 → `chmod_dialog` 输入八进制模式（0–0o7777 校验）。
   - 协议层 `CmdType::Chmod` / `ChmodRequest{path,mode}`、service `storage::chmod`、client `Client::chmod` 全链路打通。
   - 因高版 SFTP `SftpSession` 无 `setstat`，chmod 仅 rdep 协议支持；ftp/sftp 点击时给出明确提示「requires the rdep protocol...」。

- **验证**：`cargo test -p rdep-protocol -p rdep-service -p rdep-client`（lib 76 + integration 18 全绿）；
  `cargo clippy --all-targets` 无 error。新增/增强单测覆盖：站点字段往返、grep 标红渲染、tail 脏标记、
  chmod rdep/ftp 门控、八进制解析、备份改名覆盖、mtime/mode 保留。
- **详细**：见 `docs/phase8-overview.md`。
