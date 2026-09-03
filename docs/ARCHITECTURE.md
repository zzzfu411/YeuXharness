# YeuX Harness v1 架构

状态：Accepted target architecture，v0.1 基线正在实现（实现状态核对：2026-09-03）
适用平台：macOS、Linux  
许可证：Apache-2.0

本文描述 v1 的目标架构，同时明确当前仓库已经实现的边界。目标能力不等于当前可用能力；实际进度以“实现状态”一节和测试为准。

## 1. 产品不变量

YeuX 的北极星是安全、可重放、可解释。以下约束优先于功能数量：

1. Rust 服务 `yeuxd` 独占数据库、模型、工具、策略、沙箱和任务执行权。
2. 原始事件只追加。Thread、Turn、Item、Job 和调用状态都是可重建投影。
3. Replay 只重建投影，不调用 provider、工具、网络或外部系统。
4. 所有副作用进入同一执行管线，内置工具、Shell、MCP、插件、Job 和子智能体都不能绕过。
5. 有效权限只能是 `host ceiling ∩ user profile ∩ project trust ∩ turn override`；子智能体只能继承或收紧。
6. `observe` 是真正只读的执行模式，不是提示词约定。
7. 无法建立所需 OS 沙箱时失败关闭。
8. 非幂等调用若在崩溃后状态未知，只能 reconciliation，不能自动重试。
9. 补丁绑定文件 base hash；内容变化时拒绝覆盖。
10. 凭据只由 `CredentialBroker` 按句柄短期注入，不进入模型上下文、事件或普通子进程环境。

本文件的“已实现”表示代码路径和回归测试已经具备；“未完成/残余”表示仍不能作为
发布级保证。尤其是 POSIX 没有按 inode/hash 做条件 `rename` 的原语，文件发布的
最后一个命名空间竞态必须继续由调用方和 reconciliation 处理。

威胁边界详见 [THREAT_MODEL.md](THREAT_MODEL.md)。

## 2. 进程与信任边界

```text
┌──────────────────────────── User surface ────────────────────────────┐
│  yeux TUI / JSONL client                                             │
│  TypeScript; render + input only; no database or tool authority      │
└───────────────────────────────┬──────────────────────────────────────┘
                                │ JSON-RPC 2.0
                       Unix socket | stdio fallback
┌───────────────────────────────▼──────────────────────────────────────┐
│ yeuxd: Rust authority                                                │
│                                                                     │
│ protocol -> session runtime -> agent core -> unified tool pipeline  │
│                 |                 |                    |              │
│            provider adapters   policy/approval      OS sandbox       │
│                 |                 |                    |              │
│                 `------ event persistence + projections -------------│
└───────────────────────────────┬──────────────────────────────────────┘
                                │ restricted RPC
┌───────────────────────────────▼──────────────────────────────────────┐
│ process-isolated plugin host -> third-party provider/tool/command    │
└──────────────────────────────────────────────────────────────────────┘

                  SQLite WAL             artifact store
                  append-only             content addressed
```

交互客户端优先连接每用户 Unix socket：`YEUX_SOCKET` 可显式覆盖；否则使用 `$XDG_RUNTIME_DIR/yeux/yeuxd.sock`，没有 XDG runtime 时使用 `${os.tmpdir()}/yeux-<uid>/yeuxd.sock`（Linux 通常为 `/tmp/yeux-<uid>/yeuxd.sock`）。daemon 只在当前 UID 所有、group/other 不可访问的真实目录中创建 `0600` socket；客户端拒绝 symlink、错误 owner/mode/type，并在连接前后比较父目录与 socket 的 device/inode。没有可用 daemon 时启动 `yeuxd --stdio`。启用本地定时任务后，`yeuxd` 由 launchd 或 systemd 常驻。安装包最终包含版本匹配的 `yeux`、`yeuxd` 和 plugin host，不要求用户预装 Node/Bun。

当前代码已经实现 stdio、Unix socket、单写者锁和 TypeScript fallback 连接。launchd/systemd 服务、打包后的 `yeux` 可执行文件和签名发布尚未实现。

## 3. 模块边界

YeuX 保持四个 Rust crate 的模块化单体，不引入微服务或 Bazel：

| 模块 | 职责 | 禁止事项 |
|---|---|---|
| `yeux-protocol` | I/O-free wire types、稳定方法名、JSON Schema、模型/工具/权限公共类型 | 数据库、网络、进程执行 |
| `yeux-core` | Turn 与 Invocation 状态机、投影 replay、policy 语义、provider/tool/store ports | 厂商 HTTP 格式、平台 I/O |
| `yeux-runtime` | SQLite、artifact、workspace、provider、process、sandbox、descriptor 实现 | UI 行为、绕过 core 的权限决策 |
| `yeuxd` | JSON-RPC 调度、连接、订阅、命令幂等、唯一执行权 | 终端渲染、插件代码同进程加载 |

TypeScript 侧：

| 包 | 职责 |
|---|---|
| `@yeux/protocol` | JSON-RPC 客户端、连接错误和生成后的协议类型 |
| `@yeux/tui` | `yeux` 的终端交互、审批和事件投影；人类终端 sink 清理控制序列，JSONL 保留原始协议 |
| `@yeux/plugin-host` | 校验 manifest/摘要、限制能力、管理插件子进程 |

依赖方向必须保持单向：界面只依赖协议；provider 不读取工作区；插件不能直接访问 SQLite、凭据或策略引擎。

## 4. 状态与事件

### 4.1 领域层级

```text
Workspace
  `- Thread
       |- Turn
       |    `- Item
       `- child Thread (parent_thread_id + parent_seq)
```

每个 Thread 同时最多一个非终态 Turn。子智能体拥有独立 Thread 和局部 `seq`，通过 parent 和 causation 标识关联；系统不虚构跨 Thread 的全局因果顺序。

### 4.2 事实源

SQLite WAL 中的 `events` 是 Workspace/Thread/Turn/Item/Job 的唯一事实源。每个事件包含：

- 协议 schema version；
- UUIDv7 `event_id`；
- Thread 内从 1 开始严格单调的 `seq`；
- 可选 Turn、agent 和 `causation_id`；
- 时间、事件种类和结构化 payload。

数据库触发器禁止更新或删除事件。JSONL 只用于协议传输、导入导出和黄金测试，不是第二套状态库。大型输出写入内容寻址 artifact store；账本保存摘要、散列和引用。

### 4.3 Replay

Replay 算法只做以下事情：

```text
read ordered events -> validate schema/ID/seq/state transition -> apply projection
```

它不得实例化 provider、工具执行器或网络客户端。重复事件、序列缺口、非法状态跳转、包络与 payload 不一致都会失败。压缩在后续版本中生成带来源范围的 checkpoint summary，但不删除原始事件。

当前 `yeux-core::Projection` 和 `yeux-runtime::EventLedger` 已实现上述纯投影规则；已有一条覆盖完整 Thread 生命周期与跨重启去重的可执行 golden trace。快照校验、Agent loop 零调用计数和完整崩溃注入覆盖仍需补齐。

## 5. 公共协议

协议采用 JSON-RPC 2.0，stdio 与 socket 都以换行分隔 UTF-8 JSON。首条命令必须是 `initialize`。所有客户端命令携带独立于 JSON-RPC `id` 的 UUIDv7 `command_id`，所有事件通过 `event` 通知发送。

稳定方法族：

- `workspace/open|trust|status`
- `thread/start|resume|fork|read|list|archive|compact|subscribe`
- `turn/start|steer|interrupt`
- `model/list`、`skill/list`、`mcp/status`、`plugin/list`
- `job/create|list|pause|resume|run`
- 服务端请求 `approval/request`、`user/input`

稳定 API 与 `experimental/` 前缀隔离。主版本不兼容时拒绝初始化；同一主版本内服务端可接受其支持范围内的较旧 minor 客户端。重连使用 `thread/subscribe { afterSeq }`：daemon 固定 `replayedThroughSeq` 水位，先补发到该水位再发送实时事件。客户端落后于广播缓冲时连接关闭并要求从最后 `seq` 恢复。

完整 wire 约定与当前实现矩阵见 [PROTOCOL.md](PROTOCOL.md)。Rust 类型是 schema 源；稳定 schema 已生成到 `spec/schema/` 并由测试阻止漂移。TypeScript 完整自动生成仍属于 M0 未完成项。

## 6. Agent 与调用状态机

Turn 状态：

```text
accepted -> building_context -> requesting_model -> streaming
         -> proposed_tools -> waiting_for_approval -> authorizing
         -> scheduling -> executing -> integrating_results -> ...

任意活动状态 -> cancelling -> cancelled
合法活动状态 -> completed | failed
```

工具调用状态固定为：

```text
proposed -> approved -> prepared -> started
         -> completed | failed | cancelled | unknown
```

状态转换只能由 core 校验过的事件推进。`steer` 在下一个安全点修改当前 Turn 的后续行为；follow-up 在当前 Turn 完成后创建新 Turn。已经发生的副作用不会被取消动作伪装成回滚。

当前 core 已实现状态转换规则和 projection 校验。daemon 已将单请求 runner 扩展为有界多轮 Agent loop：从 root 到 leaf 按每层 `parent_seq` 构建 fork 谱系上下文，在 provider 支持 tool calls 时注册三个内置 workspace 只读工具，并在 sandbox 就绪且 host ceiling 非 `observe` 时按统一管线广告 `workspace.apply_patch`、`process.run`。runner 持久化模型流、ToolCall/ToolResult Item、Invocation 状态和最终 assistant Item，然后进入 `completed`、`failed` 或 `cancelled`。每轮调用之间会重新加载 ledger，因此已持久化的 `turn/steer` 会在下一模型请求安全点作为用户内容进入当前 loop；多个只读调用受 daemon 全局 worker 闸门约束并可并发执行，但同一 workspace identity 的 `search` 通过单槽闸门串行化，结果仍按模型首次给出的调用顺序入账和回灌。取消后的 provider delta 不落账；若工具已跨越执行边界且停止证据不足，Invocation 记为 `Unknown`，父 Turn 以 reconciliation-required 的 `failed` 诊断收束，不伪装成 `cancelled`；daemon 重启会把未终结 Turn 记录为 `failed`，不会自动重调 provider 或工具。

在只读工具之外，当前 daemon 还会在 sandbox 就绪且 host ceiling 非 `observe` 时广告受保护的
`workspace.apply_patch` 和 `process.run`。两者必须经过统一 policy/approval/sandbox 管线、
一次性 prepared token、执行前重验证和 opaque permit；网络、MCP 和插件工具仍不向模型开放。
当前闭环的边界如下：

- 文件 mutation 已在 Unix 上使用保留的 workspace root dirfd、逐组件
  `openat(O_NOFOLLOW)`、dirfd-relative 临时文件和 `renameat`，并在发布前重检父目录身份；
  这关闭了路径遍历、中间目录/符号链接重定向和大多数 stale-write 路径。POSIX 没有
  inode/hash 条件发布原语，因此非合作写者仍可在最后一次检查后替换最终名称（或移动已打开的
  父目录）；该残余必须显式报告，不能声称严格 CAS。
- 进程请求只有在 backend capability probe 通过、且每次启动前的有界 launcher
  handshake 成功后才会执行。Linux bubblewrap 使用 PID namespace、`--die-with-parent`
  和新 session；macOS Seatbelt 明确不声明 process isolation，因此 `process.run` 在该
  平台保持关闭。无法证明后代治理时失败关闭并进入可 reconciliation 的错误路径。
- 凭据 broker 已贯穿 runner/pipeline/provider 的注入 seam，lease/handle 只在运行时存在；
  独立 CLI 仍构造 `NoCredentialBroker`，未接入操作系统 keychain 或企业 secret store，
  所以未解析句柄会失败关闭。
- `invocation/reconcile` 已实现为仅接受终态父 Turn、来源为 `operator_review` 的有界证据，
  以幂等 ledger 事务收束 `Unknown`；它不会重试 provider 或工具。artifact 输出、崩溃矩阵、
  真实仓库 E2E、网络 endpoint 代理和发布/迁移仍是后续 release blocker。

## 7. 模型层

核心仅认识 provider-neutral `ModelRequest`、`ModelEvent`、content blocks 和 `ProviderCapabilities`。能力协商覆盖 tool calls、reasoning、vision、prompt cache、并行调用和 context limit。模型失败默认不跨供应商 fallback，因为成本、隐私和行为边界不同。

v1 provider 目标：

1. OpenAI Responses；
2. Anthropic Messages；
3. Gemini；
4. OpenAI-compatible adapter，覆盖 DeepSeek、xAI、OpenRouter、Ollama 和 LM Studio。

模型供应商网络与工具网络分开治理。Provider 凭据通过 opaque handle 请求 `CredentialBroker`，不得从项目配置或普通环境变量直接继承。

当前 OpenAI-compatible Chat Completions SSE adapter 可通过 daemon 的 `--provider-base-url` 与 `--model` 注册到 Turn runner；tool calling 默认按 provider capability 启用，也可用 `--disable-provider-tools` 关闭；Agent loop 默认最多 8 个模型轮次、32 个工具调用和 4 MiB 累计工具结果，并允许从 CLI 收紧。该 adapter 将非成功响应体限制为 8 KiB，并限制 SSE 缓冲（8 MiB）、流总量（64 MiB）、SSE/模型事件（各 100,000）、累计输出（32 MiB）和同时跟踪的 tool-call 状态（4,096）。`CredentialBroker` 的 runtime 注入 seam 已实现：嵌入式 daemon 可提供 broker，provider 只在 HTTP 边界短暂解析 opaque handle；独立 CLI 仍使用 `NoCredentialBroker`，因此未解析 handle 会失败关闭，且 secret 不会进入事件、模型上下文或普通环境。OpenAI Responses、Anthropic、Gemini 等原生 adapter、keychain/企业 secret store、重试退避和成本/模型目录尚未实现。

## 8. 工具与副作用

公共核心类型为 `ToolSpec`、`EffectSet` 和 `PreparedInvocation`。工具执行必须经过：

```text
1. validate input schema
2. normalize args and prepare conservative EffectSet
3. intersect host/user/project/turn grants
4. obtain invocation-bound approval when required
5. establish OS sandbox
6. persist started, then execute
7. redact and normalize output; spill large output to artifact
8. persist terminal state
```

审批绑定规范化参数、工具版本、workspace identity、Thread、agent、mode、effect digest 与过期时间。任一字段变化都使授权失效。

结构化文件工具应给出精确路径。Shell 和任意解释器只能声明粗粒度能力上界；静态命令解析可改善提示，但不能作为安全边界。v0.1 所有进程调用串行，只有结构化只读工具允许并行，结果按模型调用顺序入账。

当前 M1 专用路径已经实现 `workspace.list`、`workspace.read`、`workspace.search`：参数必须是拒绝未知字段的 JSON object；遍历和结果使用稳定顺序；路径逃逸、符号链接、硬链接、非 UTF-8 内容和越界资源会返回稳定错误。硬上限为 10,000 个遍历项、32 层深度、1 MiB 单文件、32 MiB 累计扫描、1,000 个匹配和 8 MiB 序列化结果。`workspace.search` 还受每 Turn 共享的 matcher operation budget（由 32 MiB 扫描上限推导、配置只能收紧）、同一 canonical workspace identity 的单槽 gate，以及 daemon 级 4 槽 blocking-worker 上限约束。tool-call 参数碎片按首次出现顺序汇聚，并限制每轮调用数、单调用参数和每轮累计参数大小。

每次已接受的调用会记录实际解析的 effect，并持久化 `proposed -> approved -> prepared -> started -> completed/failed/cancelled/unknown`；只读调用以无需交互审批的固定原因进入 `approved`，副作用调用通过 daemon 审批绑定。`unknown` 表示执行边界后的结果无法证明，不能静默重试，须通过 `invocation/reconcile` 收束。同一轮只读调用会在全局 worker 闸门下执行并按模型调用顺序入账；未知工具只返回错误结果，不会被转交给 Shell、插件或其他执行器；未协商 tool capability 时出现工具调用会使 Turn 失败且不执行。首版受保护 mutation/process 和 evidence-only reconciliation 已形成可测试闭环；发布级文件 CAS 的最后命名空间竞态、完整 artifact 输出/GC、endpoint 网络代理、崩溃矩阵和真实任务 E2E 仍未完成。

补丁必须携带 base hash。当前 workspace primitive 已拒绝绝对路径、`..`、静态符号链接逃逸、多硬链接和陈旧 revision；Unix 叶子文件的校验与哈希绑定同一 `O_NOFOLLOW` 文件描述符。发布路径保留 canonical workspace root dirfd，逐组件使用 `openat`/`O_NOFOLLOW`，临时文件通过 dirfd-relative `O_EXCL` 创建并以 `renameat` 原子替换，发布前重检父目录 device/inode；`workspace.apply_patch` 已作为 daemon ToolSpec 通过统一管线接入，在 capability gate 通过后由 descriptor-bound writer 发布并再次校验 revision。它不启动任意子进程，因此该路径的直接边界是 descriptor/policy/approval，而非 process-tree sandbox。`Workspace::identity_snapshot` / `live_identity` / `revalidate_identity` 会在打开时记录并在每个路径操作及发布边界重新验证 canonical root、device、inode 和 digest；`revision_snapshot` / `revalidate_revision` 同样绑定文件 revision 与 device/inode，并在哈希前后拒绝对象变化。因此根目录替换、同字节新 inode、读入期间的文件变化、中间目录替换和最终目标符号链接重定向都会失败关闭，而不会被旧的缓存 digest 静默接受。残余限制是 POSIX `renameat` 没有“仅当目标仍为该 inode/hash 才替换”的条件原语：非合作写者可在最后重检后替换最终名称，或把已打开的父目录移出原路径；结果因此是 fd 对象绑定而非完整路径命名空间 CAS，调用方必须将不确定结果交给 reconciliation。

当前 process primitive 会串行调用、清空继承环境、校验显式变量名并拒绝普通环境中的敏感变量。`process.run` 已接入 daemon 统一管线；Seatbelt/bubblewrap launcher 只获得固定最小 `PATH`，目标变量由 `/usr/bin/env -i` 或 `bwrap --setenv` 在隔离建立后注入，避免 `LD_*`/`DYLD_*` 等变量先影响 launcher。backend detection 会先做 isolation probe；每次请求在 spawn 前还要通过有界 launcher handshake。Linux bubblewrap 使用 PID namespace、`--die-with-parent` 和新 session；Unix runner 先用 `waitid(WNOWAIT)` 保留已退出 leader 的 PID/PGID 身份，再收束进程组并回收 leader，避免清理阶段按复用后的数字 PID 误发信号。无法证明后代结束、输出管道收束或已发布 patch 的目录 durability 时，runner 会将 Invocation 置为 `Unknown` 并阻断下一轮，而不是压成可重试的 `Failed`。macOS Seatbelt 不声明 process isolation，因此任意进程工具在该平台不广告；不能证明后代治理的 backend 也会失败关闭。仍未完成的是跨平台 supervisor/cgroup 证据、完整崩溃注入覆盖，以及网络 endpoint/private-network/metadata 代理。

## 9. 权限、配置与凭据

### 9.1 三种模式

| 模式 | 语义 |
|---|---|
| `observe` | 结构化只读；禁止进程、写入、外部写和工具网络 |
| `build` | 可信工作区内写入与受限进程；网络和越界访问需审批 |
| `operate` | 可请求外部写，但仍受 host ceiling、审批、密钥代理和沙箱限制 |

策略结果为 allow、ask 或 deny。显式 deny 和上层 ceiling 不能被下层授权覆盖。未信任项目最多获得 observe 能力。

### 9.2 配置优先级

```text
command/Turn override
  > trusted project .yeux/config.toml
  > profile
  > user config
  > defaults
```

项目配置不能修改凭据、provider executable、遥测或沙箱上限。项目 hooks、Skills 脚本、MCP 和插件均按内容摘要授予信任。遥测默认关闭。

### 9.3 平台沙箱

- macOS：Seatbelt；文件系统和网络 profile 可探测，但不声明任意 descendant
  process isolation，因此 `process.run` 保持关闭。
- Linux：bubblewrap/namespaces；backend 启动时执行 isolation probe，每次 target spawn 前
  执行有界 handshake，
  使用 PID namespace、`--die-with-parent`、`--new-session`、`--clearenv` 等固定参数，
  可用时记录 Landlock 能力。
- 不支持、探测失败或能力不足：失败关闭；结构化 workspace mutation 与进程能力分别判定，
  不因缺少 process isolation 而误关安全的文件工具。

当前尚未提供 endpoint 级域名/IP 代理，也未完整防护私网、云 metadata、代理绕过和 DNS
rebinding；`allow_network` 只表示 sandbox profile/namespace 层面的网络边界，不能当作
应用层网络策略。Landlock/seccomp 的强制叠加、跨平台 supervisor 和发布环境探针仍是
release blocker。

## 10. 扩展、记忆与编排

内置工具、MCP 和插件统一转为 `ToolSpec`。Skills 兼容 `SKILL.md`/agentskills.io；MCP 目标支持 stdio、Streamable HTTP 和延迟发现。第三方插件只能贡献 tools/providers/commands，不能替换 policy、ledger 或 UI。

记忆分四层：完整事件历史、SQLite FTS5 会话搜索、用户批准的策展事实、程序性 Skills。v1 默认不使用向量数据库。压缩保留所有事件，并用来源 seq 范围连接摘要和证据。

Job 固定 prompt、模型、工具集、workspace、权限 profile 和预算；无预授权动作进入 `waiting_for_approval`。默认禁止重入，多个错过周期最多补跑一次。

v1 只支持一层本地子智能体，默认并发上限 4。只读任务可共享 workspace；写任务必须使用独立 Git worktree，由父级显式审查和合并。取消、时限、token、成本和权限向下级联。

当前只有 descriptor store、Job 协议/状态投影、`invocation/reconcile` evidence-only
命令和 plugin host 基线；执行型 MCP、Skills、调度器、SQLite FTS5 和子智能体均未实现。
reconcile 只收束已知为 `Unknown` 且父 Turn 已终态的调用，不会重试任何 provider/tool，
并要求有界的 operator evidence。交互 TUI 与无头 JSONL 对同一 ledger 的完整投影一致性
门禁、artifact 引用/GC 和崩溃恢复 E2E 也仍待补齐。

## 11. 实现状态

| 能力 | 当前状态 | v1 目标 |
|---|---|---|
| JSON-RPC、stdio、Unix socket | 已实现每用户私有路径与连接前后身份检查 | 兼容矩阵、长期连接加固 |
| SQLite append-only ledger | 已实现基线 | 迁移、备份、崩溃窗口验证 |
| 纯 projection replay | 已实现基线和 lifecycle golden trace | 扩展 traces 与快照交叉校验 |
| Workspace/Thread/Turn/Item | 已实现投影、基础命令与有界多轮 Agent loop；steer 在下一模型请求安全点注入；mutation/process 受统一 permit 管线约束 | compaction、完整恢复矩阵与真实仓库 E2E |
| OpenAI-compatible provider | Chat Completions SSE adapter 已接 daemon；支持 tool calls，流/输出/状态/loop 均有硬上限；runtime/pipeline/provider 可注入 `CredentialBroker`，CLI 默认 `NoCredentialBroker` 并失败关闭未解析 handle | keychain/企业 secret store、原生 Responses/Anthropic/Gemini、契约/成本测试 |
| 结构化 workspace 只读工具 | `list/read/search` 已接 daemon；严格参数、路径/链接防护、实际 effect、并发执行与模型顺序入账 | 更完整真实仓库 E2E 与预算调优 |
| 工作区 patch/process/sandbox/artifact | Unix patch 使用 root dirfd、逐组件 `openat(O_NOFOLLOW)`、`O_EXCL` 临时文件和 `renameat`，并重检父目录身份；最终名称仍无 POSIX 条件 inode/hash CAS。process 仅在 capability probe + handshake 成功时广告（Linux PID namespace；macOS 关闭）；artifact 仍为原语 | 完整命名空间 CAS/hostile-writer 证据、跨平台 supervisor、artifact 输出/GC、endpoint 网络代理 |
| Unknown invocation / reconciliation | `invocation/reconcile` 已实现：父 Turn 终态、`operator_review` 有界证据、幂等 ledger 事务；不重试 provider/tool | 自动化 operator UX、崩溃注入与 artifact 证据关联 |
| TypeScript 终端客户端 | 安全 socket 连接、终端清理与原始 JSONL 已实现 | OpenTUI 体验、完整命令面与 JSONL parity 门禁 |
| 插件 | 独立进程与摘要校验基线 | OS 沙箱、Rust policy/ledger 接入 |
| Skills/MCP/FTS | 仅协议或 descriptor 占位；FTS 尚未实现 | M3 完整实现 |
| Job | 规格与状态管理 | M4 调度、恢复和无交互审批 |
| 子智能体 | 公共类型与事件 | M4 worktree 隔离和 handoff |
| 发行 | 未实现 | 签名二进制、Homebrew、SBOM |

## 12. v1 非目标

Windows、远程/云沙箱、消息平台、语音、企业控制面、插件市场、Python SDK、ACP、向量记忆和子智能体自动合并均不在 v1 范围内。
