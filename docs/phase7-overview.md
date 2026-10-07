# Phase 7 概览 — 部署与交付（收尾）

> 状态：✅ 完成并验证（`build-release.sh --server` 实测产出二进制；全 workspace 编译无警告、
> 协议单测 5/5、集成测试 6/6）

## 目标

把前三端产物变成**可直接上线**的交付物：systemd / Docker / Windows 服务 + 发布构建脚本 + 用户文档。
至此 **Phase 0–7 全部完成**。

## 交付物（`deploy/` + 根 README）

| 文件 | 说明 |
|---|---|
| `deploy/systemd/rdep-service.service` | systemd 单元，含 `NoNewPrivileges` / `ProtectSystem=strict` / `ReadWritePaths=/var/lib/rdep` 等加固；环境变量直连与中转两套示例 |
| `deploy/systemd/rdep-forwarder.service` | forwarder 单元（client 9443 / service 9444 双端口） |
| `deploy/Dockerfile` | 多阶段（`rust:1-slim-bookworm` → `debian:bookworm-slim`），`ARG BUILD_TARGET=service\|forwarder`，非 root 用户运行 |
| `deploy/docker-compose.yml` | forwarder + service-1 编排，证书/data 卷挂载，`RELAY_TOKEN` 经 `.env` 注入 |
| `deploy/certs/README.md` | 自签（`CA:FALSE`+`serverAuth`+SAN）与正式证书（certbot）说明 |
| `deploy/windows/install.ps1` | Windows 安装：拷二进制 → 备目录/证书 → **NSSM 注册为 Windows 服务** → 写环境变量 → 启动；支持 `-Uninstall` |
| `deploy/windows/install.bat` | 批处理包装（管理员 cmd 一键调用 ps1） |
| `deploy/build-release.sh` | 发布构建：`--server` / `--client`(交叉编译 win/mac/linux) / 默认，产物到 `dist/` |
| `README.md` | 架构、构建、直连+中转快速开始、发布/回滚工作流、Web 后台、**env 变量速查表**、部署索引 |

## 验证
- `./deploy/build-release.sh --server` 实测成功：产出 `dist/rdep-service`(12M) + `dist/rdep-forwarder`(11M)（optimized）。
- `docker-compose.yml` 经 YAML 解析校验通过（services: forwarder, service-1）。
- `build-release.sh` 通过 `bash -n` 语法检查。
- 全量回归：workspace 编译无警告、协议 5/5、集成 6/6。

## 安全默认
- Web 后台默认**仅监听 127.0.0.1**（`RDEP_WEB_LISTEN`/`RDEP_FWD_WEB_LISTEN` 需显式开启），远程访问建议走反向代理。
- `RDEP_RELAY_TOKEN` 默认值仅供开发，systemd/compose/文档均标注**务必改成强随机值**。
- 真实证书/私钥已加入 `.gitignore`（仅保留 `deploy/certs/README.md`）。

## 全项目完成情况回顾（Phase 0→7）
0. 协议地基 → 1. service 文件能力 → 2. client GUI（双栏/传输队列）→ 3. 发布/回滚（备份+重启脚本）
→ 4. tail/grep/edit → 5. forwarder 公网中转 → 6. Web 管理后台（service+forwarder）→ **7. 部署交付**。
