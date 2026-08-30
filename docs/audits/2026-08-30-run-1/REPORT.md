# YeuX Harness 安全审计报告

审计日期：2026-08-30  
目标：`/Users/zfu/Documents/develop/YeuX/Harness`  
范围：当前 M0/M1 工程基线，以及进入 M2 前必须固定的本地 IPC、终端、provider 与进程沙箱边界

## 执行摘要

本轮是该仓库第一次结构化安全审计。审计确认了两个 **MEDIUM** 问题：交互 TUI 会把不可信文本作为终端控制序列执行，以及无私有 runtime 目录时可预测的 Unix socket 可被另一普通本机账号预占并冒充 daemon。二者均已在当前工作树修复并加入回归测试。本轮没有发现可从当前产品入口触发的 HIGH 或 CRITICAL 问题。另有两项值得在发布前加固、但不满足“当前可利用漏洞”门槛的缺口也已处理：provider 累计资源预算，以及尚未接入 daemon 的 `ProcessExecutor` sandbox-launcher 环境边界。

当前代码仍是开发基线而不是可用于真实编码任务的发布版。特别是工具循环、统一审批/策略/沙箱管线、凭据代理和 plugin OS sandbox 尚未闭环；这些能力不得因为底层原语已经存在而被视为已交付。

## 基线与比较口径

YeuX Harness 是单用户、本地运行的编码智能体架构：Rust daemon 是唯一权威，TypeScript TUI 通过 stdio 或 Unix socket 连接；模型、项目、工具、MCP 和插件内容均按不可信输入处理。合理的同类基线是本地编码智能体，而不是多租户 Web 服务：

- 人类终端 sink 必须把模型和 daemon 文本当作数据，而不是终端命令；机器可读 JSONL 应保留原始协议值。
- Unix socket 即使不承担多租户身份系统，也应维持普通 Unix UID 的 owner-only IPC 边界。
- replay 不应重新执行 provider、工具或外部副作用。
- 进程和写文件能力只有在 policy、approval 与 OS sandbox 串成唯一执行路径后才能开放。

未发现此前同仓库的结构化 `findings.json`。单次审计不能覆盖全部路径；后续在 M1 工具循环和 M2 执行管线接通后应进行新的独立运行。

## 结果总览

| ID | 严重度 | 状态 | 标题 | 一句话影响 |
|---|---|---|---|---|
| YX-2026-001 | MEDIUM | 已修复 | 交互 TUI 终端控制序列注入 | 可影响 provider/daemon 输出的一方能够清屏、改标题或伪造可信界面文本 |
| YX-2026-002 | MEDIUM（条件性） | 已修复 | 可预测 Unix socket 的跨 UID endpoint 冒充 | 共享主机上的另一普通账号可在特定部署下截获 cwd/prompt 并伪造交互请求 |

## YX-2026-001：交互 TUI 终端控制序列注入

**严重度：MEDIUM**  
**CWE：CWE-150 — Improper Neutralization of Escape, Meta, or Control Sequences**

### 位置

- 基线 sink：`docs/audits/2026-08-30-run-1/evidence/renderer.pre-fix.ts:28-37`
- 基线诊断路径：`docs/audits/2026-08-30-run-1/evidence/renderer.pre-fix.ts:50-71`
- 基线审批/输入提示：`docs/audits/2026-08-30-run-1/evidence/prompter.pre-fix.ts:29-45`
- provider 文本入口：`docs/audits/2026-08-30-run-1/evidence/provider.pre-fix.rs:394-429`

### 具体攻击场景

受害者以交互模式使用一个可被攻击者影响的 OpenAI-compatible endpoint，例如远程明文 HTTP endpoint 遭到在途篡改，或 upstream 服务被攻陷。攻击者在 SSE `delta.content` 中返回 `ESC [ 2 J`、OSC title 等控制序列。基线 renderer 从 `model/event` 取出字符串后直接调用 terminal writer；诊断、失败文本和审批说明也存在同类 raw write。

可观察结果是终端清屏、标题改变、已有上下文被隐藏，或输出看起来像 YeuX 自己的审批/命令提示。此问题没有证明任意代码执行；实际影响是可信 UI 的视觉欺骗和终端状态修改，具体能力取决于终端实现与配置。

### 修复

当前实现新增 `packages/tui/src/terminal.ts`，在本轮识别的 **TUI 人类终端 sink** 去除 ANSI/OSC、C0/C1 和双向文本控制字符，并把孤立 `CR` 规范化为换行。模型 delta、诊断、审批、用户输入提示、TUI CLI error 和经 TUI 呈现的 daemon stderr 均调用该 sanitizer；`--jsonl` 继续通过 `JSON.stringify` 保留原始协议 payload。

关键修复位置：

- `packages/tui/src/terminal.ts:24-64`
- `packages/tui/src/renderer.ts:30-49`
- `packages/tui/src/prompter.ts:25-55`
- `packages/tui/src/app.ts:26-31`

回归测试：`packages/tui/test/terminal.test.ts:5-23`、`packages/tui/test/renderer.test.ts:64-110`、`packages/tui/test/prompter.test.ts:7-38`。

## YX-2026-002：可预测 Unix socket 的跨 UID endpoint 冒充

**严重度：MEDIUM（部署条件性）**  
**CWE：CWE-923 — Improper Restriction of Communication Channel to Intended Endpoints**

### 位置

- 基线默认路径：`docs/audits/2026-08-30-run-1/evidence/transport.pre-fix.ts:23-31`
- 基线连接逻辑：`docs/audits/2026-08-30-run-1/evidence/transport.pre-fix.ts:33-65`
- 基线 daemon 父目录处理：`docs/audits/2026-08-30-run-1/evidence/server.pre-fix-excerpt.rs:4-32`
- cwd/prompt 发送：`packages/tui/src/app.ts:167-195`、`packages/tui/src/app.ts:221-247`

### 具体攻击场景

在共享 Unix 主机上，受害者的 `YEUX_SOCKET` 与 `XDG_RUNTIME_DIR` 未设置，且 `os.tmpdir()` 是攻击者可写的共享目录，于是使用基线 fallback `/tmp/yeux-<uid>.sock`。另一普通账号在受害者启动 TUI 前绑定这个可预测路径，并给 socket 设置允许受害者连接的权限。基线客户端只要看到路径存在就尝试连接，并接受任何可达的 Unix-socket listener；它不验证 socket 或父目录的 owner、mode、类型、symlink、inode，也不验证 peer credentials。

只要攻击者实现形状正确的最小 JSON-RPC 响应使初始化继续，客户端在**新建 Thread 并提交 prompt**时会把 `workspace/open.path`（cwd）和 `turn/start.content`（prompt）发给 rogue endpoint；`--thread` 恢复路径本身不会发送 cwd。攻击者还可在非 JSONL 模式发送带完整合法 `params` 的 `approval/request` 或 `user/input`，并在用户实际作答时获得回答；这不等价于让合法 daemon 执行已批准动作。

适用条件是：另一 non-root 本机账号、共享目录 fallback 或不安全的显式 socket 路径、攻击者先绑定路径且 socket 允许受害者连接。获取 cwd/prompt 还要求受害者新建 Thread 并提交 prompt；获取伪造交互的回答要求非 JSONL 模式和用户实际作答。正确私有的 `XDG_RUNTIME_DIR` 会显著降低基线可利用性，因此严重度注明为条件性。

### 修复

当前 fallback 为 `${os.tmpdir()}/yeux-<euid>/yeuxd.sock`。daemon 要求直接父目录为当前 euid 所有的真实目录且 group/other 无权限，并把 socket 设为 `0600`；客户端在连接前后检查父目录和 socket 的类型、owner、mode、device 与 inode，不满足条件则失败关闭并使用私有 stdio runtime。

关键修复位置：

- `packages/tui/src/transport.ts:24-40`
- `packages/tui/src/transport.ts:60-103`
- `packages/tui/src/transport.ts:129-178`
- `crates/yeuxd/src/server.rs:277-301`
- `crates/yeuxd/src/server.rs:835-876`

回归测试：`packages/tui/test/transport.test.ts:20-102`、`crates/yeuxd/src/server.rs:1830-1867`。

### 残余边界

- root 和同 UID 进程仍在信任边界内；当前方案不是同 UID 进程间的密码学认证。
- 可预测的私有目录名仍可能被另一 UID 预先创建以造成失败关闭，但不能通过 owner 校验冒充服务。
- 对显式自定义路径只验证直接父目录；若其更高层祖先可被攻击者重命名，仍应使用受控 runtime 目录。长期可评估 peer credentials 或独立会话令牌。

## 已解决的加固项（不计入 confirmed findings）

### Provider 累计资源预算

基线只限制单个未终止 SSE buffer；完整错误响应会先读取到内存再截断，分隔良好的 SSE 总字节、事件数、累计文本和 tool-call index 状态没有应用级硬上限。攻击者需要控制已配置 provider，或能够篡改其响应；恶意 configured provider 在 v1 威胁模型中明确排除，因此本轮不把它抬升为 confirmed 漏洞。

当前 `crates/yeux-runtime/src/provider.rs` 已增加：8 KiB 非 2xx body、64 MiB SSE 总量、100,000 SSE 事件、100,000 model 事件、32 MiB 累计输出和 4,096 个 tool-call 状态上限，并为各上限提供稳定错误码与测试。这一修改同时覆盖异常 provider 的可靠性风险。

### Sandbox launcher 前的目标环境注入

基线 `ProcessExecutor` 对 `wrapped.executable` 清空环境后又直接施加调用方变量；当 wrapped executable 是 `sandbox-exec` 或 `bwrap` 时，loader 变量会先作用于 launcher。该实现缺陷真实存在，但当前 daemon 不注册进程工具，runner 的 provider request 也固定 `tools: []`，因此没有来自 TUI、JSON-RPC、模型或项目内容的生产可达入口。本轮将其归为 **M2 发布阻断项**，而不是当前漏洞。

当前实现让 production launcher 只接收固定最小 `PATH`；目标环境通过 `/usr/bin/env -i` 或 `bwrap --setenv` 在隔离建立后注入，并新增 `LD_*`/`DYLD_*` 回归测试。M2 接线时仍必须由 daemon policy 生成 `SandboxRequirement` 和环境，而不是接受不可信调用方自由放宽。

## 其他后续加固

1. 在 M1 工具循环中为 workspace read/list/search 增加文件数、单文件字节、总扫描字节、深度和时间预算。
2. 在 M2 使用 dirfd-relative/no-follow 文件发布，关闭中间目录替换和校验到 rename 的 TOCTOU。
3. 在 plugin host 接入 daemon 前增加 OS sandbox、完整包摘要、hash-to-exec 原子性、effect enforcement 和进程树清理。
4. 对工具网络使用统一代理并阻断 loopback/private/link-local/metadata、DNS rebinding 与代理变量绕过。
5. 为长期 Unix socket 运行增加真实 UDS 集成测试；当前 TypeScript owner/mode 测试使用 mocked `lstat`。
6. plugin host 直接运行时仍会把插件 stderr 写入宿主终端，直接前台运行 `yeuxd` 也有独立 stderr sink；在这些组件进入受支持的交互入口前，应复用等价 sanitizer 或明确只输出结构化日志。

## 积极安全模式

- 本轮未发现生产 `unsafe` 块；protocol/core/yeuxd library 显式 `forbid(unsafe_code)`。
- SQLite events append-only，Thread 内 `seq` 单调；mutation receipt 与事件同事务提交。
- replay 是纯 projection，重启不自动重调 provider；取消后的 provider 残余 delta 不落账。
- workspace 已拒绝绝对路径、`..`、叶 symlink、多硬链接和 stale base hash。
- process 默认不用 shell、清空继承环境、限制输出，并在 sandbox 不可用时失败关闭。
- JSON-RPC 单帧有 8 MiB 上限，TypeScript 请求有 timeout，daemon 对订阅 gap/背压给出明确诊断。

## 验证结果

最终工作树验证全部通过：

- Rust format：通过。
- Rust Clippy：workspace/all-targets，`-D warnings`，通过且 0 warning。
- Rust tests：**共 100 项**通过（runtime 61、daemon 24、core 8、protocol 6、golden trace 1）。
- 其中 schema 定向测试 2 项通过；提交的 54 份 schema 与 Rust 真源一致。
- 其中 golden trace 1 项通过，覆盖 lifecycle replay 与跨重启 command dedup。
- TypeScript typecheck：全部 workspace package 通过。
- TypeScript tests：36 项通过（protocol 9、TUI 23、plugin-host 4）。
- TypeScript build：全部 workspace package 通过。

三个 provider `httpmock` 用例需要绑定 `127.0.0.1:0`，默认受限测试环境拒绝监听；在允许 localhost bind 的本地测试环境中重跑后全部通过。
