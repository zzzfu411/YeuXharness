# YeuX Harness 安全审计架构摘要（Run 2）

审计日期：2026-08-31
目标：`/Users/zfu/Documents/develop/YeuX/Harness` 当前工作树（包括未提交 P1 改动）
比较基线：Codex、Grok Build、Pi、DeepSeek Harness、Hermes 等本地编码 Agent；不是多租户 Web 服务

## 应用、技术栈与部署

YeuX Harness 是单用户、本地运行的编码智能体平台。Rust `yeuxd` 是 ledger、provider、workspace、policy、approval 和未来副作用执行的唯一 authority；TypeScript TUI/JSONL 客户端通过 owner-only Unix socket 连接，失败时可启动私有 stdio daemon。项目文件、provider/model 输出、工具输出、MCP 与插件内容均视为不可信输入。root、同 UID 进程和用户主动配置的恶意 provider 属于 v1 明示信任边界。

Rust workspace 使用 Rust 1.98、Tokio、Serde/Schemars、Reqwest/rustls、Rusqlite bundled、rustix 与 BLAKE3。`yeux-protocol` 定义 wire/schema，`yeux-core` 定义状态机、policy、approval 与纯 projection，`yeux-runtime` 提供 SQLite、workspace、provider、process、sandbox、artifact，`yeuxd` 组合 daemon authority。TypeScript 使用 Node.js 22、pnpm、TypeScript 5.9 和 Vitest；`@yeux/protocol` 提供 JSON-RPC client，`@yeux/tui` 提供终端投影和交互，`@yeux/plugin-host` 仍是未接入 authority 的实验宿主。

关键入口：

- daemon/transport：`crates/yeuxd/src/main.rs`、`crates/yeuxd/src/server.rs`
- command surface：`crates/yeuxd/src/commands.rs`
- provider/tool loop：`crates/yeuxd/src/runner.rs`
- sealed registry 原型：`crates/yeuxd/src/tools.rs`
- capability layers：`crates/yeuxd/src/grants.rs`（当前尚未纳入 crate module graph）
- ledger/replay：`crates/yeux-runtime/src/ledger.rs`、`crates/yeux-core/src/projection.rs`
- workspace read/patch：`crates/yeux-runtime/src/workspace.rs`、`crates/yeux-runtime/src/workspace_tools.rs`
- process/sandbox：`crates/yeux-runtime/src/process.rs`、`crates/yeux-runtime/src/sandbox.rs`
- provider SSE：`crates/yeux-runtime/src/provider.rs`
- TUI server request/UI：`packages/protocol/src/json-rpc-client.ts`、`packages/tui/src/app.ts`、`packages/tui/src/prompter.ts`

## 信任模型与输入面

主要 actor 为人类用户、TUI/JSONL 客户端、daemon、provider、workspace 内容、未来工具/插件/MCP。当前真实输入面包括 CLI/env、stdio/Unix-socket JSON-RPC、8 MiB 内的 prompt/command、provider SSE/tool-call fragments、workspace 路径与文件内容、SQLite 中的历史事件、TUI server-request response。危险 sink 包括 SQLite append/projection、provider HTTP、终端、人类审批、workspace 文件写和进程启动。

当前已形成的正向边界包括：私有 socket owner/mode/type/inode 校验；TUI human sink 控制字符清理且 JSONL 保留原值；provider SSE、tool-call JSON、workspace 文件/遍历/输出有硬预算；workspace 拒绝绝对路径、`..`、symlink 与多硬链接；policy 对 unresolved scope 和能力扩张失败关闭；replay 不调用 provider/tool/network。

## 相比 Run 1 的新增攻击面

Run 1（`docs/audits/2026-08-30-run-1`）确认并关闭了两个 MEDIUM：交互 TUI 控制序列注入、条件性的跨 UID Unix-socket endpoint 冒充。本轮应跳过重复发现，集中验证 P0 tool loop 与 P1 authority 新代码。

当前 daemon 已向支持 tool calls 的 provider 发布 `workspace.list/read/search`，模型生成的路径/query/JSON 会进入真实 workspace 工具并作为 ToolResult 回灌模型。仓库内容因此首次进入 provider 与 ledger；并发、取消、结果预算、重启恢复与 prompt injection 成为真实面。协议增加 tool version、args/effect digest、idempotency、Unknown/reconciliation；协议 major 已升为 2，但迁移和部分文档尚未闭环。`workspace.apply_patch` 已有隐藏 prepare/revalidate/execute adapter；TUI 已能回答 `approval/request`，daemon 尚无 outbound request queue、pending response map 或 response 解复用。`ProcessExecutor` 仍是孤立 primitive，不能直接视为可安全发布的 `process.run`。

## 当前 P1 状态与高风险缺口

1. `EventLedger::append_invocation_outcome` 已提供 terminal state + ToolResult 原子入口，但生产 runner 仍先写 terminal transition、再写 ToolResult；崩溃或第二次写失败会留下不可恢复的不一致。
2. sealed `ToolRegistry`、plan/revalidate 和不可 Clone/Deserialize 的 by-value `ExecutionPermit` 已存在，但 runner 仍直接调用 `WorkspaceTools::prepare_effects/execute`；生产 permit constructor 和统一 `InvocationPipeline` 缺失。
3. approval binding 已精确绑定 workspace/invocation/thread/turn/agent/mode/tool/version/args/effects，但 daemon broker 缺失；客户端 response 类型仍允许携带 binding，未来 daemon 必须只采纳 bool 并自己铸造、单次消费 authority。
4. `Workspace` identity 在 open 时缓存。runner 从 ledger root 重新 open，但尚未把 live root identity 与持久化 identity、plan/revalidate、execution 全链路重验；根目录替换与中间目录 TOCTOU 必须动态验证。
5. `turn/interrupt`、`spawn_blocking` worker、Started/Cancelled/Unknown 的时间关系需要验证。写工具或进程运行时不能在副作用真正停止前宣称 Cancelled。
6. ToolSpec timeout 尚未被 runner 执行；context/ledger replay 也缺总量预算。当前主要是可靠性面，但在共享 provider 费用或持久化攻击输入场景可能形成拒绝服务或 denial-of-wallet。
7. `workspace.apply_patch` runtime 边界较成熟，但最终校验到 rename 仍不是 dirfd-relative CAS；在 policy、approval、permit、atomic ledger 与 live identity 闭环前必须保持隐藏。
8. `ProcessRequest` 当前允许调用方携带 environment、stdin 和 SandboxRequirement，且缺 P1 schema 的 argv/env/timeout/output 硬上限与 sandbox-ready/Started/go 握手；`process.run` 尚不可接入。
9. plugin host capability 目前主要是声明与 manifest hash，未提供 OS sandbox 或完整进程树治理；它尚未进入 daemon authority，不应在本轮当作已发布工具面。

## 本轮优先狩猎与修复起点

- 状态机/ledger：terminal-result 原子性、取消早报、Started→Unknown、reconcile 和重复恢复。
- authority/access control：registry/pipeline 直调绕过、permit 铸造/复用、approval response/binding、workspace trust 与 AgentId 归因。
- resource/file/process：live workspace identity、apply_patch CAS/TOCTOU、tool timeout/output budget、process executable/descendant/sandbox 边界。
- AI/LLM：不可信仓库内容经 read/search 进入 provider 后，是否能越过用户本来不具备的 capability；仅“模型可被 prompt injection”不构成 finding。

正式 finding 必须给出可执行攻击者、越过的边界、精确输入到 sink 路径与可观察影响。当前不可达的 hidden mutation/process、纯 defense-in-depth 缺口和同 UID 设计信任不应被抬高为漏洞，但仍可作为 P1 发布阻断修复项。
