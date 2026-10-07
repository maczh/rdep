# rdep 实施路线图（分阶段）

> 原则：先协议后业务、先 service 后 client、中转与 Web 管理后置。每阶段可独立验证。
>
> **状态：Phase 0–7 已全部完成并验证。** 每阶段交付说明见 `docs/phase{N}-overview.md`；
> 快速上手见根目录 `README.md`，部署产物见 `deploy/`。

## Phase 0 — 地基：workspace + rdep-protocol
- 初始化 Cargo workspace、rust-toolchain、.gitignore。
- 实现 `rdep-protocol`：帧编解码、postcard+zstd(+base64) 管线、CmdType 全量定义、分片/校验/断点续传结构、错误码。
- 单元自测：编码→解码还原、分片合并校验、base64 开关。
- **产出**：可单测通过的协议库，三端共用基础。
- **验证**：`cargo test -p rdep-protocol` 全绿。

## Phase 1 — rdep-service 核心文件能力（CLI 先行）
- TLS 监听（rustls），协议分发骨架。
- 实现：AUTH、LS、MKDIR(多级)、UPLOAD(纯上传)、DOWNLOAD、DELETE、COPY、MOVE、RENAME。
- SQLite：用户/认证方式、项目/远程目录映射。
- 先以简单 CLI 自测脚本驱动（暂不 GUI）。
- **产出**：service 能完成基础文件管理。
- **验证**：用临时测试 client（或脚本）跑通上述指令。

## Phase 2 — rdep-client 基础 UI + 连接
- egui 双栏：本地树/列表、远程树/列表、传输列表、站点管理。
- 通过 rdep-protocol 连接 service，接通 Phase 1 的全部指令。
- 本地文件浏览用异步线程，远程用协议。
- **产出**：可用 GUI 做站点管理 + 基础文件传迁。
- **验证**：连 Phase 1 service，完成一次上传/下载/改名/删除。

## Phase 3 — 发布 / 回滚 全链路
- service：PUBLISH（建 `backup/<ts>/`、先备份后覆盖、调重启脚本、最大备份 prune）、ROLLBACK（恢复 + 重启）。
- client：发布列表 UI、重启脚本配置页、历史版本选择回滚。
- **产出**：核心差异化能力可用。
- **验证**：发布→改文件→发布→回滚到旧版本，服务按预期重启。

## Phase 4 — 运维增强：TAIL / GREP / EDIT
- service：TAIL 流推送（限流、STOP）、GREP、EDIT（下载临时→保存覆盖并自动备份）。
- client：tail 窗口（实时刷新）、grep 结果页、内置文本编辑器。
- **验证**：tail 实时性、grep 结果正确、edit 保存后远程变更 + 备份生成。

## Phase 5 — rdep-forwarder 中转
- forwarder：service 注册长连接 + 断线重连、client 会话路由、零解析透传、僵尸回收。
- service：启动向 forwarder 注册（可配 ip/端口、自动重连）。
- client：中转开关 + forwarder 地址，端到端经公网中转打通 Phase 1–4。
- **验证**：client 经 forwarder 连内网 service 完成一次发布。

## Phase 6 — Web 管理界面
- service Web：admin/admin 登录、用户/项目/备份/重启脚本管理、备份列表与回滚触发。
- forwarder Web：admin/admin 登录、注册 service 列表、流量/审计查看。
- **产出**：无 GUI 也能后台管理。

## Phase 7 — 部署与交付
- systemd unit、Dockerfile、docker-compose、Windows install.ps1/bat、Win 服务注册。
- 发布构建脚本（多平台 client 打包）。
- 用户文档 / 快速上手。

## 依赖与里程碑建议
```
Phase0 ─► Phase1 ─► Phase2 ─► Phase3 ─► Phase4
                       │
                       └─► Phase5(forwarder) ─► Phase6(Web) ─► Phase7(部署)
```
- 最短可用路径（MVP）：Phase 0→1→2→3（直连即可发布/回滚）。
- 公网场景追加：Phase 5。
- 产品化追加：Phase 4、6、7。
