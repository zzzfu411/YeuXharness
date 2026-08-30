# YeuX Harness 审计架构基线

审计日期：2026-08-30  
目标：`/Users/zfu/Documents/develop/YeuX/Harness`  
历史运行：未发现此前 `security-audit-skill/YeuX-Harness` 的 `findings.json`；这是第一次结构化安全审计。

**基线快照说明：**为保留审计起点，第 3、4 节中涉及本轮问题的负面状态描述采用 2026-08-30 修复前代码；当前工作树的修复、残余边界与最终结论以同目录 `REPORT.md`、`FINDINGS-DETAIL.md` 和源码为准。

## 1. 应用与成熟度

YeuX Harness 是面向个人高级用户的本地智能体平台，不是多租户 Web 服务。当前进程拓扑由 Rust 权威 daemon `yeuxd`、TypeScript 行式终端客户端 `yeux` 和尚未接入 daemon 的 TypeScript plugin host 组成。客户端优先连接 Unix socket，失败时启动 `yeuxd --stdio`。外部网络目前只有 daemon 到无凭据 OpenAI-compatible Chat Completions endpoint 的出站 HTTP(S)。

当前最准确的成熟度是：M0 主要骨架存在但未满足全部退出门槛；M1 已有 `client -> daemon -> one provider request -> ledger -> streamed events` 的无工具纵向切片；M2 只有 workspace/process/sandbox/policy/artifact 等孤立原语；M3-M5 主要是类型、descriptor 或路线图。当前不能完成真实的“读取仓库—调用工具—回灌模型—回答”任务。

可比基线是本地编码智能体而非传统 Web 应用：模型输出、项目文件、MCP 和插件均不可信；副作用必须经过不可绕过的 policy/approval/OS sandbox；凭据不进入模型、ledger 或普通子进程；replay 只重建投影，不重新执行外部动作。仓库将 Grok Build、Codex、Pi、DeepSeek Harness 和 Hermes 的固定提交作为 clean-room 设计参考。

## 2. 技术栈与关键入口

- Rust 1.98 / edition 2021、Tokio、Serde/Schemars、UUIDv7、Reqwest/rustls、Rusqlite bundled、rustix、walkdir、BLAKE3/SHA-256。
- TypeScript 5.9、Node 22、pnpm 9、Vitest；没有真正的 OpenTUI 依赖，当前 UI 是行式终端客户端。
- JSON-RPC 2.0 over newline-delimited UTF-8，默认单帧 8 MiB。
- SQLite WAL + `synchronous=FULL`；`events` append-only，Thread 内 `seq` 严格单调；command receipt 持久化完整响应。
- Rust 类型是协议真源，已提交 54 份 JSON Schema；TypeScript 类型仍为手工子集。

关键入口：

- daemon CLI：`crates/yeuxd/src/main.rs:8`
- daemon 配置、state、socket/stdio：`crates/yeuxd/src/server.rs:42`, `:203`, `:268`, `:277`
- JSON-RPC framing、初始化、幂等：`crates/yeuxd/src/server.rs:325`, `:459`
- 方法分派、workspace/thread/turn/job：`crates/yeuxd/src/commands.rs:31`
- 单请求 runner 与上下文构建：`crates/yeuxd/src/runner.rs:147`, `:501`
- SQLite ledger/projection：`crates/yeux-runtime/src/ledger.rs:210`, `:637`
- provider/SSE：`crates/yeux-runtime/src/provider.rs:125`, `:176`
- workspace 原语：`crates/yeux-runtime/src/workspace.rs:73`
- process/sandbox 原语：`crates/yeux-runtime/src/process.rs:90`, `crates/yeux-runtime/src/sandbox.rs:174`
- TUI transport/render/approval：`packages/tui/src/transport.ts:24`, `packages/tui/src/renderer.ts:14`, `packages/tui/src/prompter.ts:14`
- plugin manifest/host：`packages/plugin-host/src/manifest.ts:31`, `packages/plugin-host/src/plugin-host.ts:54`

## 3. 参与者与信任边界

### 本机用户和 JSON-RPC 客户端

`yeuxd` 以启动者 UID/GID 运行，不降权。stdio 依赖父子管道；Unix socket 绑定后为 `0600`，但连接后没有 token、peer UID/PID 或方法级授权。`initialize` 只协商版本，自报 `clientInfo` 不构成身份。任何能连接 socket 的同 UID 进程都可读取全部 Thread、调用 provider、修改 workspace trust、archive/interrupt Turn 和管理 Job。

**审计起始基线：**Linux 无 `XDG_RUNTIME_DIR` 时，TUI 默认连接可预测的 `${tmpdir()}/yeux-<uid>.sock`，连接前不验证 owner/mode/peer。共享 `/tmp` 中存在跨用户抢占和 rogue-daemon 冒充面；当前实现已改为私有每用户目录并做连接身份检查。

### 工作区和项目内容

项目内容按威胁模型是不可信输入。当前 daemon 只 canonicalize workspace root 并记录身份，没有把文件读入模型。runtime 的 read/list/search/apply_patch 拒绝绝对路径、`..`、叶 symlink 和多硬链接，但尚未接 RPC。中间目录 TOCTOU、无文件/遍历字节上限和非 dirfd-relative rename 仍存在。

`workspace/trust` 只要求客户端回传 `workspace/open` 已公开的 identity digest。该 digest 是 stale-identity guard，不是人类授权或不可伪造凭据；在项目配置、hooks、MCP 或插件接线前必须增加可信 UI/daemon 授权边界。

### 模型供应商

provider 接收完整 root-to-leaf user/assistant 历史；tools 当前固定为空，provider 返回 tool call 时 Turn 失败而不执行。URL 只检查 HTTP(S)、无嵌入凭据/query/fragment；**审计起始基线**没有累计流量/事件/文本预算，且没有 redirect/私网/metadata/DNS rebinding 治理。恶意 provider 明确不在 v1 威胁模型；当前异常响应资源上限已在 runtime 落地，网络治理仍在路线图中。

### 进程与沙箱

ProcessExecutor 尚未接 daemon。**审计起始基线**中调用者环境被施加到 sandbox launcher 本身，`LD_PRELOAD`、`LD_AUDIT`、`DYLD_*` 等未被拒绝；当前实现已让 production launcher 只接收最小固定环境，并在隔离建立后注入目标变量。`ProcessRequest` 还允许调用者直接请求 workspace 写和全网络；接线前必须把环境与 SandboxRequirement 改成 daemon policy 的产物。

### 插件

独立 plugin host 验证 manifest 形状、主 executable 的 SHA-256、requested/granted capability 子集，并使用 `shell:false`。但插件以同 UID、无 OS sandbox 运行；capability 只是字符串，插件可直接访问用户文件、网络、daemon socket 和 SQLite。可执行路径只做 lexical containment，hash 与 spawn 分离，Node 依赖文件也不在摘要内。README 已明确不应运行不可信插件。

## 4. 输入面与危险 sink

1. CLI/env：state dir、provider URL/model、token budgets、TUI socket/daemon/cwd/prompt；字段级上限有限。
2. JSON-RPC：8 MiB 单帧；没有连接数、请求速率、空闲超时、订阅数或写超时上限。
3. Ledger/import：SQL 使用绑定参数；public import 未验证协议/UUIDv7/typed payload；多数查询 materialize 全量数据。
4. Context：daemon 几乎每个命令重放全库；runner 再加载全投影和全谱系消息，没有实际 token/byte 裁剪。
5. Provider（审计起始基线）：HTTP error body 先完整读取再截断；SSE 单个 partial event 限 8 MiB，但总流、delta 数、tool index map 和累计 assistant content 无上限；当前实现已补齐累计上限。
6. Workspace：read 整文件、list 全树、search 读取全部文件；没有单文件、文件数、总字节、深度或时间预算。
7. Process（审计起始基线）：可执行文件/cwd/env/stdin/timeout/output limit；loader env 可在 sandbox 前生效，PGID 不能治理主动脱组后代。当前 launcher 环境边界已修复，主动脱组治理仍未完成。
8. Plugin：manifest、可执行文件、stdout/stderr、tool schema/input/notification；无沙箱和 effect enforcement。
9. Terminal（审计起始基线）：model delta、diagnostic、approval 文本、daemon/plugin stderr 原样写终端；交互模式不清理 C0/CSI/OSC 控制序列，JSONL 模式会转义。当前 TUI sink 已清理，plugin-host/直跑 daemon stderr 仍是独立边界。

## 5. 已有积极控制

- 当前未发现生产 `unsafe` 块；protocol/core/yeuxd library 显式 `forbid(unsafe_code)`。
- state、SQLite 和 socket 有私有权限；state 有单写者锁。
- 事件不可更新/删除；命令 ID 冲突和 sequence gap 被拒绝。
- 事件与 mutation command receipt 同事务提交。
- replay 是纯 projection；daemon 重启不重调 provider。
- provider 取消后的残余 delta 不落账。
- workspace 拒绝绝对路径、父路径、叶 symlink 与多硬链接，patch 绑定 base hash。
- sandbox 不可用时失败关闭；process 不默认继承环境并限制 retained output。
- TypeScript 与 Rust JSON-RPC 均有 8 MiB 正常帧上限；TypeScript 客户端另有 request timeout。
- npm 公告查询在 2026-08-30 对锁定的 109 个依赖报告 0 个已知漏洞；RustSec 扫描本轮未执行，保留为 G1 供应链门禁。

## 6. Phase 2 重点

1. Linux 默认 socket 抢占与 rogue daemon：跨用户 prompt/cwd 泄露、伪造事件和审批钓鱼。
2. 终端控制序列注入：模型/daemon/plugin 输出到交互终端的 source-to-sink。
3. ProcessExecutor pre-sandbox loader 环境注入和 caller-controlled sandbox 放宽。
4. workspace read/list/search 的内存、遍历和 TOCTOU 边界。
5. provider/ledger/context 的累计资源耗尽及重复 read receipt 放大。
6. command/state machine 并发、崩溃、replay、fork 和 cancellation 逻辑。
7. plugin executable symlink/TOCTOU/import coverage、无 sandbox 和孤儿进程。
8. policy/approval 双实现、不可伪造 prepared token、workspace trust 与 AgentId 真实性。
9. 协议 Rust/TS 漂移、尾帧、慢客户端和错误信息泄露。
10. 锁定依赖、许可证、CI、迁移、备份、SBOM 与发布完整性。
