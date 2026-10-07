# Phase 5 概览 — rdep-forwarder 公网中转

> 状态：✅ 完成并通过验证（协议单测 5/5，集成测试 4/4，全 workspace 编译无警告）

## 目标

让**内网/NAT 中的 rdep-service** 能被**公网的 rdep-client** 访问：service 启动后主动向
forwarder 拨号注册一条常驻隧道，client 连 forwarder 并指定目标 service，forwarder 把两者
**零解析字节透传**打通，client 无需直连内网。

至此 workspace 含 4 个 crate：`rdep-protocol` / `rdep-service` / `rdep-forwarder` / `rdep-client`。

## 协议（rdep-protocol::relay）
握手只发生在**连接第一帧**（复用 rdep Frame 编解码），握手成功后 forwarder 不再解析 rdep 内容：
- `RelayHello { service_id, label, token }`（service → forwarder，注册）
- `RelayHelloAck { ok, message }`
- `RelayConnect { target_service_id, token }`（client → forwarder，请求路由）
- `RelayConnectResp { ok, code, message }`（ok 后连接转字节透传）

## forwarder（rdep-forwarder，新增 crate）
- **config**（全 env）：`RDEP_FWD_SERVICE_LISTEN`(9444) / `RDEP_FWD_CLIENT_LISTEN`(9443) /
  `RDEP_FWD_CERT` / `RDEP_FWD_KEY` / `RDEP_FWD_DB` / `RDEP_RELAY_TOKEN`。
- **db**（SQLite）：`services`(id/label/last_seen 审计) + `users`(默认 `admin/admin`)。
- **registry**：`Mutex<HashMap<service_id, TlsStream>>`——存放已注册 service 的「停放」隧道；
  `check_token` 校验中转密钥。
- **relay**（两个 accept 循环，均 TLS）：
  - **service 端口**：读 `RelayHello` → 校验 token → 回 Ack → 记录 DB → 把隧道 `register` 停放。
  - **client 端口**：读 `RelayConnect` → 校验 token → `take` 目标 service 隧道 →
    回 `RelayConnectResp{ok}` → `copy_bidirectional` **零解析双向透传**；未找到回 `5001`。

## service（rdep-service）
- 新增 `registry.rs`：`register_loop` 拨号 forwarder → TLS → `RelayHello` → 把流交给
  `session::handle`（parked 等待某个 client 会话）→ 会话结束断线，3s 后自动重连再注册。
- `ServiceConfig` 新增 `use_forwarder/forwarder_host/forwarder_port/forwarder_ca/relay_token/service_id/service_label`。
- `run_service` 在 `use_forwarder=true` 时 spawn 注册任务（不占用直连监听端口）。

## client（rdep-client）
- `ConnectParams` 新增 `use_forwarder/target_service_id/relay_token`。
- `do_connect`：中转模式下先发 `RelayConnect` 读 `RelayConnectResp`（失败即报错），
  再在同一连接上跑 rdep `Auth` 与后续全部指令。
- GUI 连接对话框新增「经 forwarder 中转」勾选 + 目标 service id + 中转密钥。

## 验证
| 项 | 命令 | 结果 |
|---|---|---|
| 协议单测 | `cargo test -p rdep-protocol` | 5/5 ✅ |
| 集成测试 | `cargo test -p rdep-client --no-default-features --test integration` | 4/4 ✅ |
| 全 workspace 编译 | `cargo build` | ✅ 无警告 |

新增 `relay_via_forwarder_publish_e2e`（Phase 5 验收）：起 forwarder + 注册到它的 service，
client 经 forwarder 完成 `mkdir → upload(v0) → publish(v1，备份+restart) → download`，
校验下载内容为 v1——证明 rdep 协议可端到端经公网中转打通。

## 设计要点 / 已知限制
- **连接模型**：一条 service 隧道同时只服务**一个** client 会话（串行）；会话结束后隧道关闭，
  service 侧自动重连再注册。对部署工具（并发少）足够；如需并发可扩展为「信令长连接 + 每会话独立连接」。
- **安全**：forwarder 与 service/client 之间均为 TLS；握手校验共享中转密钥；
  配对成功后 forwarder 只做字节透传，不解析 rdep 协议，缩小攻击面。

## 踩坑
- 注册表存具体 `TlsStream<TcpStream>`，故 service/client 连接 handler 不能用泛型流（要用具体类型）。
- `tracing_subscriber` 的 `EnvFilter` 需开启 `env-filter` feature。
- relay 消息在 `rdep_protocol::relay` 子模块，不在 crate 根。

## 下一步（Phase 6+）
- **Phase 6** Web 管理后台（service + forwarder，admin/admin；forwarder 侧展示在线 service 列表/审计）。
- **Phase 7** 部署产物（systemd / Dockerfile / docker-compose / Windows 安装脚本）。
