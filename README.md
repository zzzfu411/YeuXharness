# YeuX Harness

> 安全、可重放、可解释的本地混合智能体平台。

YeuX Harness 面向个人高级用户，采用 Rust 权威运行时和 TypeScript 终端界面。它将模型输出、仓库内容、工具、MCP 与插件都视为不可信输入，并用事件账本、能力交集、精确审批和操作系统沙箱约束副作用。

项目采用 Apache-2.0 许可证，首发目标平台为 macOS 和 Linux。

## 当前状态

**这是 v0.1 工程基线：只读 Agent loop 已贯通，但还不是可完成真实“读、改、测、修”的发布版。**

当前代码已经提供：

- 四个 Rust crate：`yeux-protocol`、`yeux-core`、`yeux-runtime`、`yeuxd`。
- JSON-RPC 2.0 类型、UUIDv7 命令标识、Thread 内单调事件序列和协议版本协商。
- 从 Rust 类型生成并提交的 54 份稳定 JSON Schema，以及字节级 drift test。
- SQLite WAL 追加式事件账本、禁止更新/删除的数据库触发器，以及纯事件投影 replay。
- `Workspace -> Thread -> Turn -> Item` 投影、单 Thread 单 active Turn 约束、按 `parent_seq` 继承上下文的 fork、steer 控制事件持久化、interrupt 执行控制和按 `afterSeq` 补发。
- stdio 与 Unix socket daemon 传输；客户端默认探测 `$XDG_RUNTIME_DIR/yeux/yeuxd.sock`，否则使用 `${os.tmpdir()}/yeux-<uid>/yeuxd.sock`（Linux 通常为 `/tmp/yeux-<uid>/yeuxd.sock`）。socket 父目录和节点必须由当前 UID 所有且仅 owner 可访问，客户端在连接前后核对类型及 device/inode。
- OpenAI-compatible 流式 provider 与有界多轮只读 Agent loop。Provider 声明支持 tool calls 时，daemon 只注册 `workspace.list`、`workspace.read`、`workspace.search`；模型可调用工具、接收结果并继续推理。Tool-call JSON 分片、调用数/参数/结果预算、累计输出和 provider 流均有硬上限；`workspace.search` 另受每 Turn 共享的 matcher operation budget（由 32 MiB 扫描上限推导、配置只能收紧）、同一 canonical workspace identity 的单槽 gate，以及 daemon 级 4 槽 blocking-worker 上限约束。
- 受 revision 保护的工作区读写原语、内容寻址 artifact store、能力策略和 macOS/Linux 沙箱封装原语。三个只读工具具有严格 JSON 参数、稳定输出顺序、workspace 路径与链接防护，并记录解析后的实际读取 effect。
- TypeScript 协议包和进程外 plugin host 基线；人类终端上的模型/诊断/交互文本会移除 ANSI/OSC、C0/C1 和双向文本控制字符，`--jsonl` 则保留原始协议 payload。插件可执行文件需要摘要校验，能力只能从 manifest 请求集合中收紧。
- 可执行 lifecycle golden trace，覆盖订阅补发、Turn 中断、replay 和 daemon 重启后的命令去重。

当前只读闭环的边界如下：

- `turn/start` 会在持久化 Turn 和用户消息后启动后台 runner；配置 `--provider-base-url` 和 `--model` 时，可完成 `provider -> read-only tools -> provider -> answer` 的多轮任务。流式模型事件、ToolCall/ToolResult Item、Invocation 状态和 Turn 终态均进入 ledger。未配置 provider 时，Turn 以 `provider_unconfigured` 诊断进入 `failed`。
- 同一模型轮次内的多个只读调用会并发执行（同一 canonical workspace identity 的 `search` 默认串行），但结果严格按模型首次给出的调用顺序持久化并回灌。Invocation 当前经历 `proposed -> approved -> prepared -> started -> completed/failed/cancelled/unknown`；`unknown` 表示停止或外部结果无法证明，必须先 reconciliation，未注册工具只形成错误结果，不会被分派为 Shell、写文件或网络操作。
- runner 在每次后续 provider 请求前从 ledger 重新加载上下文，因此已持久化的 `turn/steer` 会在下一模型请求安全点进入当前 loop。取消后不再提交残余 provider delta；若工具已跨越执行边界但无法证明停止，会记录有界 Unknown 诊断并将 Turn 以 reconciliation-required 的 `failed` 收束，而不是伪装成 clean `cancelled`；daemon 重启会把未终结 Turn 记录为 `failed`，不会自动重放外部工作。
- 默认 Turn 上限为 8 个模型轮次、32 个工具调用、4 MiB 累计工具结果和一个共享 search operation budget（不超过一次 32 MiB 扫描的 matcher 工作量；CLI 只能收紧）。单个只读调用还受 10,000 个遍历项、32 层深度、1 MiB 单文件、32 MiB 总扫描、1,000 个匹配和 8 MiB JSON 输出限制。
- 写文件、补丁和进程原语仍未接入 daemon 的统一 policy/approval/sandbox 管线，也不会暴露给模型。`CredentialBroker` 尚未接入 provider 配置；这是从只读闭环进入安全编码闭环前的明确阻断项。
- `thread/compact` 和 `job/run` 明确返回“功能不可用”；Job 目前只有规格与状态管理。
- Anthropic、Gemini、OpenAI Responses、Skills、MCP 执行、FTS 记忆、定时任务和 worktree 子智能体仍在路线图中；交互 TUI 与无头 JSONL 的完整投影一致性测试也尚未完成。
- 当前 plugin host 有独立进程、最小环境和摘要校验，但还没有接入 Rust 策略内核与 OS 沙箱，因此不应运行不受信任插件。
- TypeScript 协议类型目前只覆盖客户端使用的子集；从已提交 JSON Schema 自动生成完整类型仍需完成。

因此，当前适合协议和运行时开发、架构评审以及有边界的只读仓库探索；在写入、进程、审批与沙箱闭环完成前，不应作为自主编码工具使用。

## 核心承诺

### 安全

所有副作用最终都必须经过同一条管线：

```text
validate -> prepare effects -> policy intersection -> approval
         -> OS sandbox -> execute -> redact/normalize -> persist result
```

有效能力只能缩小：

```text
host ceiling ∩ user profile ∩ project trust ∩ turn override
```

目标状态下，`observe` 必须是执行层保证的只读模式；`build` 允许可信工作区内的受限写入和进程；`operate` 才能请求外部写操作。审批不能突破 host ceiling，子智能体也不能提权。当前三个内置只读工具已经执行严格参数校验、解析实际读取 effect、持久化调用状态并受路径与资源上限约束；写入、进程和网络能力尚未接入统一策略、审批与沙箱管线，因此仍保持不可用。

### 可重放

SQLite 中的追加事件是会话事实源。Replay 只读取历史事件并重建相同投影，**绝不会自动重新调用模型、工具、网络或外部系统**。未知状态的非幂等操作只能进入 reconciliation，不能自动重试。

### 可解释

事件携带 schema version、UUIDv7 `event_id`、Thread 内单调 `seq` 和 `causation_id`。当前只读闭环已记录 ToolCall/ToolResult、Invocation 状态和解析后的读取 effect；目标状态下，每次工具调用还必须完整说明工具版本、权限来源、审批摘要、沙箱证据和执行结果。

## 进程拓扑

```text
yeux (TypeScript terminal client)
   |  JSON-RPC 2.0 over private per-user Unix socket
   |  fallback: spawn `yeuxd --stdio`
   v
yeuxd (Rust authority)
   |- protocol dispatch and event subscriptions
   |- agent state machine and provider/tool ports
   |- policy, approval and sandbox boundary
   `- SQLite ledger and artifact store

third-party plugin
   ^
   `- restricted out-of-process plugin host (target architecture)
```

`yeuxd` 是数据库、模型、工具、沙箱和任务执行的唯一权威。TypeScript 客户端不得打开 SQLite，也不得直接执行工具。

## 仓库结构

```text
crates/
  yeux-protocol/   # I/O-free wire types、JSON Schema 源和稳定方法名
  yeux-core/       # 状态机、投影 replay、能力交集和执行端口
  yeux-runtime/    # SQLite、workspace、provider、process、sandbox、artifact
  yeuxd/           # stdio/Unix socket JSON-RPC daemon
packages/
  protocol/        # TypeScript JSON-RPC 客户端与当前协议类型
  tui/             # 协议专用终端客户端，发布目标命令名为 `yeux`
  plugin-host/     # 进程外插件宿主基线
docs/
  adr/             # 已接受架构决策
spec/traces/       # 黄金事件轨迹规范与后续 fixtures
spec/schema/       # 从 Rust 公共类型生成的稳定 JSON Schema
```

## 开发

环境要求：Rust 1.98、Node.js 22 或更高版本、pnpm 9.15.9。

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets

pnpm install
pnpm typecheck
pnpm test
pnpm build
```

可以单独启动协议 daemon：

```bash
cargo run -p yeuxd -- --stdio --state-dir /tmp/yeux-dev
```

stdio 与 Unix socket 都使用一行一个 JSON-RPC 消息的 UTF-8 JSON。当前终端客户端源码包名为 `@yeux/tui`，可执行命令名为 `yeux`；最终发行包会将它与匹配版本的 daemon 和 plugin host 一起打包，不依赖用户预装 Node/Bun。

## 文档

- [架构](docs/ARCHITECTURE.md)
- [协议](docs/PROTOCOL.md)
- [路线图](docs/ROADMAP.md)
- [Run 3 执行记录](docs/audits/2026-09-01-run-3/EXECUTION_LOG.md)
- [竞争差距分析与 P0–P4 计划](docs/COMPETITIVE_GAP_ANALYSIS.md)
- [威胁模型](docs/THREAT_MODEL.md)
- [架构决策](docs/adr/)
- [设计系统 / Paper Signal](docs/design/README.md)：深色优先的 TUI/CLI/网页美学、Unicode/ASCII 资产和可选双鱼欢迎资产

## 设计来源

YeuX 是 clean-room 实现，吸收以下项目的公开设计经验：

| 来源 | 主要借鉴 | YeuX 的取舍 |
|---|---|---|
| [Grok Build@bc7f02e](https://github.com/xai-org/grok-build/tree/bc7f02eddd3d84085849dc19ed216f11c23b0571) | runtime/workspace/policy 分层、延迟 MCP、worktree 子任务 | 沙箱默认开启，安全 hooks 失败关闭 |
| [Codex@d58d0e5](https://github.com/openai/codex/tree/d58d0e5841e0de08e251673db2d5af8cf3a1ad51) | Thread/Turn/Item、app-server、审批与 OS 沙箱正交 | 保留小型模块化单体，不复制企业与云端复杂度 |
| [Pi@853a80d](https://github.com/earendil-works/pi/tree/853a80d26c90a14c1886f0ebb8ffaae133ca2185) | 小内核、供应商无关 API、steer、fork、确定顺序 | 增加强制沙箱、审批和权限继承 |
| [DeepSeek Harness@0a53fb5](https://github.com/deepseek-ai/deepseek-harness/tree/0a53fb55bea101816fa226bb964ae2bed71c343b) | capability seams、可撤销注册、append-only ledger | policy、ledger 与 UI 边界不可被插件替换 |
| [Hermes@66666f6](https://github.com/NousResearch/hermes-agent/tree/66666f6e2eca0ae883195a34c66131985ea7dd06) | FTS 搜索、策展记忆、Skills、本地任务 | v1 不复制大单体与全消息网关 |

若后续复制 MIT/Apache 许可代码，必须保留原许可并更新 `THIRD_PARTY_NOTICES.md`。

## v1 边界

v1 不包含 Windows、远程/云沙箱、消息平台、语音、企业控制面、插件市场、Python SDK、ACP、向量记忆或子智能体自动合并。

详细交付顺序与发布门槛见 [ROADMAP](docs/ROADMAP.md)。
