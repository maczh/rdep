# Phase 1 完成：rdep-service 核心文件能力

> 状态：✅ 已落地，自测客户端端到端 9 步全过 ｜ 2026-10-07

## 产出
- 新 crate `crates/rdep-service`：TLS 监听（rustls 0.23 + tokio-rustls）+ 协议分发 + 文件管理 + SQLite 认证。
- 自测客户端 `rdep-selftest`（独立 bin）：loopback TLS 连服务，端到端跑通
  `AUTH → MKDIR → LS → UPLOAD → DOWNLOAD → RENAME → COPY → MOVE → DELETE`。
- 开发用自签证书 `crates/rdep-service/certs/server.{crt,key}`（`basicConstraints=CA:FALSE` + `serverAuth` EKU）。

## 已实现指令（对应协议 CmdType）
| 指令 | 状态 | 说明 |
|------|------|------|
| AUTH | ✅ | SQLite 校验用户名/密码，默认账户 `admin/admin` |
| LS | ✅ | 列目录，返回 `FileEntry`（名称/类型/大小/mtime） |
| MKDIR | ✅ | 一次性创建多级子目录 |
| UPLOAD | ✅ | `UploadInit` → 分片(`DataChunk`) → `UploadCommit`；`ChunkAssembler` 合并 + 整文件 SHA256 校验 |
| DOWNLOAD | ✅ | 读文件 → 分片 → `DataChunk` 流推送 → `CmdResponse` |
| DELETE | ✅ | 单/批量删除（文件或目录递归） |
| COPY | ✅ | 单/多文件复制，可改名或复制到子目录 |
| MOVE | ✅ | 移动到目标子目录 |
| RENAME | ✅ | 文件/子目录改名 |
| PING | ✅ | 心跳返回 `Pong` |
| Publish / Tail / Grep / Edit / Rollback | ⏳ | 后续阶段 |

## 关键设计点
- **传输层** `transport::FrameCodec`：在任意 `AsyncRead+AsyncWrite` 上做帧读写，自动处理 TCP 粘包/拆包（缓冲凑齐整帧再 `parse_frame`）。
- **会话** `session::handle`：每连接一任务；未认证仅允许 `AUTH`；维护进行中上传状态表 `transfer_id → (目标路径, ChunkAssembler)`。
- **路径安全** `storage::resolve`：按路径分量逐级拼接并拒绝 `..`，所有操作被约束在 `root` 之内，防越权逃逸。
- **TLS**：服务端从 PEM 加载证书/私钥；自测客户端将自签证书作为根 CA 信任（生产应换正规 CA 或 mTLS）。
- **认证** `db::Db`：rusqlite，`users` 表校验，`projects` 表预留给后续发布/回滚。

## 自测结果
```
[ok] AUTH
[ok] MKDIR /a/b/c
[ok] LS / -> found a/
[ok] UPLOAD /a/b/c/hello.txt (3 chunks)
[ok] DOWNLOAD content matches
[ok] RENAME -> hi.txt
[ok] COPY -> /a/b/copy.txt
[ok] MOVE -> /a/copy.txt
[ok] DELETE hi.txt + copy.txt
ALL SELFTEST PASSED
```

## 运行方式
- 自测：`cargo run -p rdep-service --bin rdep-selftest`
- 启动服务：`cargo run -p rdep-service`
  （可用环境变量覆盖：`RDEP_LISTEN` / `RDEP_ROOT` / `RDEP_CERT` / `RDEP_KEY` / `RDEP_DB`）

## 下一步：Phase 2 — rdep-client 基础 UI
- 用 **egui/eframe** 搭双栏界面（本地/远程树+列表、传输队列、站点管理），通过 `rdep-protocol` 连 service，接通本阶段全部指令。
