# Phase 3 概览 — 发布（备份 + 重启脚本）/ 回滚全链路

> 状态：✅ 完成并通过验证（协议单测 5/5，集成测试 2/2，全 workspace 编译无警告）

## 目标

打通「发布 → 备份 → 重启 → 出错回滚」闭环：
发布一批文件时**先自动备份被覆盖的旧文件**（时间戳版本目录），全部写完后**执行指定重启脚本**；
若发布出问题，可选择任一备份版本**回滚恢复**。

## 协议（rdep-protocol）
- 新增 `CmdType::PublishCommit = 16`（触发收尾：执行重启脚本）。
- 新增 `PublishCommitRequest { remote_dir }`。
- 复用既有 `PublishRequest/PublishItem`（发布宣布）、`UploadInit.backup_first`（备份标记）、
  `RollbackRequest`（回滚）。

## 服务端（rdep-service）
### Storage（`storage.rs`）
- 新增 `backup_keep`（默认 10，可用 env `RDEP_BACKUP_KEEP` 覆盖）。
- `save_with_backup(remote_path, data)`：写入前若目标已存在，先复制到
  `backup/<YYMMDDHHmm>/<相对路径>`，再写新内容；超出保留上限时剪枝最旧版本。
- `list_backup_versions()`：列出 `backup/` 下所有版本（升序）。
- `rollback(remote_dir, version)`：校验版本号（仅数字，防路径注入），
  把 `backup/<version>/<remote_dir>` 递归恢复到目标目录（覆盖）。
- 版本时间戳用 `chrono::Local::now().format("%y%m%d%H%M")`（定宽，可字典序排序）。

### 配置（`config.rs`）
- `ServiceConfig` 新增 `scripts_dir`（env `RDEP_SCRIPTS`，默认 `data/scripts`）。

### 会话（`session.rs`）
- `ActiveUpload` 改为存 `remote_path: String` + `backup: bool`；提交时若 `backup` 则走
  `save_with_backup`，否则普通 `save`。
- `Publish`：宣布发布（校验重启脚本存在），进入发布态（记录 remote_dir / script / 完成计数）。
- `PublishCommit`：执行 `sh <script> <remote_dir>`；非零退出返回 `RestartFailed`。
- `Rollback`：调用 `storage.rollback`，失败返回 `PathNotFound`。
- 脚本解析：优先 `scripts/<id>`，再 `scripts/<id>.sh`。

> 说明：重启脚本为服务端本地脚本，由 `sh` 执行（部署工具设计使然）。

## 客户端（rdep-client）
- `client.rs` 新增：
  - `PublishFile { remote_path, local_path }`（发布清单单条目）。
  - `Command::Publish { remote_dir, restart_script_id, files }` / `Command::Rollback { remote_dir, version }`。
  - `Event::PublishDone { ok, message }`（重启脚本执行结果）。
  - `do_publish`：宣布发布 → 逐文件「带备份上传」(UploadInit backup_first / DataChunk / UploadCommit) → 收尾 `PublishCommit` 触发重启脚本。
  - `do_rollback`：发送 `RollbackRequest`。
  - `Client::publish/rollback` 方法。
- `app.rs`：顶栏加「发布/回滚」按钮，打开窗口——发布区（远端目录、重启脚本 ID、加入发布清单、执行发布），
  回滚区（远端目录、列出备份版本、下拉选择版本、执行回滚）。

## 验证
| 项 | 命令 | 结果 |
|---|---|---|
| 协议单测 | `cargo test -p rdep-protocol` | 5/5 ✅ |
| 集成测试（含发布回滚） | `cargo test -p rdep-client --no-default-features --test integration` | 2/2 ✅ |
| 全 workspace 编译 | `cargo build` | ✅ 无警告 |

`publish_and_rollback_e2e` 覆盖：上传 v0 → 发布 v1（自动备份 v0 + 执行 `restart` 脚本 exit 0）→
校验远端=v1 → `ls /backup` 取备份版本 → 回滚 → 校验远端恢复=v0。

## 踩坑
- `send_cmd` 收 `&mut FrameCodec`（不是 `&mut Session`），需传 `&mut s.codec`。
- `CmdType::PublishCommit` 是枚举变体，不能当顶层类型 import（只有 `PublishCommitRequest` 是类型）。

## 下一步（Phase 4+）
- **Phase 4** TAIL / GREP / EDIT（实时日志 tail、内容检索 grep、远端在线编辑）。
- **Phase 5** rdep-forwarder（服务注册长连、客户端路由、零解析转发）。
- **Phase 6** Web 管理后台（service + forwarder，admin/admin）。
- **Phase 7** 部署产物（systemd / Dockerfile / docker-compose / Windows 安装脚本）。

> 已知限制：备份版本时间戳为「分钟」粒度，同一分钟内两次发布到同一文件会复用同一版本目录。
