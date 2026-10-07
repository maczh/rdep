# Phase 2 概览 — rdep-client 基础 UI + 连接 + 文件操作

> 状态：✅ 完成并通过验证（协议单测 5/5，客户端集成测试 1/1，GUI 二进制可编译链接）

## 目标

实现 rdep 桌面客户端的第一版可用形态：通过 `rdep-protocol` 与 Phase 1 的
`rdep-service` 通信，提供仿 FileZilla 的双栏文件浏览、传输队列、站点管理，
并接通 AUTH / LS / MKDIR / UPLOAD / DOWNLOAD / DELETE / COPY / MOVE / RENAME 全量指令。

## 新增产物

### `crates/rdep-client/`
- **`src/client.rs`** — 网络核心。
  - 线程模型：**GUI 主线程 + 后台 current-thread Tokio 线程**桥接。
    - 指令：`tokio::sync::mpsc`（`Command` 枚举）GUI → 后台；
    - 事件：`std::sync::mpsc`（`Event` 枚举）后台 → GUI，GUI 每帧 `try_recv`。
  - `Client` 句柄：`connect/disconnect/ls/mkdir/upload/download/delete/rename/mv/copy/next_event/recv_timeout`。
  - `Session`：`FrameCodec<TlsStream<TcpStream>>` + 自增 `seq`。
  - 实现 each 指令：`do_connect`(TLS+Auth)、`do_ls`、`do_simple`(MKDIR/DELETE/RENAME/MOVE/COPY)、
    `do_upload`(分片并发 + 逐片 SHA256 + UploadInit/Commit)、`do_download`(DataChunk 流 → 落盘)。
- **`src/app.rs`** — egui 0.29 主界面（`gui` feature 内）：
  - 顶栏：连接/断开 + 状态；左栏本地、右栏远端、底栏传输队列、中心日志。
  - 本地：目录上下、刷新、双击进目录、上传↑；远端：目录上下、刷新、下载↓、删除、新建目录。
  - 事件驱动刷新：`drain_events` 把 `Event` 映射到 UI 状态（目录列表 / 传输进度 / 操作结果）。
- **`src/main.rs`** — `eframe::run_native` 入口。
- **`tests/integration.rs`** — 无界面集成测试：起真实 `rdep-service`（TLS+SQLite），
  校验 连接→MKDIR→LS→UPLOAD→DOWNLOAD(内容比对)→DELETE→断开 全链路。
- **`Cargo.toml`** — `egui`/`eframe` 设为 `optional`，由 `gui` feature（默认开启）引入；
  `[[bin]]` 标注 `required-features = ["gui"]`，从而无显示环境可用 `--no-default-features` 仅编译网络核心。

### 协议层补充
- `crates/rdep-protocol/src/lib.rs` 新增 `Direction { Upload, Download }` 枚举（供 `Event::TransferStarted` 标记方向）。

## 验证

| 项 | 命令 | 结果 |
|---|---|---|
| 协议单测 | `cargo test -p rdep-protocol` | 5/5 通过 |
| 客户端集成测试 | `cargo test -p rdep-client --no-default-features --test integration` | 1/1 通过 |
| GUI 库编译 | `cargo build -p rdep-client --lib`（含 gui） | 通过（app.rs 编译无误） |
| GUI 二进制链接 | `cargo build -p rdep-client` | 通过（egui/eframe 在本机可链接） |

> 备注：本机无 X server（无 `DISPLAY`），GUI 窗口实际启动需在桌面环境进行；
> 代码与编译/链接均已验证。

## 踩坑与修复（本阶段）
- `rustls_pemfile::certs()` 返回 `Iterator<Item=Result>`，不能直接 `.context()`；改为先 `collect::<Result<Vec<_>,_>>()` 再处理。
- `ServerName::try_from(&str)` 借用了 host 引用，导致 `'static` 逃逸报错；改用 `ServerName::try_from(p.host.clone())`（`TryFrom<String> → ServerName<'static>`）。
- `lib.rs` 原 `pub use client::{..., Direction, ...}` 无法编译（`Direction` 在 client 仅为私有 import）；改为协议层直接 `pub use rdep_protocol::Direction`，并弃用原 `DirectionUpload` 常量、改用 `Direction::Upload`。
- `app.rs` 在 `for` 循环内对 `self` 既不可变借用（`self.local_entries.iter()`）又可变借用（`enter_local`），报 E0502；改为循环内只记录导航目标、循环后统一调用。

## 下一步（Phase 3+）
- **Phase 3** 发布/回滚全链路：service 备份目录 `backup/<YYMMDDHHmm>/`、重启脚本、剪枝；client 发布清单 UI、回滚版本选择。
- **Phase 4** TAIL / GREP / EDIT。
- **Phase 5** rdep-forwarder（服务注册长连、客户端路由、零解析转发）。
- **Phase 6** Web 管理后台（service + forwarder，admin/admin）。
- **Phase 7** 部署产物（systemd / Dockerfile / docker-compose / Windows 安装脚本）。
