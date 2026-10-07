# Debug 日志与「点了没反应」排障手册

## 1. 日志开关

两端（rdep-service / rdep-client）都用 `tracing`，**默认 debug 级**，可用 `RUST_LOG` 覆盖：

```bash
RUST_LOG=trace ./target/debug/rdep-service      # 服务端：stderr（systemd/容器采集）
RUST_LOG=debug  ./target/debug/rdep-client      # 客户端：stderr + 文件
RUST_LOG=rdep_client=trace,rdep_service=debug   # 分模块细调
```

> service 此前 `tracing_subscriber::fmt().init()` **忽略 RUST_LOG**（缺 env-filter feature），
> 现已在 `Cargo.toml` 打开 `env-filter` 并在 `main.rs` 里用 `EnvFilter` 初始化。

## 2. 客户端日志落在哪

- **stderr**：终端启动时可见，启动时打印 `rdep-client logging to <path>`。
- **文件**：`<配置目录>/rdep/logs/rdep-client.log`
  （Linux `~/.config/rdep/logs/`，macOS `~/Library/Application Support/rdep/logs/`，
  Windows `%APPDATA%\rdep\logs\`）；>5MB 滚动为 `rdep-client.log.old`。
  桌面环境启动 GUI 时 **stderr 不可见**，反馈问题请附此文件。
- **GUI 日志面板**：每行带 `[HH:MM:SS]` 时间戳，内容同步进 debug 日志（`gui log`）。

## 3. 服务端关键日志点

启动第一行即**生效配置**（回答「远程 `/` 到底是哪个目录」）：

```
INFO  rdep-service configuration resolved listen=127.0.0.1:19443 root=/ meta=/home/…/data/meta/_ db=… scripts=… cert=… backup_keep=10 forwarder=false
```

> `root=` 是**部署目录**（可只读）；`meta=` 是 service 私有工作目录，
> 分片暂存 `.rdep-staging/` 与备份 `backup/` 都在这里。启动日志还会打一行
> `storage: root (deployment) and meta (staging/backup) separated root=… meta=…`。
> 上传报 `stage chunk …: Permission denied` 时，**先看 `meta=` 指向的目录是否可写**。

之后每条连接：

```
DEBUG accepted tcp connection peer="127.0.0.1:32866"
DEBUG tls handshake ok peer="127.0.0.1:32866"
DEBUG session: opened peer="…"
DEBUG session: frame received peer="…" frame_type=CmdRequest payload_len=16
DEBUG auth: request peer="…" user=admin method=Password pass_len=5
INFO  auth: success peer="…" user=admin
DEBUG session: sending response peer="…" seq=1 cmd=Auth resp_ok=true
DEBUG session: ping -> pong peer="…"
DEBUG ls: request peer="…" path=/ recursive=false
DEBUG storage.ls: enter requested=/ resolved=/ recursive=false
DEBUG storage.ls: done requested=/ entries=25
DEBUG ls: ok peer="…" path=/ entries=25
```

覆盖的指令：`Auth / Ls / Mkdir / Upload(init,commit) / Download / Delete / Copy / Move /
Rename / Publish / PublishCommit / Rollback / Backups / Tail / Grep / Edit / Ping`，
每条都记录**入参**（路径、长度、标志位）与**返回值**（ok、message、条目数、字节数）；
失败分支从 `?` 改为显式 `warn` + 错误响应，不再默默断连。
`storage` 层额外记录 `requested → resolved` 的路径解析结果（沙箱边界取证）。

## 4. 客户端关键日志点

```
DEBUG action: connect backend=Rdep host=… port=… user=admin pass_len=5 ca=… use_forwarder=false
DEBUG client command <- gui cmd=Ls("/")
DEBUG probe: ping timeout 3s (assume dead)        ← 探活失败
DEBUG send cmd request seq=2 cmd=Ls body_len=6
DEBUG recv cmd response seq=2 ok=true message= body_len=…
DEBUG client event -> gui event=DirListed { path: "/", entries: … }
DEBUG action: upload backend=Rdep local=… remote=…
```

- 每个按钮动作 → `action: …`（upload/download/mkdir/delete/rename/tail/grep/edit/sync/publish/rollback/connect/disconnect）
- 后台线程收到指令 → `client command <- gui`
- 每次探活 → `probe: …`
- 每次协议交互 → `send cmd request` / `recv cmd response`
- 回传界面 → `client event -> gui`
- 口令/令牌**脱敏**：`ConnectParams` 自定义 `Debug`，日志里是 `pass="***"`、`relay_token="***"`

## 5. 典型故障读法

| 现象 | 客户端日志 | 服务端日志 / 结论 |
| --- | --- | --- |
| `op failed: read stream` | `ls failed: read stream` | 客户端**读帧失败**＝连接已被对端切断。服务端对应 `session: frame read failed, closing: read stream: …`。最常见：连到 forwarder 但目标 service 未注册/未配对（隧道被关）；或该端口上不是 rdep-service（如 8443 被 forwarder 占用，service 要换 `RDEP_LISTEN`） |
| 远程 `/` 看着像沙箱 | `storage.ls: resolved=…` | 远程 `/` = service 的 `RDEP_ROOT`。要看真实根目录，以 `RDEP_ROOT=/` 启动 service（启动日志 `root=/` 可验证） |
| 点了 Refresh 没变化 | 无 `Listing …` 日志说明按钮没触发；有则说明应答被处理 | 旧 bug：残留的 `pending_tree_ls` 把主面板应答吞进树缓存。现已在 Error/Disconnected/Refresh 时清理 |
| 连接失败 | `connect failed: {完整错误链}` | `no CA cert configured` / `read CA cert …` / `invalid peer certificate …` 按提示核对 |
| 认证失败 | `auth failed: invalid credentials` | 服务端 `auth: invalid credentials user=… failures=N`（同一连接连续失败 5 次后要求重连） |
| 上传报 `stage chunk N of transfer <id>: Permission denied (os error 13)` | `UPLOAD FAILED: commit resp: read response` 或 `stage chunk …` | ✅ 旧版把分片暂存区建在**部署根**下（`<RDEP_ROOT>/.rdep-staging/`），部署根不可写（如 `RDEP_ROOT=/` 且 service 非 root）必然 EACCES；现暂存与备份都在 `RDEP_META`。服务端会同时在 `session: handler error, closing: stage chunk …` 与 `session ended: …` 打 WARN，两侧按 `transfer_id` 对照 |
| 回滚窗口没有版本列表 | `BackupVersions` 事件未到达 / 为空 | 备份已移到 meta，客户端用新增的 `Backups` 指令（命令号 17）取版本；服务端 `backups: request` → `backups: ok versions=N` |

## 6. 回归测试

针对本次修复新增 GUI 单测（`crates/rdep-client/src/app.rs::gui_smoke`）：

- `stale_pending_tree_ls_is_cleared_on_error_and_disconnect`
- `dir_listed_updates_main_pane_after_stale_tree_marker`
- `refresh_without_connection_gives_hint`
- `gui_log_has_timestamp`
