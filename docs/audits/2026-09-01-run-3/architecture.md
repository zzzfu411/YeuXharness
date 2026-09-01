# YeuX Harness 安全审计架构与范围（Run 3）

审计日期：2026-09-01（Asia/Shanghai）
审计目标：`/Users/zfu/Documents/develop/YeuX/Harness` 当前工作树
HEAD：`05e02ea59f088e4f0731df3dcd94499509a64107`
分支：`main`（相对 `origin/main` 无提交差异）
工作树：存在大量用户未提交改动；本轮将“当前工作树”作为唯一审计对象，不回滚、不覆盖这些改动。
产物目录：`docs/audits/2026-09-01-run-3/`

> 注：本架构快照记录审计取样时的修复前状态；当前工作树的实施进度和
> 验证门禁以 [`EXECUTION_LOG.md`](EXECUTION_LOG.md) 及项目主文档为准。

## 1. 审计目标与结论口径

本轮针对 Run 2 之后真正接通的只读 Agent loop、工具调用、事件账本恢复、工作区边界、provider 流、IPC/TUI 和尚未接线的 P1 原语进行安全审计。目标不是列出所有“还没做的功能”，而是回答三个问题：

1. 当前入口是否存在可执行、可观察、具有真实影响的漏洞；
2. 已存在的安全原语是否真的位于生产执行路径，而不是只存在于孤立模块或测试；
3. 从 M1 只读基线进入“读、改、测、修”前，哪些工作必须按什么顺序完成，才能形成可验收的发布闭环。

严重度遵循安全审计技能的 likelihood × impact 口径。`docs/THREAT_MODEL.md` 明确把 root/equivalent、用户主动配置的恶意 provider 和同一 daemon 服务互不信任用户列为 v1 范围外，但没有单独定义同 UID 本地进程。为保持与当前“单用户、本地、私有工作区”的支持部署一致，本轮把需要额外文件写权限的同 UID/共享可写路径替换视为 conditional P1 release blocker，而不是无条件的 v1 confirmed finding；若产品纳入共享可写工作区或同 UID 对立进程，应重新评级。恶意模型输出、恶意仓库内容、恶意工具/插件请求和资源耗尽仍在范围内。

## 2. 方法与证据

### 2.1 使用的方法

- 读取 README、ARCHITECTURE、ROADMAP、P1 执行计划、威胁模型和两轮历史审计；跳过 Run 1 已确认且当前已修复的 TUI 控制序列和跨 UID socket 冒充。
- 对 Rust 四 crate、TypeScript 三包、JSON-RPC、事件/投影、provider SSE、workspace、process/sandbox、plugin host 和 CI 做静态数据流追踪。
- 用仓库测试和 lint 建立基线；用绝对 Rust 1.98 toolchain 复跑本机环境中未加入 PATH 的工具链。
- 对候选问题做最小动态复现：搜索最坏输入的独立 release harness、工作区根替换行为、输入块/能力覆盖回显行为。
- 由独立验证代理分别尝试推翻搜索 DoS、根身份/中间目录竞态和 TurnStart 输入污染结论；只有通过验证的路径计入 findings。

### 2.2 复现与验证限制

- 默认沙箱禁止测试进程监听 `127.0.0.1:0`；provider HTTP mock 用例在获得本机回环监听许可后重跑通过。该环境限制不是产品测试失败。
- 未安装 `cargo-audit`、OSV scanner、Semgrep 等依赖/静态安全扫描器，因此本轮不作 CVE 或第三方依赖清洁声明；供应链门禁进入发展计划。
- 本轮没有改动生产源代码；仅新增本目录审计文档和结构化 findings。

## 3. 应用与部署拓扑

```text
┌──────────────────────────────┐
│  用户 / 恶意仓库 / 模型输出    │
└──────────────┬───────────────┘
               │ prompt、workspace 内容、provider tool-call
               v
┌──────────────────────────────┐
│ TypeScript @yeux/tui          │
│ 交互终端 / --jsonl 客户端      │
└──────────────┬───────────────┘
               │ JSON-RPC 2.0：stdio 或 per-UID Unix socket
               v
┌─────────────────────────────────────────────────────────┐
│ Rust yeuxd（唯一 authority）                             │
│ server -> commands -> SQLite ledger/projection           │
│                    └-> TurnRunner                       │
│                        ├-> configured provider (HTTP SSE) │
│                        └-> workspace.list/read/search     │
└───────────────┬───────────────────────────────┬─────────┘
                │                               │
                v                               v
       ┌─────────────────┐             ┌─────────────────────┐
       │ SQLite/WAL ledger│             │ Workspace filesystem │
       │ append + replay  │             │ read/patch primitives│
       └─────────────────┘             └─────────────────────┘

  已存在但尚未进入 daemon authority 的边界：
  ProcessExecutor / SandboxBackend / policy / approval / grants / plugin-host
```

### 3.1 组件清单

| 组件 | 位置 | 当前职责 | 审计判断 |
|---|---|---|---|
| Wire protocol | `crates/yeux-protocol`、`packages/protocol` | JSON-RPC、schema、事件/工具/能力类型 | Rust 是权威 schema；TS 仍是手工子集，存在漂移风险 |
| Core state | `crates/yeux-core` | 状态机、policy/approval 类型、纯 projection | 原语和语义测试较完整，未等于 daemon 已调用 |
| Runtime | `crates/yeux-runtime` | ledger、workspace、provider、process、sandbox、artifact | 只读路径已实用；写/进程/凭据代理尚未闭环 |
| Daemon | `crates/yeuxd` | IPC、命令、后台 runner、事件广播 | 当前生产工具路径绕过 sealed registry/policy/approval |
| TUI | `packages/tui` | 人类终端投影、交互输入、传输 | Run 1 控制序列和 socket 身份修复已在当前树回归 |
| Plugin host | `packages/plugin-host` | 独立进程启动、manifest/hash、JSON-RPC | 未接入 daemon policy/OS sandbox；不应运行不受信插件 |
| CI | `.github/workflows/ci.yml` | Rust fmt/clippy/schema/test，TS typecheck/test/build | 基础门禁存在；缺依赖、SBOM、签名、发布/安全烟测 |

## 4. 主要数据流与信任边界

### 4.1 当前只读纵向流

1. TUI/JSONL 将 `turn/start` 发到 `yeuxd`；`server.rs` 逐行解析，单帧上限为 8 MiB。
2. `commands.rs::turn_start` 将 TurnStarted 和 UserMessage 写入 ledger，并异步启动 `TurnRunner`。
3. Runner 从投影读取 lineage，向配置 provider 发出请求；当 provider 声明 tool calls 时，在 `runner.rs:282-301` 创建 `WorkspaceTools`。
4. provider 的 tool-call JSON 在 runner 中组装；每个 turn 累计最多 32 个调用（同一模型响应可以并发启动，后续 model round 不重置该预算）、默认最多 8 轮、工具结果总量 4 MiB。
5. `persist_tool_proposals` 记录 ToolCall/InvocationProposed；随后 `spawn_blocking` 直接调用 `WorkspaceTools::execute`。
6. `workspace.search` 收集工作区文件、逐文件读取并调用 `collect_literal_matches`；结果作为 ToolResult 写入 ledger，再回灌 provider。
7. provider 文本/工具事件经 ledger 广播给订阅客户端；人类 TUI sink 清理控制字符，JSONL 保留协议原值。

### 4.2 信任边界表

| 输入/主体 | 视为不可信的部分 | 当前控制 | 仍需关注 |
|---|---|---|---|
| 模型输出 | 文本、tool name、路径、query、参数 | 工具白名单、JSON schema、资源上限 | prompt injection 可诱导昂贵搜索；副作用管线未统一 |
| 仓库内容 | 文件名、目录树、文件字节、skills/hooks | 相对路径、canonical containment、最终组件 `O_NOFOLLOW`、硬链接检查；workspace runtime 已加入 root device/inode/digest 与文件 revision live revalidate | 中间目录 TOCTOU、path-based rename 的最后 CAS 窗口仍未闭环 |
| provider | SSE 分片、事件、tool-call JSON | HTTP timeout、SSE/事件/累计输出上限 | configured provider 在 v1 是信任边界；decoder 算法仍可优化 |
| TUI/JSON-RPC 客户端 | 参数、重连、慢读者 | UUIDv7、初始化顺序、8 MiB 帧、命令去重、共享 command gate | 同 UID 客户端不是独立安全主体；响应无总字节上限 |
| SQLite 历史 | 事件 payload、lineage、重复 ID | append-only、事务、纯 projection | lineage/context 无总量预算；terminal/result 生产路径非原子 |
| plugin/MCP | manifest、能力请求、输出 | plugin manifest 校验/hash、独立进程 | 未接 policy/approval/sandbox/进程树，协议主版本不一致 |
| 进程/沙箱 | executable、argv、环境、后代 | primitive 默认清空环境、输出/timeout、不可用失败关闭 | ProcessExecutor 未由 daemon 暴露；PGID 不能证明所有后代终止 |

## 5. 安全控制成熟度矩阵

| 控制目标 | 当前状态 | 证据 | 发布含义 |
|---|---|---|---|
| 事件账本与 replay | 已实现但有集成缺口 | `yeux-runtime/src/ledger.rs` append/replay；golden trace | replay 不重调外部系统；runner 仍需原子 terminal+result |
| 只读工具路径 | 可用 | `workspace_tools.rs` 三工具、严格参数/预算 | M1 可用；搜索算法 DoS 需先修 |
| Tool registry | 原型存在、生产未接 | `yeuxd/src/tools.rs`；runner 直接持有 `WorkspaceTools` | 所有未来副作用必须阻止旁路 |
| Capability/policy | 类型与 evaluator 存在，grants 未纳入 module graph/runner | `yeux-core`、`yeuxd/grants.rs` | 不能把声明当成执行授权 |
| Approval broker | 协议/绑定原语存在，daemon outbound broker 缺失 | `yeux-core/approval.rs`、server/commands | M2 阻断；不能开放写/进程 |
| Workspace containment | 基本防护已实现，live identity/revision 部分落地 | `workspace.rs` validate/canonicalize/nofollow；`identity_snapshot`/`revalidate_identity`；`revision_snapshot`/`revalidate_revision` | 需 dirfd/openat2 与 dirfd-relative publish 才能封闭 intermediate symlink 和最后 CAS 窗口 |
| Provider 资源 | 多数有硬上限 | `provider.rs` StreamAccounting/SSE caps | 恶意 configured provider 非 v1 finding；仍需超时/算法加固 |
| Sandbox/process | 原语质量较好，未接入 | `process.rs`、`sandbox.rs` | 沙箱不可用应继续 fail closed；保持 process 工具隐藏 |
| TUI sink | Run 1 修复已接入 | `packages/tui/src/terminal.ts` 及回归测试 | 需保持 JSONL 原值与 human sink 分离 |
| Plugin | hash/manifest 基线 | `packages/plugin-host/src/manifest.ts` | 未接入 authority 前禁止不受信插件 |
| CI/release | 基础工程门禁 | `.github/workflows/ci.yml` | 缺依赖审计、SBOM、签名、可复现构建和 release smoke |

## 6. 攻击面优先级

### P0（当前可触发）

- provider tool-call → `workspace.search` 的输入与算法复杂度；这是本轮唯一计入 findings 的问题。
- 模型/仓库内容进入 provider 上下文；需保持 query、文件、输出和总轮次预算。

### P1（当前可执行但必须在发布前闭环）

- runner 直接调用 `WorkspaceTools::prepare_effects/execute`，绕过 `ToolRegistry`、policy、approval、permit 和统一 scheduler。
- ledger 已有 `append_invocation_outcome`，但 runner 先写状态再写 ToolResult，崩溃窗口会产生不一致。
- workspace 根 identity 未在每次 runner 初始化/执行前与持久化 digest 比对；中间目录只保护最终路径组件。
- `spawn_blocking` worker 没有实际使用 ToolSpec timeout，取消只能阻止结果入账，不能终止 CPU 工作。
- apply_patch 最终校验到 rename 仍是路径级 TOCTOU；process/sandbox/credential broker 未进入 authority。

### P2（产品/发布成熟度）

- lineage、TurnStart、响应和连接缺少统一语义预算；capabilities 仍将未实现的 jobs/plugins 报为 true。
- plugin protocol major 1 与 daemon `PROTOCOL_VERSION` 2 不一致；无 OS sandbox/进程树治理。
- CI action 使用浮动 major/stable 引用；无 RustSec/npm audit、license allowlist、SBOM、provenance、签名和 soak。

## 7. 阶段判断

当前最准确的产品定位是：**M1 只读工程基线，尚未达到 M2 受保护编码闭环，也不是可用于自主“读、改、测、修”的 v0.1 发布版。**

已具备的优势：Rust/TS 分层清楚；事件追加与纯 replay 设计正确；单 Thread active Turn、命令去重、schema drift、路径基础防护、provider 多重资源上限和 TUI sink 修复都有测试；没有发现当前入口到 shell、写文件、网络或插件执行器的权限逃逸。

必须先完成的门槛：修复搜索 CPU DoS；将所有工具接到不可旁路的 prepare → policy/capability → approval → sandbox → execute → evidence 管线；把 live workspace identity、dirfd/CAS、取消/Unknown/原子结果和进程树终止做成可证明的状态；然后再开放 apply_patch 与 process.run。

## 8. 验证证据索引

| 证据 | 结果 |
|---|---|
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo test --workspace --all-targets` | 173 项通过；provider loopback mock 在允许绑定后通过 |
| `cargo run -p yeux-protocol --example export_schemas -- --check` | 应作为 CI/收尾门禁复跑 |
| `pnpm typecheck` | 通过 |
| `pnpm test` | 51 项通过（protocol 9、TUI 38、plugin-host 4） |
| `pnpm build` | 通过 |
| 独立搜索基准 | 1 MiB 文件约 110 ms；32 MiB、4096 字节共同前缀约 2.66 s；32 并发 32 MiB 扫描约 14.36 s（本机 ARM release harness） |
| Run 1 回归 | TUI 控制字符与跨 UID socket 冒充修复在当前树保留 |

测试命令、时间和限制将在 `REPORT.md` 收尾部分再次列出；本文件只提供架构与范围，不替代 finding 细节。
