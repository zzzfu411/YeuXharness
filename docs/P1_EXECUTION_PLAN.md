# YeuX Harness P1 受保护编码闭环执行计划

状态日期：**2026-08-31**

> 注：本文是 Run 3 前的 P1 设计基线，保留当时对 runner 直连
> `WorkspaceTools` 的前置判断。当前只读 registry/runner 已按该路线完成部分
> 接线；实际完成项、测试门禁和仍未闭环的副作用管线以
> [`docs/audits/2026-09-01-run-3/EXECUTION_LOG.md`](audits/2026-09-01-run-3/EXECUTION_LOG.md)
> 为准。

Goal：在不扩大未闭环可信计算基的前提下，把 P0 的只读 Agent loop 推进为可完成“读、改、测、修”的 P1 编码闭环。所有写文件和进程副作用必须经过同一条 daemon authority 管线；任何客户端、插件、provider 或特殊内置工具都不能绕过 policy、approval、sandbox、ledger 和恢复语义。

## 1. 结论与执行顺序

P1 不能从“把 `workspace.apply_patch` 和 `process.run` 注册给模型”开始。**在本计划的设计基线（2026-08-31）中**，P0 runner 仍直接调用 `WorkspaceTools`，没有构造 `PreparedInvocation`，没有使用 `ToolExecutor` port，也没有把 policy、精确 approval、prepared token 或 sandbox 接到真实执行路径；Run 3 执行记录说明了随后已完成的只读 registry 迁移。

因此固定执行顺序为：

```text
P1-A 契约与恢复不变量
  -> P1-B sealed registry / invocation pipeline
  -> P1-C 现有只读工具迁移到唯一 pipeline
  -> P1-D 双向 approval broker
  -> P1-E workspace.apply_patch
  -> P1-F process supervisor + process.run
  -> P1-G 真实读改测修 E2E 与发布门禁
```

在 P1-A 至 P1-D 通过前，`workspace.apply_patch` 和 `process.run` 不得出现在 provider 的 ToolSpec 列表中。

## 2. 不可妥协的不变量

### 2.1 单一 authority

- tool registry 可以组合，但 policy、approval、ledger、execution permit 和恢复逻辑不可被插件替换。
- core/runtime 不得长期保留语义不同的两套 policy evaluator。
- TUI、JSONL、SDK 和测试客户端只回答交互请求，不能铸造或扩大 capability。
- provider 只能提出 tool call；不能直接控制 sandbox requirement、prepared token 或 approval binding。

### 2.2 精确且单次的 approval

Approval 必须绑定：

- `approval_id`
- `invocation_id`
- workspace ID 与 identity digest
- thread ID、turn ID、agent ID
- effective capability mode
- tool ID 与 version
- normalized arguments digest
- effect digest 与完整 granted effects
- expiry

客户端只返回 allow/deny。`ApprovalBinding` 由 daemon 使用当前 prepared invocation 铸造；客户端返回的 binding 字段一律忽略。

### 2.3 执行 permit 不可伪造或复用

- 对外可序列化的 `PreparedInvocation` 不是执行权。
- executor 只接受字段私有、by-value、单次消费的 `ExecutionPermit`。
- permit 只能在 prepare 重验、policy allow、approval validate 和 sandbox capability 检查全部成功后铸造。
- permit 过期、workspace identity 变化、参数/effect digest 变化后失效。

### 2.4 Ledger 是唯一事实源

每次 invocation 至少要能重建：

- call ID、tool ID/version
- normalized arguments 或其受控 artifact 引用
- arguments/effect/workspace identity digest
- idempotency 与 reversibility
- policy decision、effective grant 摘要、approval ID
- sandbox backend/capability 证据
- attempt、Started 时间、terminal state
- ToolResult、artifact、truncation 与 reconciliation 证据

terminal state 与 ToolResult 必须原子提交，避免出现 `Completed` 但没有结果。

### 2.5 Crash、取消与 Unknown

- `Proposed`、`Approved`、`Prepared` 可重新 prepare，但必须重验当前 workspace identity 与 digest。
- `Started` 的幂等调用可使用同一 idempotency key 恢复或重试。
- `Started` 的非幂等或终止状态无法证明的调用进入 `Unknown`，只允许 reconciliation，不自动重试。
- `Unknown` 不是“工作已完成”的普通 terminal；必须存在显式 reconcile 路径进入 `Completed` 或 `Failed`。
- 写工具或进程仍在运行时，取消不得提前写成 `Cancelled`。

### 2.6 Sandbox 与网络

- sandbox unavailable 或缺少所需能力时失败关闭。
- 模型不能设置 `SandboxRequirement`。
- P1 首版 `process.run` 固定无网络。当前 Seatbelt/bubblewrap 只能表达全 outbound 开关，不能兑现 endpoint-scoped `NetworkEffect`。
- 任意进程写范围初版只允许显式 `read_only` 或 `workspace_write`，不把 shell/argv 静态解析当作安全边界。

## 3. 分阶段实施

## P1-A：协议、状态与恢复契约

### 交付物

1. `ApprovalBinding` 增加 `invocation_id` 和 `turn_id`。
2. 修正 `InvocationState::Unknown` 与 recovery disposition 的矛盾，增加显式 reconciliation transition/API。
3. 扩展 invocation proposal/evidence 事件和 projection：tool version、call ID、args/effect digest、idempotency、approval、sandbox 与 attempt。
4. projection 校验 invocation event 的 thread/turn 与 proposal 一致。
5. 增加 ledger 原子提交 terminal state + ToolResult 的批处理入口。
6. daemon restart 将遗留 `Started` 按 idempotency 转为 retryable 或 `Unknown`，不永久留在 `Started`。

### 验收

- 修改任意 approval-bound 字段都会使 approval 失效。
- 同 thread、同参数的第二个 invocation 不能复用 allow-once approval。
- crash 在 terminal/result 提交任意边界都不会产生不一致 projection。
- replay 不调用 provider、tool、sandbox 或 approval client。

## P1-B：Sealed Tool Registry 与统一 Invocation Pipeline

### 推荐接口

```text
ToolRegistry::try_new(registered_tools)
  -> advertised_specs()
  -> resolve_exact(tool_id, version)

sealed ToolPreparer::plan(context, arguments)
  -> ToolPlan(normalized args, concrete effects, payload)

sealed ToolPreparer::revalidate(context, plan)
  -> identical args/effect digest

InvocationPipeline::invoke(request, cancellation)
  -> InvocationOutcome

sealed ToolExecutor::execute(ExecutionPermit, cancellation)
  -> ToolOutput
```

### 唯一路径

```text
resolve exact tool/version
  -> validate input schema
  -> normalize arguments
  -> prepare concrete effects
  -> verify effect is subset of ToolSpec template
  -> persist proposal
  -> resolve host/user/project/turn grant
  -> core policy
  -> approval if required
  -> revalidate workspace/args/effects
  -> validate approval
  -> check sandbox capability
  -> mint one-shot permit
  -> persist Started
  -> execute with scheduler/timeout/cancel
  -> validate/redact/truncate/artifact output
  -> atomically persist terminal + ToolResult
```

### 验收

- 未注册、重复 ID/version、无效 schema 和 template 扩权均稳定失败。
- runner 不再直接调用具体 workspace/process primitive。
- 所有 built-in tools 使用同一 pipeline；不存在只读、内部或测试专用绕过路径。

## P1-C：迁移 P0 只读工具

### 交付物

- 将 `workspace.list/read/search` 包装为 sealed registered tools。
- 保留当前严格 JSON、canonical effect、symlink/hardlink 防护和资源预算。
- read-only policy 自动 allow，但仍生成完整 PreparedInvocation/evidence。
- 保持同轮并发、按模型调用顺序入账。

### 验收

- P0 全部单元、runner、server E2E 测试不回归。
- read-only tool 不产生 approval request。
- 迁移前后 provider 可见 ToolSpec 和稳定错误码兼容。

## P1-D：Connection Interaction Broker 与 Approval

### 交付物

- 每连接 outbound request queue。
- pending server-request map、唯一 request ID、oneshot response。
- 入站 JSON-RPC command 与 response 解复用。
- request timeout、client disconnect、turn interrupt 和 daemon shutdown 传播。
- Turn 与发起连接/server-request capability 绑定。
- 无交互客户端和 JSONL fail closed。
- TUI 显示 workspace、tool、normalized args、effect、timeout、网络/写权限，并保持终端清理。

### 验收

- allow、deny、timeout、断线、重复/未知 response ID 均有测试。
- 断线后 invocation 不执行，Turn 进入明确失败/取消状态。
- daemon 忽略客户端伪造的 ApprovalBinding，只接受 bool 决策。

## P1-E：`workspace.apply_patch`

### 输入

- workspace-relative `path`
- `base_revision`
- 有界 UTF-8 replacement/content

### Effect

- canonical workspace-relative single-file read/write scope
- `IdempotentWithKey`
- `Reversible` 或在证据不足时 `Compensatable`

### 结果

- path、previous/new revision、bytes written
- 有界 diff summary
- 完整 diff 或大结果的 artifact URI

### 验收

- base revision 冲突绝不覆盖用户内容。
- absolute、`..`、symlink、hardlink、非 UTF-8 和越界 replacement 稳定失败。
- approval 后、执行前再次校验 revision、workspace identity、args/effect digest。
- 并发外部修改、崩溃和取消测试不会产生静默覆盖。

## P1-F：`process.run`

### 首版 schema

- absolute executable 或经过受控 toolchain resolver 的 executable ID
- argv
- workspace-relative cwd
- `access: read_only | workspace_write`
- timeout 与 output limit（只能在硬上限内收紧）
- 初版禁止任意 environment、stdin 和 network

### 必须先补的底层能力

1. 可证明进程树终止的 supervisor；仅 PGID kill 不足以阻止 `setsid`/`setpgid` 逃逸。
2. sandbox 内 trusted init 的 ready/go handshake：隔离建立后报告 ready，daemon 持久化 Started，再允许 target exec。
3. 无法证明 side effect 未发生或进程树已终止时，状态必须为 `Unknown`。

### 验收

- sandbox unavailable 时目标进程从未启动。
- read-only 进程不能写 workspace；workspace_write 需要 exact approval。
- timeout/cancel 后所有后代均被证明终止，否则进入 Unknown。
- stdout/stderr 独立、UTF-8/二进制边界、裁剪和 artifact 有测试。
- `..`、cwd symlink、环境注入、setuid/setgid executable 和超大 argv 被拒绝。

## P1-G：产品与发布门禁

### 真实 E2E

```text
client
  -> daemon
  -> provider asks read
  -> provider asks patch
  -> approval/request
  -> patch committed
  -> provider asks process test
  -> approval/request
  -> sandboxed test
  -> provider final answer with diff/test evidence
```

### 故障注入

- proposal 前后
- approval 前后
- prepared token 铸造前后
- sandbox ready 前后
- Started 提交前后
- side effect 完成后、terminal/result 提交前
- client disconnect、interrupt、daemon restart

### 仓库门禁

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `pnpm typecheck`
- `pnpm test`
- `pnpm build`
- schema drift、local link、task-scoped `git diff --check`

## 4. Agent 团队组织

| Agent | 当前职责 | 写入边界 |
|---|---|---|
| root | Goal owner、计划、统一 pipeline、集成、最终门禁 | 全局集成，避免覆盖设计/TUI 并行工作 |
| p1_contract_audit | 协议、policy、approval、事件、恢复不变量审计 | 先只读审计 |
| patch_tool | `workspace.apply_patch` runtime adapter 与边界测试 | `crates/yeux-runtime`，不改 runner/server/TUI |
| process_policy_audit | process/sandbox/policy 缺口与测试矩阵 | 先只读审计 |

后续按阶段释放 slot：P1-A 完成后设立 protocol/recovery agent；P1-D 设立 broker/TUI agent；P1-F 设立 supervisor/sandbox agent。任何 agent 在共享文件写入前先向 root 声明文件范围。

## 5. 完成定义

P1 只有同时满足以下条件才可关闭：

- provider 能完成真实“读、改、测、修”。
- patch 与 process 只能从统一 pipeline 执行。
- 每个副作用调用都有 exact approval、policy、sandbox 和 ledger 证据。
- 未信任 workspace、无交互客户端、sandbox 缺失和 approval 失败均关闭能力，而不是降级绕过。
- crash/restart 不会重复未知非幂等操作。
- replay 零外部调用。
- Rust、TypeScript、E2E、故障注入和文档门禁全部通过。
