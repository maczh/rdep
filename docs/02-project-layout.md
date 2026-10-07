# rdep 工程结构与构建布局

采用 **Cargo workspace** 单仓多 crate，共享一套协议与通用库，三端各自独立构建出可执行文件。

## 1. 目录结构

```
rdep/
├── Cargo.toml                  # workspace 根
├── rust-toolchain.toml         # 锁定工具链（建议 stable 1.80+）
├── .gitignore
├── docs/                       # 设计文档（本目录）
├── crates/
│   ├── rdep-protocol/          # ★ 三端共用：帧/编解码/加密/分片/指令定义
│   │   └── src/lib.rs
│   ├── rdep-common/            # 配置、错误、日志、路径工具（三端共用）
│   │   └── src/lib.rs
│   ├── rdep-client/            # GUI 客户端（egui/eframe）
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── gui/            # 双栏树/列表/传输队列/设置页
│   │   │   ├── session/        # 协议连接、传输管理
│   │   │   └── editor/         # 内置文本编辑器（Edit 功能）
│   │   └── Cargo.toml
│   ├── rdep-service/           # 服务（headless + Web 管理）
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── server/         # TLS 监听、协议分发
│   │   │   ├── fs/             # 文件读写、备份、回滚
│   │   │   ├── exec/           # 重启脚本执行（sh/ps1/bat/systemd/docker）
│   │   │   ├── registry/       # 向 forwarder 注册长连接
│   │   │   ├── db/             # SQLite：用户/项目/备份元数据
│   │   │   └── web/            # axum REST + 静态 SPA
│   │   └── Cargo.toml
│   └── rdep-forwarder/         # 公网中转
│       ├── src/
│       │   ├── main.rs
│       │   ├── relay/         # 会话路由、零解析透传
│       │   ├── registry/      # service 注册表、僵尸回收
│       │   ├── db/            # SQLite 审计
│       │   └── web/           # axum 管理界面
│       └── Cargo.toml
└── deploy/                     # 部署产物模板
    ├── rdep-service.service    # systemd unit
    ├── Dockerfile.service
    ├── docker-compose.yml
    ├── install.ps1             # Windows 安装/服务注册
    └── install.bat
```

## 2. 依赖边界

- `rdep-protocol` 不依赖任何 IO 运行时，纯类型 + 编解码，便于三端复用与单测。
- `rdep-common` 提供配置读取（TOML）、统一错误类型、结构化日志（tracing）。
- client 仅依赖 protocol + common；service / forwarder 额外依赖 tokio / rustls / axum / rusqlite。

## 3. 构建目标

| 产物 | 命令 | 目标平台 |
|------|------|----------|
| client | `cargo build -p rdep-client --release` | Windows x64 / macOS universal / Linux x64 |
| service | `cargo build -p rdep-service --release` | Linux x64 / Windows Server x64 |
| forwarder | `cargo build -p rdep-forwarder --release` | Linux x64 |

> GUI 方案若后续改用 `tauri`，client crate 结构会调整为前端 + Rust 壳，但协议层不变。本设计以 egui 为默认。

## 4. 配置样例（client site）

```toml
# rdep-client 站点配置
[site."prod-web"]
host = "10.0.0.12"          # 可内网 IP
port = 8443
protocol = "rdep"           # ftp | sftp | rdep
use_forwarder = true
forwarder_host = "forwarder.example.com"
forwarder_port = 9443
user = "deploy"
pass = "********"
local_root = "D:/projects/web/dist"
remote_root = "/opt/app/web"
```
