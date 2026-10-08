# rdep 项目长期记忆

## 项目概况
- rdep = 远程同步部署系统，Rust 三端：**rdep-client**（GUI 仿 FileZilla，egui/eframe）、**rdep-service**（headless + Web 管理）、**rdep-forwarder**（公网中转 relay）。
- 自定义私有协议 `rdep-protocol`：TLS + postcard + zstd(+base64) + 分片并发 + 断点续传。
- 设计文档见 `docs/`：`00-architecture` / `01-protocol` / `02-project-layout` / `03-roadmap`。

## 技术选型（已定）
- 异步 tokio；TLS rustls；序列化 postcard（`alloc` feature）；压缩 zstd；哈希 sha2；client GUI = egui/eframe。
- service / forwarder Web = axum；DB = rusqlite / sqlx-sqlite。

## 实施路线
- Phase 0（协议地基，**已完成**）→ 1(service 核心文件能力) → 2(client UI) → 3(发布/回滚全链路) → 4(tail/grep/edit) → 5(forwarder 中转) → 6(Web 管理) → 7(部署产物) → **8(client 体验增强，已完成：FileZilla 风格站点管理 / tail 自动滚+关窗停跟随 / grep 标红 / 编辑先备份后覆盖 / 上传下载保留 mode+mtime / 右键权限远程 chmod(rdep only))**。
- chmod 仅 rdep 协议支持（高版 `russh_sftp::client::SftpSession` 无 `setstat`），与 publish/rollback 一致。

## 约定
- 三端共用 `crates/rdep-protocol`，该 crate **不依赖异步运行时**，纯类型 + 编解码，便于单测与复用。
- 工程为 Cargo workspace（resolver=2），新 crate 加入 `Cargo.toml` members。
- **`root` 与 `meta` 必须分离**（2026-10-07 定）：`RDEP_ROOT` 是部署目录，**可只读**，只放业务文件；
  `RDEP_META`（默认 `data/meta/<root 转义>`）是 service 私有工作目录，放分片暂存 `.rdep-staging/` 与备份 `backup/`。
  任何"service 自己要用但不属于业务"的写操作都只能落在 meta。因为备份不在部署根，
  客户端取版本走协议指令 `CmdType::Backups`(17)，**不要**再用 `ls /backup`。
- **协议新增指令的清单**：`CmdType` 枚举 + `from_u8` 映射 + Request/Response 类型（protocol crate）
  → service `session.rs` 分支（带日志、失败返错误响应）→ client `Command`/`Event` + `do_*` + 派发
  → 需要 UI 的话补 i18n（**`table_sanity` 单测查重复 key**）→ 集成测试。
- **client UI 结构**：工具条 `Publish`/`Sync`/`Rollback` 三个独立按钮各自开独立窗口；
  tail/grep/edit 只从**远程文件右键菜单**进入，统一「参数窗 → Execute → 结果编辑框」三段式。
