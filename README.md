# rdep

远程发布/部署工具集：把本地构建产物**安全地发布**到远端服务器，支持**自动备份、失败回滚、
实时日志（tail）、内容检索（grep）与在线编辑**；内网机器可通过 **forwarder 公网中转**被访问，
两端都带 **Web 管理后台**（默认 `admin/admin`）。

```
直连模式：
  rdep-client ──TLS──▶ rdep-service（目标服务器）

中转模式（service 在 NAT/内网）：
  rdep-client ──TLS──▶ rdep-forwarder ──TLS──▶ rdep-service
                            ▲
                            └── service 启动时主动注册长连接 ──┘
```

## 组件

| 组件 | 说明 | 形态 |
|------|------|------|
| `rdep-protocol` | 私有二进制协议（帧编解码 / 分片并发 / SHA 校验 / 断点续传） | 三端共用库 |
| `rdep-service` | 目标机服务：文件管理、备份/回滚、重启脚本、Tail/Grep/Edit、Web 管理 | headless |
| `rdep-forwarder` | 公网中转：service 注册长连接、client 会话路由、零解析透传、Web 管理 | headless |
| `rdep-client` | 桌面客户端（egui，FileZilla 风格）：快速连接条、**双栏目录树 + 文件列表**、传输队列（进行中/失败/成功 标签页）、发布/回滚、目录同步、tail/grep/编辑、站点管理、**rdep / FTP / SFTP 三协议**、多语言（英/简中/繁中，默认英文） | GUI |

### 核心能力
- **API 令牌认证**：可在 Web 后台为 CI/CD 签发令牌（`rdp_` 前缀，**明文只显示一次**），
  客户端勾选「使用 API 令牌认证」即可连接——流水线无需持有 admin 口令（改密不打断 CI）。
- **站点管理**：连接配置可命名保存到本地（`sites.json`），一键切换/删除；记住上次所在远端目录，重连即回到原处。
- **多协议**：`rdep`（自有协议，功能最全）/ `FTP`（基础文件操作，明文）/ `SFTP`（SSH，支持浏览/上传/下载/新建/删除/改名/目录同步/断点续传/tail/grep/编辑）。
- **多语言**：界面支持英文 / 简体中文 / 繁體中文，默认英文；顶栏可随时切换，选择持久化到 `rdep/ui.json`。新增文案用**英文作 key**，缺翻译回退英文，构建永不因漏翻而中断。
- **文件操作**：浏览/上传/下载/删除/复制/移动/改名/建目录，**权限位保留**（`+x` 不丢失）。
- **发布 / 回滚**：覆盖前自动备份（`backup/<版本>/`，默认留 10 个），收尾执行重启脚本，出错一键回滚。
- **项目即单一来源**：在 Web 后台登记项目（部署目录 + 重启脚本）后，客户端按**项目名**发布；服务端以项目记录为准，并**强制把上传文件落到项目目录之下**，客户端无法越出该目录。
- **目录同步**：本地目录 → 远端，rsync 风格大小+mtime 快筛，支持 dry-run 预览、删除远端多余文件、单文件失败不中断整批。
- **弱网韧性**：**断点续传**上传（服务端分片落盘，重试只补缺失片）、**自动重连**（探活后透明重连）、下载**流式落盘**+SHA-256 校验+原子改名。
- **运维**：tail 实时日志（可跟随/停止）、grep 内容检索、远端文件在线编辑。
- **公网中转**：service 主动注册到 forwarder，client 经其路由；同一 service 可并发服务多个 client。
  管理后台记录**每个 service 的中继审计**（客户端会话数、最后服务时间），可回答「谁在用这个 service」。

## 构建

```bash
# 开发（Rust stable）
cargo build                                   # 全部
cargo test -p rdep-protocol                   # 协议单测
cargo test -p rdep-client --no-default-features --test integration -- --test-threads=2
cargo test -p rdep-client --lib                              # 站点/FTP/GUI 冒烟（需 gui 特性）
cargo test -p rdep-client --no-default-features --test ftp_e2e -- --test-threads=1

# 发布产物（输出到 dist/）
./deploy/build-release.sh --server            # service + forwarder
./deploy/build-release.sh --client            # 交叉编译 client（win/mac/linux）
```

> GUI 依赖系统图形库（Linux 需 glib/gtk 等开发包）。无显示环境下用
> `--no-default-features` 仅构建网络核心。

## 快速开始

### 1) 直连模式

**服务端**（目标机器）：

```bash
# 准备证书（自签示例，正式环境用 certbot/内部 CA）
openssl req -x509 -newkey rsa:4096 -nodes -days 3650 \
  -keyout server.key -out server.crt -subj "/CN=localhost" \
  -addext "basicConstraints=CA:FALSE" -addext "extendedKeyUsage=serverAuth" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"

# 运行（默认账户 admin/admin）
RDEP_LISTEN=0.0.0.0:8443 \
RDEP_ROOT=./data/root RDEP_CERT=./server.crt RDEP_KEY=./server.key \
RDEP_SCRIPTS=./data/scripts RDEP_WEB_LISTEN=127.0.0.1:8080 \
cargo run --release -p rdep-service
```

**客户端**：

```bash
cargo run --release -p rdep-client
```

连接对话框填：主机 `127.0.0.1`、端口 `8443`、用户/密码 `admin/admin`、
**CA 证书**选 `server.crt`。即可浏览、拖拽上传、发布/回滚、tail 日志。

### 2) 中转模式（内网 service 被公网 client 访问）

**forwarder**（公网机器）：

```bash
RDEP_FWD_CLIENT_LISTEN=0.0.0.0:9443 \
RDEP_FWD_SERVICE_LISTEN=0.0.0.0:9444 \
RDEP_FWD_CERT=./server.crt RDEP_FWD_KEY=./server.key \
RDEP_RELAY_TOKEN=<强随机密钥> RDEP_FWD_WEB_LISTEN=0.0.0.0:8081 \
cargo run --release -p rdep-forwarder
```

**service**（内网机器，注册到 forwarder）：

```bash
RDEP_USE_FORWARDER=1 RDEP_FWD_HOST=forwarder.example.com RDEP_FWD_PORT=9444 \
RDEP_FWD_CA=./forwarder.crt RDEP_RELAY_TOKEN=<同一密钥> \
RDEP_SERVICE_ID=svc-1 RDEP_SERVICE_LABEL=prod-web \
RDEP_CERT=./server.crt RDEP_KEY=./server.key \
cargo run --release -p rdep-service
```

**client**：连接对话框勾选「经 forwarder 中转」，主机/端口填 forwarder 地址，
填「目标 service id」与「中转密钥」。

## 发布 / 回滚（核心工作流）

1. 在客户端「发布/回滚」窗口把本地文件加入**发布清单**并选择**重启脚本 ID**。
2. 点「执行发布」：服务端先备份被覆盖的旧文件到 `backup/<YYMMDDHHmm>/`，写入新文件，
   全部完成后执行 `sh <restart_script_id> <remote_dir>`。
3. 出问题：在同一窗口「列出备份版本」→ 选版本 → 「执行回滚」即恢复。
   也可在 Web 后台点回滚。

- 备份版本默认保留最近 `RDEP_BACKUP_KEEP=10` 个，超出自动剪枝。
- 重启脚本放在 `RDEP_SCRIPTS` 目录，文件名即 ID（支持 `<id>` 或 `<id>.sh`）。

## 连接排障（FAQ）

- **rdep 提示 `no CA cert configured`**：rdep 协议是 TLS 双端自签体系，客户端必须填
  **CA cert path**（服务端 `certs/server.crt`；服务端证书自带
  `DNS:localhost, IP:127.0.0.1` SAN，可直接用 `127.0.0.1` 连本机）。
- **rdep 提示 `tls handshake`**：TCP 通了但证书对不上。典型原因：
  ① 目标端口上跑的不是 rdep-service（如同机部署了 forwarder，8443 端口撞车——
  把 service 换端口 `RDEP_LISTEN=127.0.0.1:9443`）；
  ② CA cert path 填的不是该 service 实际使用的证书。
  客户端现在会输出**完整错误链**（如 `read CA cert: No such file…`、
  `invalid peer certificate: UnknownIssuer`），按提示核对即可。
- **service 默认账号**：`admin/admin`（首启自动引导）。
- **远程面板的 `/` 不是真实文件系统根目录**：rdep 协议是**沙箱化**的文件存储协议，
  远程 `/` 即 service 的 `RDEP_ROOT`（默认 `data/root`），`..` 无法越出该根（防越权）。
  要像 SFTP 一样浏览真实文件系统，以 `RDEP_ROOT=/` 启动 service 即可（已实测
  `ls /` 返回真实根目录；注意 `.rdep-staging`/`backup` 工作目录会建在 `/` 下）。
  服务端**不跟随符号链接**（防链接逃逸），因此 `bin -> usr/bin` 这类链接目录
  显示为文件，浏览 `usr` 即可看到相同内容。
- **Refresh 按钮点了没反应**：此前是残留的「目录树 ls」在途标记把主面板应答吞进了树缓存，
  以及未连接时静默 no-op。现在：出错/断连会清理该标记；未连接点 Refresh 会明确提示
  `Not connected; connect first`；刷新前先清标记，保证应答落在主面板。
- **SFTP 无任何反应 / 一直 connecting**：确认对端 22 端口是真实 sshd
  （`ssh-keyscan -T 5 <host>` 应返回 host key）；连接有 20s 超时，超时/认证失败
  都会在状态栏与日志给出明确报错。首次连接按 TOFU 记录主机指纹到
  `~/.config/rdep/known_hosts.json`，之后指纹变化会拒绝连接（防中间人）。

## 日志与排障（Debug 日志）

两端都默认输出 **debug 级**结构化日志（含完整入参、指令、接口与返回值），并支持 `RUST_LOG` 覆盖：

```bash
RUST_LOG=trace ./target/debug/rdep-service        # 服务端：stderr（systemd 会采集）
RUST_LOG=debug  ./target/debug/rdep-client        # 客户端：stderr + 日志文件
```

- **客户端日志同时写入文件**：`<配置目录>/rdep/logs/rdep-client.log`
  （Linux 即 `~/.config/rdep/logs/rdep-client.log`，>5MB 滚动为 `.log.old`）。
  GUI 从桌面启动时 stderr 不可见，**反馈问题请附这个文件**。
- **客户端 GUI 日志面板**每行带 `[HH:MM:SS]` 时间戳，可与服务端日志按时间对表。
- **口令/令牌一律脱敏**：`ConnectParams` 自定义 `Debug`，日志中显示为 `pass="***"`。

服务端关键日志点：启动时的**生效配置一行**（`root=` 即 `RDEP_ROOT` 解析结果，
回答「远程 `/` 到底是哪个目录」）→ accept/TLS → session 每帧 → 每条指令
（`auth/ls/upload/download/...` 的入参与结果）→ storage 层的 `requested → resolved` 路径解析。

客户端关键日志点：GUI 每个按钮动作（`action: upload/download/mkdir/...`）→
后台线程收到的每条 `Command` → 每次探活（`probe: ping timeout` / `read error`）→
协议层每条请求/应答（`send cmd request` / `recv cmd response`，带 seq、ok、body 长度）→
回传 GUI 的每个 `Event`。

典型故障读法：

- 客户端 `op failed: read stream` = **读帧失败**（对端已断连）。服务端日志里对应
  `session: frame read failed, closing: read stream: …`；最常见是连到了 forwarder
  但该 service 未注册/未配对（隧道被 forwarder 关闭），或 service 端口被别的进程占用。
- 客户端 `ls failed: …` 带完整错误链（`{e:#}`），服务端对应 `ls: request` /
  `storage.ls: done entries=N`，可确定「请求到了没、返回几条」。

## 断点续传 / 自动重连（弱网韧性）

- **断点续传**：上传分片先在服务端落盘暂存（`root/.rdep-staging/`），客户端用稳定的
  `transfer_id = hash(路径+内容)`；重试/断线重连时，init 会回传服务端已收分片，客户端
  **只补传缺失片**。大文件发布到弱网可断点恢复，而非从头再来。
- **自动重连**：客户端在每个操作前用 `PING/PONG` 探活；连接掉线时自动用上次参数重连并
  重新认证，随后重试的操作即可命中续传（无需手动重连）。
- **并发中转**：service 维持 `RDEP_MAX_SESSIONS`（默认 4）条常驻隧道，同一 service 可并发
  服务多个 client（经 forwarder 时）。

## Web 管理后台（默认 `admin/admin`）

- **service**（`RDEP_WEB_LISTEN`）：概览、用户/项目 CRUD、备份列表、回滚触发。
- **forwarder**（`RDEP_FWD_WEB_LISTEN`）：已注册 service 列表（在线状态 + 最后活动审计）。

浏览器打开对应端口即可；受保护接口需先 `POST /api/login` 换 Bearer token。

## 配置（环境变量）

**rdep-service**

| 变量 | 默认 | 说明 |
|---|---|---|
| `RDEP_LISTEN` | `0.0.0.0:8443` | rdep 协议监听 |
| `RDEP_ROOT` | `data/root` | 远程文件根目录 |
| `RDEP_DB` | `data/rdep.db` | SQLite |
| `RDEP_SCRIPTS` | `data/scripts` | 重启脚本目录 |
| `RDEP_CERT` / `RDEP_KEY` | `certs/server.crt` / `.key` | TLS |
| `RDEP_BACKUP_KEEP` | `10` | 备份保留数 |
| `RDEP_WEB_LISTEN` | 关闭 | Web 后台监听 |
| `RDEP_USE_FORWARDER` | `0` | 是否注册到 forwarder |
| `RDEP_FWD_HOST` / `RDEP_FWD_PORT` / `RDEP_FWD_CA` | — / `9444` / — | forwarder 地址与 CA |
| `RDEP_RELAY_TOKEN` | `rdep-relay-token` | 中转密钥（**务必改**） |
| `RDEP_SERVICE_ID` / `RDEP_SERVICE_LABEL` | — | 本 service 标识 / 标签 |

**rdep-forwarder**

| 变量 | 默认 | 说明 |
|---|---|---|
| `RDEP_FWD_CLIENT_LISTEN` | `0.0.0.0:9443` | client 会话端口 |
| `RDEP_FWD_SERVICE_LISTEN` | `0.0.0.0:9444` | service 注册端口 |
| `RDEP_FWD_CERT` / `RDEP_FWD_KEY` | `certs/server.crt` / `.key` | TLS |
| `RDEP_FWD_DB` | `data/forwarder.db` | SQLite |
| `RDEP_RELAY_TOKEN` | `rdep-relay-token` | 中转密钥（**务必改**） |
| `RDEP_FWD_WEB_LISTEN` | 关闭 | Web 后台监听 |

## 部署

见 [`deploy/`](deploy/)：

- **systemd**：`deploy/systemd/rdep-{service,forwarder}.service`（含安全加固）。
- **Docker**：`deploy/Dockerfile` + `deploy/docker-compose.yml`（先在 `.env` 设 `RELAY_TOKEN`）。
- **Windows**：`deploy/windows/install.ps1` / `install.bat`（经 NSSM 注册为 Windows 服务）。
- **证书**：`deploy/certs/README.md`（自签 / 正式证书说明）。

## 文档

- 架构：`docs/00-architecture.md`；协议：`docs/01-protocol.md`；路线图：`docs/03-roadmap.md`。
- 各阶段交付说明：`docs/phase{0..6}-overview.md`。

## 协议支持与能力边界

| 能力 | rdep | FTP | SFTP |
|---|---|---|---|
| 浏览/上传/下载/新建/删除/改名 | ✅ | ✅ | ✅ |
| 传输加密 | ✅ TLS | ❌ **明文** | ✅ SSH |
| 发布（备份+覆盖+重启脚本） | ✅ | ❌ | ❌ |
| 一键回滚 | ✅ | ❌ | ❌ |
| 目录同步（含预览/删除多余） | ✅ | ❌ | ✅ |
| 断点续传 | ✅ | ❌ | ✅ |
| tail / grep / 远程编辑 | ✅ | ❌ | ✅ |
| forwarder 公网中转 | ✅ | ❌ | ❌ |

`SFTP` 基于纯 Rust 的 `russh` + `russh-sftp`，首次连接按**指纹信任（TOFU）**记录主机密钥到
`known_hosts.json`，指纹不匹配（疑似中间人）直接拒绝。GUI 把**发布/回滚**（仅 rdep 专属）显式
禁用并说明原因，高级文件能力（同步/tail/grep/编辑/续传）则对 rdep 与 SFTP 同时开放。
FTP 仍为**明文传输**，且不具备上述高级能力；GUI 在选择 FTP 站点时会**显式禁用**并说明原因
（不静默失败）。公网传输请优先使用 rdep（可经 forwarder 中转）或 SFTP。

> **中文显示（tofu/方块）修复**：GUI 启动时调用 `fonts::install_cjk_fonts()`，
> 按常见路径扫描系统中文字体（如 `Noto Sans CJK`、`WenQuanYi`、`Microsoft YaHei`、
> `~/.local/share/fonts` 等），注入 egui 的 `FontDefinitions` 兜底链；找不到时静默跳过，
> 不影响英文界面。deepin 等缺失内置中文字体的环境因此可正常显示中文。

### 测试覆盖

| 套件 | 数量 | 覆盖 |
|---|---|---|
| 协议单测 | 5 | 帧编解码、分片、校验、续传位图 |
| service 单测 | 17 | 暂存 GC、符号链接安全、**口令散列/认证迁移**、**路径穿越/注入对抗测试**（12 种穿越写法、回滚源路径越权、版本号注入、合法路径不误伤）、Web 令牌随机性 |
| forwarder 单测 | 9 | 口令散列（5）、Web 会话令牌 256bit 随机性、会话上限、**中继审计计数**、老库迁移补列 |
| client 单测 | 26 | 站点持久化/混淆、**FTP** 列表解析/转义/时间/参数映射、**SFTP** 命令拼接/参数映射/known_hosts/引号转义、**i18n** 查表与回退/持久化/语言码、**字体**扫描 |
| GUI 冒烟 | 7 | 无显示环境下用 `egui::Context::run()` 跑完整渲染路径：全窗口打开、边界数据（`..`/含空格名/空列表/传输三态）、协议门控、路径拼接、**SFTP/FTP 后端双栏渲染** |
| rdep 集成 | 18 | 直连、发布回滚、目录同步、续传、下载完整性、权限、tail/grep/edit、中转发布、并发中转、双 Web 后台、站点驱动连接、**认证闸门 + 暴力破解节流**、**按项目发布（配置单一来源）**、控制帧向前兼容、**Web 破坏性端点**、**API 令牌认证** |
| FTP 端到端 | 2 | 对**进程内最小 FTP 服务器**跑通登录/PASV/MLSD/STOR/RETR/改名/删除/含空格文件名 |

FTP 后端无需外部 FTP 服务即可验证：`tests/ftp_e2e.rs` 内置了一个只实现所需命令子集的
FTP 服务端，其 MLSD 输出刻意采用真实服务器格式（分号字段、空格转义为 `\\040`），
以确保校验的是解析器而非它自己的输入格式。

> GUI 说明：`app.rs` 约 1300 行界面逻辑通过 `RdepApp::new_headless()` /
> `update_ui()` 与 eframe 解耦，可在无 `DISPLAY` 环境用 egui 的 `Context::run()` 跑完整渲染
> 路径做冒烟测试（`cargo test -p rdep-client --lib`）。

## 安全

- **路径边界**：所有文件操作经 `Storage::resolve()`，拒绝任何 `..`，并二次校验
  结果仍在 root 内。`rollback` 的备份源路径同样经 `resolve()` 后再拼接。
- **备份保留**：`RDEP_BACKUP_KEEP`（默认 10）自动剪枝最旧版本。
- **断点续传暂存**：`.rdep-staging/` 由 `RDEP_STAGING_TTL_HOURS`（默认 24h）TTL 回收。
- **符号链接**：不跟随链接进入目录（防环），写入不穿链接。
- **符号链接与路径穿越有专门对抗测试**：`cargo test -p rdep-service --lib`（`storage::security_tests`）。
- **认证闸门**：除 `AUTH` 外所有指令都必须先认证；`DataChunk` 帧在认证前因不存在活跃传输而自动失效（有专门测试验证未认证请求无任何副作用）。
- **口令存储**：PBKDF2-HMAC-SHA256（随机 16 字节盐），格式 `pbkdf2-sha256$<rounds>$<salt>$<hash>`；比较为常量时间。
  默认 100k 轮（release ≈50ms），**生产建议上调** `RDEP_PBKDF2_ROUNDS=600000`（OWASP 建议值）。
  升级前创建的裸 SHA-256 口令仍可登录，并在**登录成功时自动升级**，无需强制改密。
- **优雅关闭**：service 与 forwarder 均收到 SIGTERM/SIGINT 后停止接受新连接并让在途会话/中转自然结束，
  避免 systemd `restart` 时把进行中的传输硬生生打断。
- **暴力破解节流**：同一连接连续认证失败 5 次后拒绝继续尝试（需重连），并逐次递增延时。局限：按连接计数，重连即重置——只能抬高单连接爆破成本，跨连接/分布式爆破需额外的账号级限流。

- **Web 后台会话安全**：`admin/admin` 登录后签发 **256 bit CSPRNG 令牌**（`getrandom`，hex 编码），有效期 1 小时；所有 `/api/*` 均需 `Authorization: Bearer`；同时在线会话上限 64，登录路径即清理过期会话。**务必修改默认口令**。
- **审计修复**：会话令牌早期实现为 `SHA256(counter:nanos:pid)`（三者均可推断，可被离线枚举），已改为操作系统随机源；`rollback` 曾可被构造路径读取 root 之外的目录，已要求备份源经 `resolve()` 校验。

> **中继审计语义**：后台列表中「最后注册」= service 最近一次向 forwarder 注册的时间；
> 「客户端会话」= 该 service 累计被中继服务的客户端连接次数；「最后服务」= 最近一次服务客户端的时间。
> 连接池下 service 长期空闲时前两者不会刷新（连接仍停放，属正常），在线状态以注册表为准。


> **优雅关闭**：service / forwarder 都监听 SIGTERM 与 SIGINT。forwarder 的关闭信号用 `watch` 广播
> 给两个 accept 循环（service 注册口 + client 接入口）与主流程，主流程收到后退出。
