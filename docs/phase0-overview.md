# Phase 0 完成：rdep-protocol 协议库

> 状态：✅ 已落地并通过单元测试 ｜ 2026-10-07

## 产出
- 工程骨架：Cargo workspace（`resolver=2`）、`rust-toolchain.toml`、`Cargo.lock`、`.gitignore`。
- `crates/rdep-protocol`：**纯类型 + 编解码层**，零异步依赖，三端（client/service/forwarder）共用。
- 单元测试 **5 个全部通过**（`cargo test -p rdep-protocol`）。

## 已实现能力
1. **帧编解码**：`magic/version/type/flags + payload`，支持流式分帧（返回已消费字节，便于 TCP 粘包处理）。
2. **编码管线**：`postcard → zstd 压缩 → （可选）base64`，解码逆序还原；flag 位声明启用了哪些步骤，保证「可还原」。
3. **帧类型**：`CmdRequest` / `CmdResponse` / `DataChunk` / `StreamPush` / `Ctrl`。
4. **全量指令定义**：`Auth/Ls/Download/Upload/Publish/Tail/Grep/Edit/Rollback/Delete/Copy/Move/Rename/Mkdir/Ping` 及各自请求/响应载荷结构体。
5. **分片传输**：`DataChunk`（index + data + chunk_sha256）、`ChunkAssembler`（按序合并 + 逐片 & 整文件 SHA256 校验 + 缺片查询）、`split_file` 切分器、断点续传位图（`AckBitmap`/`Query`）。
6. **错误码**：`ErrorCode`（认证/路径/冲突/校验/重启/中转等 10 项）。
7. **控制帧**：`Ping/Pong/Stop/Resend/AckBitmap/Query`。

## 验证结果
```
running 5 tests
test tests::frame_roundtrip_plain ... ok
test tests::response_builders ... ok
test tests::chunk_assembler_missing_and_bad_sha ... ok
test tests::frame_roundtrip_compressed_b64 ... ok
test tests::chunk_assembler_full_and_verify ... ok
test result: ok. 5 passed; 0 failed; 0 ignored
```

## 下一步：Phase 1 — rdep-service 核心文件能力
- TLS 监听（rustls）+ 协议分发骨架。
- 实现：`AUTH / LS / MKDIR(多级) / UPLOAD(纯上传) / DOWNLOAD / DELETE / COPY / MOVE / RENAME`。
- SQLite：用户 / 认证方式、项目 / 远程目录映射。
- 先用临时 CLI 脚本驱动自测，暂不依赖 GUI。

## 环境备注
- 本机经 **rsproxy 镜像** 安装 Rust（`static.rust-lang.org` 限速 ~30KB/s）；`/tmp` 是 10MB tmpfs，安装须 `export TMPDIR=/home/macro/tmp`（真实磁盘）。
