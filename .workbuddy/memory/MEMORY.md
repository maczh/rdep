# rdep 项目长期记忆

## 项目概况
- rdep = 远程同步部署系统，Rust 三端：**rdep-client**（GUI 仿 FileZilla，egui/eframe）、**rdep-service**（headless + Web 管理）、**rdep-forwarder**（公网中转 relay）。
- 自定义私有协议 `rdep-protocol`：TLS + postcard + zstd(+base64) + 分片并发 + 断点续传。
- 设计文档见 `docs/`：`00-architecture` / `01-protocol` / `02-project-layout` / `03-roadmap`。

## 技术选型（已定）
- 异步 tokio；TLS rustls；序列化 postcard（`alloc` feature）；压缩 zstd；哈希 sha2；client GUI = egui/eframe。
- service / forwarder Web = axum；DB = rusqlite / sqlx-sqlite。

## 实施路线
- Phase 0（协议地基，**已完成**）→ 1(service 核心文件能力) → 2(client UI) → 3(发布/回滚全链路) → 4(tail/grep/edit) → 5(forwarder 中转) → 6(Web 管理) → 7(部署产物)。

## 约定
- 三端共用 `crates/rdep-protocol`，该 crate **不依赖异步运行时**，纯类型 + 编解码，便于单测与复用。
- 工程为 Cargo workspace（resolver=2），新 crate 加入 `Cargo.toml` members。
