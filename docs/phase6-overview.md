# Phase 6 概览 — Web 管理后台（service + forwarder）

> 状态：✅ 完成并通过验证（协议单测 5/5，集成测试 6/6，全 workspace 编译无警告）

## 目标

无 GUI 也能后台管理：
- **service Web**（默认 `admin/admin`）：登录、概览、用户/项目/备份管理、**回滚触发**。
- **forwarder Web**（默认 `admin/admin`）：登录、**已注册 service 列表**（在线状态 + 审计）。
- 技术栈：**axum 0.7**（HTTP 服务），前后端一体（内嵌单页 HTML/JS，无需构建）。

## service Web（rdep-service::web）
- `WebState { db, storage, sessions }`（`Arc<Mutex<..>>`，`#[derive(Clone)]`——axum 要求 state 可 Clone）。
- **认证**：`POST /api/login {username,password}` → 校验 DB 后签发随机 token（内存保存、1h 过期）；
  其余接口需 `Authorization: Bearer <token>`，否则 401。
- **接口**：
  - `GET /api/status`：概览（用户数 / 项目数 / 备份版本数）。
  - `GET|POST /api/users`、`DELETE /api/users/:id`：用户 CRUD（管理后台账户）。
  - `GET|POST /api/projects`、`DELETE /api/projects/:id`：项目 CRUD。
  - `GET /api/backups`：备份版本列表。
  - `POST /api/rollback {remote_dir,version}`：**后台直接触发回滚**。
  - `GET /`：内嵌单页管理界面（登录 + 概览 + 用户 + 项目 + 备份回滚分区）。
- **启用**：`ServiceConfig.web_listen`（env `RDEP_WEB_LISTEN`）非空时由 `run_service` 拉起，默认不启用。
- Db 增补 `list_users/create_user/delete_user/list_projects/create_project/delete_project`。

## forwarder Web（rdep-forwarder::web）
- `WebState { db, registry, sessions }`（同样 Clone 化）。
- **接口**：
  - `POST /api/login`：admin/admin 换 token。
  - `GET /api/services`：合并 **DB 审计记录**（id/label/last_seen）与 **注册表在线状态**，返回
    `{services:[{id,label,last_seen,online}], online_count}`。
  - `GET /`：内嵌单页（service 表格 + 在线/离线高亮，5s 自动刷新）。
- **启用**：`ForwarderConfig.web_listen`（env `RDEP_FWD_WEB_LISTEN`），由 `run_forwarder` 拉起。

## 验证
| 项 | 命令 | 结果 |
|---|---|---|
| 协议单测 | `cargo test -p rdep-protocol` | 5/5 ✅ |
| 集成测试 | `cargo test -p rdep-client --no-default-features --test integration -- --test-threads=2` | 6/6 ✅ |
| 全 workspace 编译 | `cargo build` | ✅ 无警告 |

新增：
- `service_web_admin_e2e`：未认证 401 → 错误密码 401 → admin/admin 登录 → 首页 → 概览 → 用户 CRUD → 项目 CRUD → 备份列表。
- `forwarder_web_services_e2e`：forwarder(带 web) + 注册 service → 未认证 401 → 登录 → service 列表含 `web-relay-svc` 且 `online:true`。
- 测试用极简 HTTP/1.1 客户端（`http_request` + `extract_token`）避免引入 reqwest。

## 踩坑
- **axum 要求路由 state 必须 `Clone`**：最初 `WebState` 内含 `Mutex` 不可 Clone，导致所有 handler 报
  “`Handler` bound 不满足”。解法：`sessions` 改 `Arc<Mutex<..>>` 并 `#[derive(Clone)]`。
  （用最小 axum 复现确认 State-only handler 本身无问题，差异在 Clone。）
- **测试线程耗尽**：6 个测试并行、每个 helper 起 multi-thread runtime（默认 12 worker）→ `os error 11`。
  解法：helper 统一 `.worker_threads(2)`，并以 `--test-threads=2` 跑集成测试。

## 下一步（Phase 7）
- 部署与交付：systemd unit、Dockerfile、docker-compose、Windows install.ps1/bat + Win 服务注册、
  发布构建脚本、用户文档/快速上手。
