# rdep 私有协议规范 v0.1（与实现对齐）

> 本文档已与 `crates/rdep-protocol` 当前实现对齐。标 ✅ 者为已实现并由集成测试覆盖；
> 标 ⏳ 者为结构已定义、尚未在客户端接线。

## 1. 传输与分层

- 承载：TCP + **TLS 1.3**（rustls）。中转模式下 client⇄forwarder、forwarder⇄service 均为 TLS。
- 内容处理管线（发送方向）：`业务结构体 → postcard 编码 → zstd 压缩 → （可选）base64 编码`。
  接收方向逆序还原。帧头 `flags` 位声明启用了哪些步骤，保证可还原。
- 一个逻辑会话（session）内划分三类通道，复用同一条 TLS 连接：
  - **控制通道**：请求/响应式指令（`CmdRequest` / `CmdResponse`）。
  - **数据通道**：文件分片传输（`DataChunk`）。
  - **流通道**：`StreamPush`（如 TAIL 主动推送）。

## 2. 帧格式（Frame）—— 头固定 12 字节

```
 0        4      5      6      7      8              12
 +--------+------+------+------+------+-----------+---------------+
 | magic  | ver  | type | flags| rsvd | payload_len |    payload    |
 |"RDEP"  |      |      |      |      |   (u32 BE)  |  (按flags解码) |
 +--------+------+------+------+------+-----------+---------------+
```

- `magic`：`0x52444550`（"RDEP"，大端），常量 `MAGIC`。
- `ver`：协议版本（u8），当前 `0x01`（常量 `PROTOCOL_VERSION`）。
- `type`：帧类型（u8，`FrameType`）：
  - `0x01` `CmdRequest` / `0x02` `CmdResponse` / `0x03` `DataChunk` /
    `0x04` `StreamPush` / `0x05` `Ctrl`
- `flags`（`FrameFlags`，按位）：
  - `0x01` `COMPRESSED`（zstd）/ `0x02` `BASE64` / `0x04` `CHUNK_CHECKSUM`
- `payload_len`：**解码前** payload 字节数（u32 大端）。
- 常量 `HEADER_LEN = 12`。

> 实测注意：TLS 本身是流式，TCP 会粘包/拆包，`FrameCodec` 内部维护读缓冲，
> 凑齐 `12 + payload_len` 才解析出一帧。

## 3. 控制通道指令集（CmdType）

```
enum CmdType {          // 实际线值
    Auth = 1,          // 登录认证
    Ls,                // 目录列表
    Download,          // 下载文件
    Upload,            // 上传（Init/Commit 复用此 type，按 body 可解析性区分）
    Publish,           // 发布：宣布（进入发布态）
    Tail,              // 流式 tail -f
    Grep,              // 内容查找
    Edit,              // 读取/保存远端文件
    Rollback,          // 回滚到指定备份版本
    Delete,            // 单个/批量删除
    Copy,              // 复制
    Move,              // 移动
    Rename,            // 改名
    Mkdir,             // 一次性创建多级子目录
    Ping,              // 心跳
    PublishCommit = 16,// 发布收尾：执行重启脚本
    Backups = 17,      // 列出备份版本（备份库不在部署根，无法用 Ls 遍历）
}
```

> `Backups`（命令号 17）是为 **root/meta 目录分离** 补的指令：备份版本库已从部署根
> （`<RDEP_ROOT>/backup/`）迁到 service 私有工作目录（`<RDEP_META>/backup/`），
> 客户端无法再用 `Ls /backup` 枚举版本，因此单列一条指令。
> 请求 `BackupsRequest{}`，响应 `BackupsResponse{ versions: Vec<String> }`（版本号升序）。

### 请求/响应通用结构（postcard 编码）

```rust
struct CmdRequest  { seq: u32, cmd: CmdType, body: Vec<u8> }
struct CmdResponse { seq: u32, ok: bool, code: u16, message: String, body: Vec<u8> }
```

除 `Auth` 外，其余指令须先 `AUTH` 成功，否则回 `1001`。

### 关键指令载荷

| 指令 | 请求 body | 响应 body | 状态 |
|---|---|---|---|
| Auth | `AuthRequest{user,pass,method}` | `AuthResponse{token,expires}` | ✅ |
| Ls | `LsRequest{path,recursive}` | `LsResponse{entries:[FileEntry]}`（`recursive=true` 返回该目录下所有文件的相对路径，供目录同步做差异比对） | ✅ |
| Mkdir | `MkdirRequest{paths:[..]}` | — | ✅ |
| Upload | `UploadInit` 或 `UploadCommit`（见 §4） | — | ✅ |
| Download | `DownloadRequest{remote_path,policy}` | 先下发 `DataChunk` 流，末尾一个 `CmdResponse`，**响应体 = 文件 sha256(32B)**（client 落盘后校验） | ✅ |
| Delete | `DeleteRequest{paths:[..]}` | — | ✅ |
| Copy | `CopyRequest{src,dst,policy}` | — | ✅ |
| Move | `MoveRequest{src,dst_dir}` | — | ✅ |
| Rename | `RenameRequest{src,new_name}` | — | ✅ |
| Publish | `PublishRequest{remote_dir,restart_script_id,project,items}` | 进入发布态（校验脚本存在） | ✅ |
| PublishCommit | `PublishCommitRequest{remote_dir}` | 执行 `sh <script> <remote_dir>` 的结果 | ✅ |
| Rollback | `RollbackRequest{remote_dir,version}` | — | ✅ |
| Tail | `TailRequest{path,lines,follow}` | 进入流通道（`StreamPush`） | ✅ |
| Grep | `GrepRequest{path,pattern,flags}` | `GrepResponse{lines:[..]}` | ✅ |
| Edit | `EditRequest{remote_path,content}`（见下） | 读：`body`=文件内容；存：`ok` | ✅ |
| Ping | — | — | ✅ |

`FileEntry{ name, is_dir, size, mtime, mode }`。

### 发布 / 回滚时序（实现要点）

```
client                                   service
  |-- Publish{remote_dir, script_id, items} -->|  进入发布态
  |<------------- CmdResponse(ok) -----------|
  |-- UploadInit{..., backup_first=true} --->|   (逐个文件)
  |-- DataChunk ... (分片) ----------------->|
  |-- UploadCommit{transfer_id} ----------->|   备份旧文件→落盘→计完成数
  |<------------- CmdResponse(ok) -----------|
  ... (重复所有文件) ...
  |-- PublishCommit{remote_dir} ---------->|   执行重启脚本
  |<--------- CmdResponse(ok / RestartFailed) -|
```

- **备份**：写新内容前，若目标已存在，先复制到 `<RDEP_META>/backup/<YYMMDDHHmm>/<相对路径>`；
  超出 `RDEP_BACKUP_KEEP`（默认 10）自动剪枝最旧版本。
  ⚠️ 备份库在 **service 私有工作目录（meta）**，**不在部署根**——部署根可能只读
  （如 `RDEP_ROOT=/` 且 service 非 root），且客户端 `ls /` 不应看到 `backup`。
  因此枚举版本走 `CmdType::Backups`，不能用 `Ls /backup`。
- **重启脚本**仅在 `PublishCommit` 执行（不是每个 Upload commit 都执行）。

**项目模式（`project` 字段）**

- `project` 非空时，service 从 `projects` 表取出 `remote_dir` 与 `restart_script`，
  并**忽略**客户端自报的 `remote_dir` / `restart_script_id`。
- 随后的每条 `UploadInit.remote_path` 会被**重新落到项目目录之下**（只取文件名），
  因此客户端无法把文件写到项目目录之外——项目记录是部署位置的唯一决定方。
- `items` 只是**声明性清单**（客户端据此规划分片上传），真正决定落盘位置的是
  `UploadInit.remote_path`；约束必须在 Upload 阶段施加，改写 `items` 是无效的。
- 项目不存在 → `CmdResponse{ok:false, message:"project not found"}`。
- **回滚**：`Rollback{remote_dir, version}` 把 `<meta>/backup/<version>/<remote_dir>` 恢复覆盖；
  version 仅允许数字（防路径注入）；备份源强制位于 meta 之内，越权直接拒绝。

### Edit 读 / 存

```rust
struct EditRequest {
    remote_path: String,
    content: Option<String>,   // None = 读取；Some = 保存（先备份再覆盖）
}
```

## 4. 文件分片传输（并发 + 校验）

### 4.1 初始化（控制通道，`CmdType::Upload`）
```rust
UploadInit {
    transfer_id: u64, remote_path: String, size: u64, mtime: i64,
    chunk_size: u32, total_chunks: u32, file_sha256: [u8;32],
    backup_first: bool,      // 发布/Edit 保存时为 true
    mode: u32,                // 权限位（unix st_mode & 0o777；0/非unix=不设置）
}
```
服务端响应体为 `UploadInitAck{ received: Vec<u32> }`——**已暂存的分片序号（升序）**，
客户端据此只补传缺失片（断点续传）。落盘后服务端按 `mode` 恢复权限位（保留可执行位）。

### 4.2 分片帧（`DataChunk`）
```rust
DataChunk { transfer_id: u64, index: u32, data: Vec<u8>, chunk_sha256: [u8;32] }
```
服务端校验分片 `chunk_sha256` 后，把分片落盘暂存到
`<RDEP_META>/.rdep-staging/<transfer_id>/<index>.chunk`（先写 `.tmp` 再原子改名）。
**「已收片集合」= 现存的分片文件**，天然跨连接/跨会话持久化。
⚠️ 暂存区必须在 **meta**（service 私有、保证可写）而非部署根：早期实现建在
`<RDEP_ROOT>/.rdep-staging/`，于是 `RDEP_ROOT=/` 且 service 非 root 时上传直接
`stage chunk …: Permission denied (os error 13)`。

### 4.3 提交与校验（控制通道，`CmdType::Upload` + `UploadCommit{transfer_id}`）
服务端：从暂存区按 `0..total_chunks` 顺序合并 → 校验整文件 `file_sha256`（不符回
`3002` 并清理暂存）→ （`backup_first` 则先备份）→ 落盘 → 清理暂存目录。

### 4.4 断点续传（✅ 已实现）
- 客户端用**稳定 `transfer_id` = hash(remote_path + 文件 sha256)**：同一文件同一内容的
  重试/断线重连复用同一 id → 命中服务端暂存区；内容变化则 id 变化，天然隔离。
- 流程：init → 读 `UploadInitAck.received` → **跳过已收片、只发缺失片** → commit。
- 集成测试 `upload_resume_e2e`：发 0/1 片后断线 → 重连 init 收到 `received=[0,1]`
  → 只补发 2 号片 → commit，合并文件与源一致、暂存区被清理。
- 早期设计曾用 `Ctrl::{Resend,AckBitmap,Query}` 表达续传请求，但**从未实现**，
  且服务端会静默丢弃它们。续传需求已由上面的 `UploadInitAck.received` 完整满足，
  故这三个变体已从协议中**移除**——保留无法工作的协议表面只会误导第二实现者。

## 5. 流通道 TAIL（server push）

```rust
StreamPush { transfer_id: u64, line: String, eof: bool }   // FrameType::StreamPush
```

- `Tail{path,lines,follow:false}`：回灌文件末尾 N 行 → 推 `eof` → 末尾一个 `CmdResponse`。
- `follow:true`：增量轮询（读取新字节、拆行、推送），客户端发 `Ctrl::Stop` 结束；
  service 以 `timeout(200ms, read_frame)` 探测控制帧，并设 3600s 上限。结束时同样补 `eof`+`CmdResponse`，
  以保证会话同步（客户端会一直读到最终 `CmdResponse`）。

## 6. 控制帧 CTRL（`FrameType::Ctrl`，Ctrl 枚举）
```
enum Ctrl { Ping, Pong, Stop }
```

| 变体 | 方向 | 用途 |
|---|---|---|
| `Ping` | client → service | 探活（配合自动重连）；service 回 `Pong` |
| `Pong` | service → client | 探活响应 |
| `Stop` | client → service | 结束 tail follow |

**向前兼容要求**：service 收到**无法解码**的 Ctrl 帧（未来版本新增的变体、
或旧客户端发送的已废弃变体）时**必须忽略而非断连**，且不返回任何响应。
否则协议版本错配会直接踢掉整个会话。集成测试
`undecodable_ctrl_frame_does_not_kill_session_e2e` 覆盖此行为。

## 7. 错误码（`ErrorCode`）

| code | 含义 |
|---|---|
| 0 | 成功 Ok |
| 1001 | 认证失败 AuthFailed |
| 1002 | 会话过期 SessionExpired |
| 2001 | 路径不存在 PathNotFound |
| 2002 | 权限不足 PermissionDenied |
| 2003 | 文件名/命名冲突 NameConflict |
| 3001 | 分片校验失败 ChunkChecksumFailed |
| 3002 | 整文件 SHA 不匹配 FileChecksumMismatch |
| 4001 | 重启脚本执行失败/超时 RestartFailed |
| 5001 | forwarder 未找到目标 service ForwarderNoService |
| 6001 | 未实现 NotImplemented |

## 8. 与 forwarder 的中转（`rdep_protocol::relay`）

握手只发生在**连接第一帧**（复用本协议的 Frame 编解码）；握手成功后 forwarder **零解析**
按字节 `copy_bidirectional` 透传，不再解释 rdep 协议内容，缩小攻击面。

```
service 侧（拨号 forwarder 的 service 端口）：
  RelayHello{ service_id, label, token }  --->  forwarder
  <--- RelayHelloAck{ ok, message }       ---    校验 token；隧道停放进注册表，等待 client 配对
  （此后该连接 = 一条等待中的隧道，service 阻塞等第一个 rdep 帧）

client 侧（连 forwarder 的 client 端口）：
  RelayConnect{ target_service_id, token } --->  forwarder
  <--- RelayConnectResp{ ok, code, message } -    ok 后连接转字节透传，client 继续跑 rdep 协议
```

- 失败码：密钥错 → `1001`；目标 service 不在线/无可用隧道 → `5001`。
- **连接池 / 并发模型**：service 侧维持 `RDEP_MAX_SESSIONS`（默认 4）条常驻注册连接；
  forwarder 按 `service_id` 停放这些连接（上限 16 条/服务）。每个 client 会话从池中取走
  **一条**隧道配对，因此**同一 service 可并发服务最多 `RDEP_MAX_SESSIONS` 个 client**。
  会话结束后隧道关闭，service 对应循环 3s 后重连补回池中，维持并发能力。
  （相比「信令长连接 + 每会话独立连接」的方案，该模型实现更简单、握手不变。）

## 附：客户端辅助枚举
- `Direction{ Upload, Download }`：仅用于客户端 UI / 传输事件标记方向，不上线路。

## 附：客户端韧性行为（基于本协议实现，非新指令）
- **自动重连**：client 在每个操作前用 `Ctrl::Ping` 探活（service 回 `Ctrl::Pong`）；连接掉线时
  自动用上次参数重连并重新 `AUTH`，随后重试的操作即可命中断点续传。
- **断点续传**：`transfer_id = hash(remote_path + 文件 sha256)` 稳定派生；重试/重连时 init 拿到
  `UploadInitAck.received` 后只补缺失片（详见 §4.4）。
- **下载流式落盘**：`Download` 响应体携带的 sha256 用于 client 落盘后完整性校验；落盘经
  临时文件 + 原子改名，失败不留半成品。
- **目录同步**：client 用 `Ls{recursive}` 取远端全量文件，以「大小 + mtime」快筛判定变更，
  对变更文件走「带备份 + 断点续传上传」，可选删除远端多余文件；均在既有指令之上编排，无新指令。

---
*相关文档：架构 `00-architecture.md`、目录 `02-project-layout.md`、路线图 `03-roadmap.md`。*
