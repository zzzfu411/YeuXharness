# YeuX Harness v1 路线图

状态日期：2026-09-03<br>
当前阶段：M0/M1 核心闭环已具备，M2 首版统一副作用管线已合并并通过主线 CI；文件 CAS、凭据注入、进程树监督、reconciliation 与发布门槛仍未完成<br>
当前版本：`0.1.0` 开发基线，不是 v0.1 发布版

路线图采用阶段门槛，而不是以“文件已经存在”判断完成。某项能力只有在 daemon 执行路径中接通、失败路径经过测试、文档与协议同步后才算交付。

历史执行证据见 [`Run 3 执行记录`](audits/2026-09-01-run-3/EXECUTION_LOG.md)；当前事实、GitHub 状态与本阶段计划见 [`Run 4 状态与计划`](audits/2026-09-03-run-4/STATUS_AND_PLAN.md)。

## 1. 当前实现快照

| 区域 | 已落地 | 尚未闭环 |
|---|---|---|
| 仓库 | Apache-2.0、NOTICE、四 Rust crate、三个 TypeScript 包、macOS/Linux CI | 发布构建、安装器、SBOM |
| 协议 | JSON-RPC 类型、UUIDv7 ID、版本协商、54 份稳定 schema 与 drift test | TS 自动生成与跨语言完整漂移门禁 |
| 状态 | Workspace/Thread/Turn/Item/Job 事件、状态机、纯 projection replay | compaction、FTS、快照校验、reconciliation UI |
| 存储 | SQLite WAL、追加式 events、Thread 内 seq、内容寻址 artifact | 迁移、备份、配额与 GC |
| daemon | stdio、每用户私有 Unix socket、单写者锁、订阅/补发、跨重启命令去重、有界多轮 Agent loop、ToolCall/ToolResult 与 Invocation 入账；M2 pipeline 已接入 mutation/process/approval/sandbox | 完整 provider 凭据调度、reconciliation UI、真实任务 E2E |
| runtime | workspace revision、结构化 `list/read/search`、root/file live identity revalidation、OpenAI-compatible adapter、policy、process、sandbox、artifact 原语；`CredentialBroker` seam 已存在 | daemon credential 注入、dirfd/openat2 CAS、完整进程树监督、网络代理与 artifact 输出策略 |
| TypeScript | JSON-RPC 客户端、socket 身份检查、终端安全渲染与原始 JSONL、stdio fallback、plugin host | OpenTUI、完整协议面、交互/JSONL parity、plugin OS 沙箱与 daemon 接入 |
| 自动化/多智能体 | 公共类型、事件和 Job 元数据状态 | scheduler、worktree 子智能体、预算与 handoff |

最重要的现状限制已经从“没有工具循环”转移为“首版副作用路径虽已接通，但还没有达到 hostile workspace 与发布级恢复门槛”：`turn/start` 在配置 provider 后可完成只读多轮任务；sandbox 就绪且 host ceiling 非 `observe` 时，provider 还可看到受统一管线保护的 `workspace.apply_patch` 与 `process.run`。文件目录 CAS、凭据注入、脱组进程树治理、完整 reconciliation 和真实“读、改、测、修”测试仍是发布阻断项。

## 2. M0：契约与仓库基线

目标：在执行真实副作用前固定不可轻易返工的边界。

### 已实现

- [x] Apache-2.0、NOTICE 和第三方许可记录入口。
- [x] `yeux-protocol`、`yeux-core`、`yeux-runtime`、`yeuxd` 四 crate 工作区。
- [x] Rust 权威 daemon + TypeScript 协议客户端的进程拓扑 ADR。
- [x] append-only ledger、纯投影 replay、capability/approval/sandbox ADR。
- [x] 威胁模型和 macOS/Linux CI 基线。
- [x] `ModelRequest`、`ModelEvent`、`ToolSpec`、`EffectSet`、`PreparedInvocation`、`EventEnvelope`、`CapabilityGrant`、`JobSpec`、`AgentSpawnSpec`、`AgentResult` 公共类型。
- [x] Turn 与 Invocation 状态转换检查、审批绑定 digest 语义。

### 完成 M0 仍需

- [x] 从 Rust schema bundle 生成并提交稳定 JSON Schema，测试执行字节级漂移检查。
- [ ] 从同一 schema 生成完整 TypeScript 类型，CI 阻止手工漂移。
- [x] 添加可执行 lifecycle golden trace，覆盖初始化、订阅、Turn、中断、replay 与跨重启去重。
- [ ] 用 faux clock、可注入 ID、faux provider/tool 证明固定 trace 可重复。
- [ ] 固定并测试平台能力矩阵和所有崩溃窗口的预期状态。
- [x] 将成功 command receipt 持久化接入 daemon；变更事件与响应同事务提交，重启后可去重。

### 退出门槛

- 同一事件序列总是生成相同投影。
- Replay 时 provider、网络和工具调用计数严格为零。
- schema 主版本不兼容时明确拒绝，未知兼容字段可被旧客户端忽略。
- 所有命令和事件 ID、时间均可在测试中注入。

## 3. M1：只读纵向闭环

目标：从 TypeScript 客户端发起 Turn，经 Rust daemon 调用模型和结构化只读工具，再以事件流返回结果；重启后可继续读取。

### 已实现的组成部分

- [x] stdio 和每用户 Unix socket JSON-RPC 传输；默认端点为 `$XDG_RUNTIME_DIR/yeux/yeuxd.sock`，否则为 `${os.tmpdir()}/yeux-<uid>/yeuxd.sock`。父目录/socket 的 owner、mode、类型以及连接前后的 device/inode 均受校验。
- [x] `initialize` 和协议主版本检查。
- [x] workspace open/status/trust。
- [x] thread start/resume/fork/read/list/archive/subscribe。
- [x] turn start/steer/interrupt 的持久化控制面。
- [x] SQLite ledger 与从事件重建 projection。
- [x] OpenAI-compatible Chat Completions SSE adapter 和 provider-neutral 流事件；错误体、SSE 缓冲/总量、SSE/模型事件、累计输出和 tool-call 状态均有硬上限。
- [x] 从 ledger 构建上下文（含按 `parent_seq` 截断的多级 fork 谱系）、调用 provider、持久化流事件/assistant Item 并进入 Turn 终态的 runner。
- [x] 通过 daemon 参数注册无凭据 OpenAI-compatible endpoint，并将运行中 interrupt 传递到 runner。
- [x] 取消后拒绝 provider 残余 delta；工具若已跨越执行边界且结果未知则记录 Unknown 并以 reconciliation-required 失败收束；重启后将未终结 Turn 明确标记失败且不重调 provider 或工具。
- [x] 将 `workspace.list`、`workspace.read`、`workspace.search` 注册为 provider 可见的结构化只读工具；严格拒绝未知 JSON 字段并记录实际解析的 read effect。
- [x] 为只读工具固定路径逃逸、symlink、硬链接、UTF-8 与资源防护；硬上限覆盖遍历项、深度、单文件、累计扫描、匹配数和 JSON 输出。
- [x] 汇聚碎片化 tool-call JSON，保持首次出现顺序并限制每轮调用数、单调用参数和累计参数字节。
- [x] 将 runner 扩展为有界多轮 `provider -> tools -> provider` loop；默认限制 8 个模型轮次、32 个工具调用和 4 MiB 累计工具结果。
- [x] 持久化 ToolCall/ToolResult Item 和 `proposed -> approved -> prepared -> started -> completed/failed/cancelled/unknown` Invocation 生命周期；Unknown 需要后续 reconciliation。
- [x] 同轮只读工具受 daemon 全局 worker 闸门约束并可并发执行，结果严格按模型调用顺序持久化并进入下一次请求；同一 workspace identity 的 `search` 使用单槽闸门。
- [x] 每轮模型请求前重新加载 ledger，使已持久化 `turn/steer` 在下一安全点进入当前 loop。
- [x] 未注册或未协商工具永不分派到 Shell、写入、网络或插件执行器；错误路径有稳定诊断和无副作用测试。
- [x] 增加真实 JSON-RPC 纵向测试，覆盖 `client -> daemon -> provider -> workspace.read -> provider -> answer`。
- [x] TypeScript 连接、JSONL 渲染和交互输入基线；人类终端输出会清理 ANSI/OSC、C0/C1 与双向文本控制字符，JSONL 保留原始协议内容。

### 完成 M1 仍需

- [ ] 完成 `--jsonl` 无头模式与交互 TUI 的投影一致性测试。
- [ ] 增加会话 FTS 搜索所需的最小投影，完整记忆仍属于 M3。
- [ ] 将只读纵向测试扩展为真实仓库任务套件，并补齐 crash/restart、replay 零外部调用和随机并发顺序门禁。

### 退出门槛

- 一条真实只读任务可经 `yeux -> yeuxd -> provider -> read/search -> provider -> answer` 完成。
- daemon 崩溃并重启后，`thread/subscribe { afterSeq }` 无缺失地补发。
- Replay 只重建状态，不重新请求 provider 或工具。
- 慢客户端收到背压诊断并能从最后确认的 `seq` 恢复。
- 同一 Thread 的第二个 active Turn 被拒绝。

## 4. M2：受保护的编码闭环

目标：发布 v0.1，可在真实仓库中安全完成“读、改、测、修”。

首版副作用管线已合并：`workspace.apply_patch` 与 `process.run` 作为隐藏 adapter 注册，经过统一 policy/approval/sandbox、一次性 token、执行前重验证和 opaque permit；sandbox 不可用时不向 provider 广告。M2 后半段仍需把原语提升到发布级安全证明，特别是 dirfd-relative CAS、凭据注入、进程树监督、reconciliation 与 artifact。

### 交付物

- [x] 内置 `workspace.apply_patch` adapter（含 base revision、diff summary、沙箱发布后 revision 校验）；Git diff/checkpoint 和更完整版本冲突 UX 待补。
- [x] 完整调用生命周期：`proposed -> approved -> prepared -> started -> terminal/unknown`，terminal ToolResult 原子入账。
- [x] `approval/request` TUI 交互、deny 默认、inspect unified diff 和 daemon-minted 精确 ApprovalBinding。
- [x] macOS Seatbelt、Linux bubblewrap/namespaces 能力探测；能力不足时失败关闭。
- [x] 已加固 launcher 环境边界的串行 `ProcessExecutor` 接入 daemon 统一管线；仍需补齐独立 stdout/stderr、超时及覆盖 `setsid`/`setpgid` 逃逸的完整进程树监督。
- [ ] 将 `CredentialBroker` 从 daemon 配置注入 provider，并提供 OS keychain seam 与轮换测试。
- [x] 为 mutation prepare→execute 传递 `FileRevisionSnapshot`，并将 device/inode/digest 纳入 runtime-only authority binding；同字节新 inode 会在 permit 前失败关闭。
- [ ] 使用 root dirfd/`openat`/`openat2` 完成目录级 CAS，消除最后的检查-发布竞态。
- [x] prepared/consumed token 增加 TTL 回收、容量上限和过期 fail-closed 语义。
- [ ] artifact 引用、输出裁剪、敏感数据跨 chunk 删改和配额。
- [ ] 将基础 Unknown marker/diagnostic 扩展为副作用工具的完整 reconciliation 流程与交互界面。
- [ ] 工具网络代理与私网、云 metadata、DNS rebinding 和代理绕过防护。

### v0.1 发布门槛

- 在真实测试仓库中完成“读、改、测、修”，并展示 diff 和验证结果。
- 人工并发修改使 base hash 失效时，补丁明确冲突且不覆盖用户内容。
- Shell 重定向、子 Shell、`..`、符号链接与硬链接不能绕过策略。
- 沙箱不可用时命令不执行。
- 在批准后、启动后、副作用完成后和提交前注入崩溃，不重复未知非幂等操作。

## 5. M3：模型、上下文与扩展

目标：补齐多 provider、长期上下文和受限扩展，同时保持 policy、ledger 与 UI 不可替换。

### 交付物

- [ ] OpenAI Responses、Anthropic Messages、Gemini 原生 adapter。
- [ ] OpenAI-compatible 覆盖 DeepSeek、xAI、OpenRouter、Ollama 和 LM Studio。
- [ ] capability negotiation 和 provider 契约测试；默认不跨供应商 fallback。
- [ ] 工具结果裁剪和带 source seq 范围的 checkpoint compaction。
- [ ] SQLite FTS5 搜索与经用户批准的策展记忆；向量数据库保持关闭。
- [ ] `SKILL.md`/agentskills.io 加载、来源和内容摘要信任。
- [ ] MCP stdio、Streamable HTTP 和延迟工具发现。
- [ ] plugin host 接入 Rust policy、approval、OS sandbox 和 ledger。
- [ ] `yeux doctor` 与 `yeux policy explain`。

### 退出门槛

- 四类 provider 通过碎片化 tool JSON、重复 call ID、429、超时、断流、溢出和取消契约测试。
- 压缩后的所有模型输入都能追溯到 ledger 范围，原始事件仍完整。
- 一万个 MCP 工具不会一次性进入模型上下文。
- 插件崩溃、超时、摘要变化或请求未声明能力时，不影响 daemon 与状态库。
- 插件只能贡献 tools/providers/commands，不能替换 policy、ledger 或 UI。

## 6. M4：本地自动化与隔离子智能体

目标：完成后台 daemon、可恢复 Job 和一层本地子智能体。

### 交付物

- [ ] launchd/systemd 服务与后台任务生命周期。
- [ ] 固定 prompt、模型、工具集、workspace、权限 profile 和预算的 Job snapshot。
- [ ] 本地 loopback webhook、调度恢复、DST 与错过周期处理。
- [ ] 默认禁止重入；错过多个周期最多补跑一次。
- [ ] 无交互预授权时进入 `waiting_for_approval`，不得静默扩大能力。
- [ ] 一层子智能体，默认并发上限 4。
- [ ] 只读子任务共享 workspace；写子任务强制独立 Git worktree。
- [ ] token、成本、时限、取消与 capability 向下级联。
- [ ] 结构化 `AgentResult` handoff、父级审查和显式合并。

### beta 发布门槛

- daemon 重启、系统休眠、DST、重复触发和凭据失效不会造成重复外部写。
- 无审批后台动作失败关闭。
- 子智能体不能提权、跨 worktree 写入或自动合并。
- 父级取消会结束子级与孤儿进程；有改动的 worktree 不自动删除。
- 合并冲突必须返回审查状态，不静默选择任一版本。

## 7. M5：v1 加固与发布

目标：将已通过阶段门槛的本地产品做成可安装、可升级、可审计的 v1。

### 交付物

- [ ] SQLite schema migration、备份、恢复和一致性检查。
- [ ] 协议兼容套件和 TUI/core 配套版本检查。
- [ ] artifact GC、容量配额、性能分析和长任务测试。
- [ ] 依赖固定、许可证审计、SBOM 和可复现构建。
- [ ] 签名 macOS/Linux release、校验和安装脚本与 Homebrew formula。
- [ ] 打包匹配版本的 `yeux`、`yeuxd` 和 plugin host，不要求 Node/Bun。
- [ ] 用户文档、安全指南、扩展作者文档和故障排查。
- [ ] 遥测保持默认关闭；任何可选指标先本地删改。

### v1 发布门槛

- 真实代码任务闭环通过，且 TUI 与无头模式得到一致投影。
- 随机崩溃恢复不重复副作用，无静默数据损坏。
- 四类 provider 全部通过契约测试。
- 安全测试矩阵无已知高危逃逸。
- 24 小时 soak 无进程、文件描述符或 artifact 泄漏。
- 安装包、依赖和示例扩展均有来源、许可证、签名和校验和。

## 8. 横向测试矩阵

每个里程碑都必须扩展以下测试，而不是把安全与恢复留到 M5：

| 领域 | 必测场景 |
|---|---|
| 协议 | 重复命令、冲突 command ID、慢客户端、背压、seq 缺口、重连、版本不匹配 |
| Provider | tool JSON 分片、重复 ID、限流、超时、context overflow、断流、取消后 delta |
| 文件 | `..`、绝对路径、符号/硬链接、大小写、Unicode、TOCTOU、stale base hash |
| 进程 | 重定向、子 Shell、最小环境、输出上限、超时、`setsid`/`setpgid`、进程树取消 |
| 网络/密钥 | 私网与 metadata、DNS rebinding、代理绕过、敏感环境、跨 chunk 泄漏 |
| 崩溃 | 批准后、prepared 后、started 后、副作用后、SQLite commit 与 artifact 发布窗口 |
| Job | 重启、休眠、DST、missed run、重复触发、重入、凭据失效、无交互审批 |
| 子智能体 | 提权失败、预算耗尽、父级取消、孤儿进程、worktree 冲突、重复 handoff |

## 9. 明确不进入 v1

Windows、远程/云沙箱、消息平台、语音、企业控制面、插件市场、Python SDK、ACP、向量记忆和自动合并子智能体改动进入 v1.x 或更后版本。M2/M3 未通过前，不提前扩大到这些功能。
