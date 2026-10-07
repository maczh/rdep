# Phase 4 概览 — TAIL / GREP / EDIT（实时日志 / 内容检索 / 在线编辑）

> 状态：✅ 完成并通过验证（协议单测 5/5，集成测试 3/3，全 workspace 编译无警告）

## 目标

补齐运维三大刚需：
- **TAIL**：实时查看远端日志尾部，支持 `tail -f` 跟随与主动停止；
- **GREP**：在远端文件/目录中检索关键字；
- **EDIT**：在线拉取并编辑远端文件，保存时自动备份。

## 协议（rdep-protocol）
- `EditRequest` 增加 `content: Option<String>`：
  - `None` = 读取（响应 body 即文件内容）；
  - `Some(text)` = 保存（服务端先备份再覆盖）。
- 复用既有 `TailRequest{path,lines,follow}`、`GrepRequest{path,pattern,flags}`、
  `GrepResponse{lines}`、`StreamPush{transfer_id,line,eof}`、`Ctrl::Stop`。

## 服务端（rdep-service）
### Storage
- `read_lines` / `tail_lines(path,n)`：读全部行 / 最后 n 行。
- `read_from_offset(path, offset)`：tail follow 增量读取，返回 `(新增字节, 新偏移)`；
  文件被截断时从头读（检测轮转）。
- `walk_files(path)`：递归列文件；`rel(abs)`：root 内绝对路径转相对路径。

### 会话（session.rs）
- **TAIL**：先推末尾 N 行（`StreamPush`）；`follow=true` 时进入轮询循环——
  增量读新行并推送，同时用 `timeout(200ms, read_frame)` 探测控制帧：
  - `Ctrl::Stop` → 结束；`Ping` → 回 `Pong`；其他指令 → 回「busy」并继续；
  - 连接关闭 / 读错 / 到达上限（3600s）→ 结束。
  结束时推 `eof` 标记 + `CmdResponse`，客户端据此收尾。
- **GREP**：文件或目录递归检索；`flags`：`i` 忽略大小写、`n` 显示行号；
  目录模式结果前缀 `相对路径:行号: 内容`。
- **EDIT**：`content=None` 读文件回 body；`content=Some` 走 `save_with_backup` 保存。

## 客户端（rdep-client）
- `client.rs`：
  - `Command::Tail{path,lines,follow}` / `Grep{path,pattern,flags}` /
    `EditGet{remote_path}` / `EditSave{remote_path,content}`。
  - `Event::TailLine{line}` / `TailDone{ok,message}` / `GrepResult{lines}` / `EditLoaded{content}`。
  - `do_tail`：follow 用 `Arc<AtomicBool>` 停止标志；GUI 调 `request_stop_tail()` 置位后，
    `do_tail` 发 `Ctrl::Stop` 并进入「排空」模式读到结束 `CmdResponse`，**保证会话不残留帧**。
  - `do_grep` / `do_edit_get` / `do_edit_save`；便捷方法 `tail/grep/edit_get/edit_save/request_stop_tail`。
- `app.rs`：顶栏加「日志/编辑」窗口——TAIL（路径/行数/跟随/开始/停止，实时输出）、
  GREP（路径/关键字/标志/搜索，结果列表）、EDIT（加载/保存，等宽多行编辑器）。

## 验证
| 项 | 命令 | 结果 |
|---|---|---|
| 协议单测 | `cargo test -p rdep-protocol` | 5/5 ✅ |
| 集成测试 | `cargo test -p rdep-client --no-default-features --test integration` | 3/3 ✅ |
| 全 workspace 编译 | `cargo build` | ✅ 无警告 |

新增 `tail_grep_edit_e2e` 覆盖：上传 5 行日志 → tail 末 3 行 → grep(带行号) 命中 →
edit 读取/保存(自动备份)/重读确认 → **follow tail 主动停止** → 验证停止后会话仍同步可继续用 ls。

## 踩坑
- `FrameCodec::write_frame` 收 `&Frame`，不能传值（`write_frame(ctrl_frame(..))` 要写 `&`）。
- tail follow 结束务必让客户端读到最终 `CmdResponse`，否则残留帧会让下一个指令错位。

## 下一步（Phase 5+）
- **Phase 5** rdep-forwarder（服务注册长连、客户端路由、零解析转发）。
- **Phase 6** Web 管理后台（service + forwarder，admin/admin）。
- **Phase 7** 部署产物（systemd / Dockerfile / docker-compose / Windows 安装脚本）。
