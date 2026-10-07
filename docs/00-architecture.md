# rdep 远程同步部署系统 — 总体架构

> 版本：v0.1（已实现，Phase 0–7 完成）｜ 2026-10-07
> 本文描述的“核心职责/拓扑/模型”均已落地；个别实现细节以 `01-protocol.md` 与
> `phase{N}-overview.md` 为准，文中已标注与实现有出入之处。

## 1. 三大子系统

| 子系统 | 形态 | 运行位置 | 语言 | 核心职责 |
|--------|------|----------|------|----------|
| **rdep-client** | 桌面 GUI（仿 FileZilla） | 运维/开发终端（Win/Mac/Linux-X） | Rust | 站点管理、双栏文件树/列表、传输列表、发布/回滚、tail/grep/编辑 |
| **rdep-service** | headless 服务 + Web 管理 | 目标服务器（内网，Linux/Win Server） | Rust | 接收 rdep 协议、文件读写、备份/回滚、重启脚本执行、向 forwarder 注册 |
| **rdep-forwarder** | 公网中转代理（纯 relay） | 公网 Linux | Rust | 在 client 与 service 之间智能转发数据包，Web 管理，SQLite 审计 |

## 2. 连接拓扑

```
[直连模式]
  rdep-client  ──TLS──▶  rdep-service

[中转模式]  (service 在 NAT/内网，client 在公网)
  rdep-client  ──TLS──▶  rdep-forwarder  ──TLS──▶  rdep-service
                              ▲                         │
                              └──── service 启动时主动注册长连接 ───┘
```

- **forwarder 是反向隧道枢纽**：service 启动时向 forwarder 拨号注册，建立一条**常驻 TLS 长连接**（断线自动重连）。
- client 连接 forwarder 的「client 端口」，forwarder 按会话把流量路由到已注册、对应的 service 隧道。
- client 端配置「是否需要中转」+ forwarder 的 ip/端口；中转模式下 client 实际上连接的是 forwarder，由 forwarder 寻址到具体 service。

## 3. 备份与回滚模型

> **目录分离（重要）**：service 有两个目录——
> `root`（= `RDEP_ROOT`，对外暴露的部署根，客户端看到的 `/`，**可能只读**）与
> `meta`（= `RDEP_META`，service 私有工作目录，存放**分片暂存区** `.rdep-staging/` 与
> **备份库** `backup/`）。二者必须分开：否则部署根不可写时上传直接
> `Permission denied (os error 13)`，且客户端 `ls /` 会看到内部目录。

- **发布（PUBLISH）**：client 先发 `Publish` 进入发布态，随后逐文件「带备份上传」——
  service 写新内容前若目标已存在，先把**当前远程文件复制一份**到
  `<meta>/backup/<YYMMDDHHmm>/<相对路径>`；全部文件完成后由 `PublishCommit` 执行重启脚本。
- **最大历史备份数**：可配（`RDEP_BACKUP_KEEP`），默认 10；超出时 prune 最旧的版本目录。
- **备份在 meta 下，客户端无法用 `Ls` 遍历**：因此枚举版本用独立的 `CmdType::Backups`
  （命令号 17）→ `BackupsResponse{ versions }`；回滚窗口打开时自动拉一次。
- **回滚（ROLLBACK）**：client 指定某历史版本，service 从 `<meta>/backup/<version>/<remote_dir>`
  把文件恢复覆盖到远程目录；备份源强制位于 meta 内，越权拒绝。
  > 实现注记：当前**回滚只恢复文件、不自动执行重启脚本**（重启仅在发布 `PublishCommit` 时触发）。
  > 如需“回滚后重启”，后续可扩展。
- 备份目录结构与远程目录结构一致，便于整目录恢复。

## 4. 重启执行

- 发布收尾（`PublishCommit`）时，service 按 `restart_script_id` 在 `RDEP_SCRIPTS` 目录
  解析脚本（先试 `<id>`，再试 `<id>.sh`），以 `sh <script> <remote_dir>` 同步执行；
  非零退出回 `4001 RestartFailed`。
  > 实现注记：当前**仅 `sh` 执行**。`.ps1`/`.bat`/systemd/docker 等类型为设计扩展点，尚未实现。
- 脚本由 client 在每次发布时以 `restart_script_id` 指定；service 端未强绑定“项目→脚本”映射
  （Web 的 projects 表虽含 `restart_script` 字段，但发布流程未读取它）。
- 执行结果（成功/失败 + stderr 摘要）回传 client 展示。

## 5. 认证

- 账户/密码/认证方式在 **rdep-service** 中配置（AUTH 指令对接）。
- forwarder 的 Web 管理默认账户 `admin/admin`，service Web 管理默认账户 `admin/admin`。

## 6. 技术选型（建议）

| 维度 | 选型 | 理由 |
|------|------|------|
| 异步运行时 | **tokio** | 三端统一，生态成熟 |
| TLS | **rustls**（纯 Rust，无 OpenSSL 依赖） | 跨平台打包简单，GUI 端友好 |
| 二进制序列化 | **postcard** | 极紧凑、no_std 友好，适合私有协议 |
| 压缩 | **zstd** | 高压缩比 + 快，分片/整体均可 |
| 分片校验 | **sha2 (SHA-256)** | 每片 + 整文件双重校验 |
| client GUI | **egui / eframe** | 纯 Rust、单二进制、跨平台、适合双栏文件管理器 |
| service/fwd Web | **axum** + 内嵌单页（JSON REST，无需前端构建） | 轻量 REST，管理界面随二进制分发 |
| 数据库 | **rusqlite**（同步，简单）或 **sqlx-sqlite**（异步） | 用户/项目/备份元数据/审计 |

> 注：GUI 也可选 `iced` 或 `tauri`。egui 在「双树+列表+传输队列」这类密集控件场景开发效率最高，推荐作为首选，后续可替换。

## 7. 关键风险点（实现状态）

1. **并发分片合并顺序**：✅ commit 阶段按 `chunk_index` 严格重排并逐片 + 整文件双重 SHA 校验；
   单片失败回 `ChunkChecksumFailed`（`RESEND` 结构已定义，当前走整文件重传）。
2. **service 长连接注册重连**：部分解决——service 侧断线 3s 自动重连重注册，forwarder 注册表
   同 id 新连接会替换旧连接（旧连接被丢弃）。**尚未做显式心跳/僵尸超时回收**。
3. **重启脚本安全**：⚠️ 当前以 `sh` 执行 `RDEP_SCRIPTS` 下的脚本（部署工具设计使然，脚本即代码）。
   建议生产环境：脚本目录只允许受信任内容、`RDEP_SCRIPTS` 收紧权限、后续可加白名单/超时熔断。
4. **大文件/断点续传**：⏳ 协议结构（`Ctrl::AckBitmap/Resend`、`ChunkAssembler::missing`）已就绪，
   但客户端当前整文件重传，位图续传尚未接线。
5. **tail 流控**：✅ client 可随时 `Ctrl::Stop` 结束跟随；service 侧轮询 200ms + 3600s 上限。
