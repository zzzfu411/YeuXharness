# YeuX Harness 完整安全审计报告（Run 3）

审计日期：2026-09-01（Asia/Shanghai）
审计目标：`/Users/zfu/Documents/develop/YeuX/Harness` 当前工作树
审计提交基线：`05e02ea59f088e4f0731df3dcd94499509a64107`
审计类型：源代码审计 + 动态复现 + 独立对抗性验证
输出目录：`docs/audits/2026-09-01-run-3/`

> 注：本文是 Run 3 修复前的冻结审计证据；其中的旧行号、旧实现描述和
> 测试数字用于复现当时的 finding，不代表当前工作树。修复后的实施状态、
> 最新门禁与剩余阻断项以同目录 [`EXECUTION_LOG.md`](EXECUTION_LOG.md) 为准。

## 执行摘要

结论先行：YeuX Harness 已经形成一个质量不错的 **M1 只读 Agent engineering baseline**，但还没有达到可以安全完成真实“读、改、测、修”的 M2/v0.1 发布门槛。本轮确认 1 个可达的算法复杂度型 CPU 拒绝服务问题（`YX-2026-003`，LOW，定向本地可用性）；没有确认当前入口可触发的 shell 执行、写文件越权、跨 UID socket 冒充、provider 注入或 HIGH/CRITICAL 权限突破。

这不是“可以直接发布”的结论。当前最重要的风险并非缺少某个单独 API，而是若干安全原语尚未位于同一条不可旁路的生产执行路径：runner 直接调用 `WorkspaceTools`，没有经过已经存在的 sealed registry、capability/policy、approval broker、permit、sandbox 和统一 outcome/reconciliation。`workspace.apply_patch`、`process.run`、网络和插件因此仍应保持隐藏。根目录 identity live revalidate、中间目录 TOCTOU、terminal/result 原子入账、取消/Unknown、上下文预算和进程树终止也必须在开放副作用前完成。

Run 1 确认的 TUI 控制序列注入和条件性跨 UID Unix socket endpoint 冒充，已在当前工作树接入修复并通过相应回归；本轮没有重复报告它们。所有本轮产物只写入审计目录，未修改生产源代码。

## 1. 结果总览

| ID | 严重度 | 状态 | 优先级 | 核心影响 |
|---|---|---|---|---|
| YX-2026-003 | LOW（单用户本地口径；共享 daemon 时需重评） | Confirmed | P0 热修 | 恶意仓库/模型输出可使 `workspace.search` 进行数十亿次重复比较，拖慢或暂时占满 daemon CPU；取消不能立即回收 worker |

### 1.1 发布判断

| 发布目标 | 判断 | 原因 |
|---|---|---|
| 协议/运行时开发、受控只读探索 | 可以（修复 DoS 后更稳妥） | 只读工具白名单、路径基础防护、ledger/replay、provider/TUI 预算和测试已存在 |
| 面向单用户的只读 beta | 有条件 | 需先修 YX-2026-003，补充并发/取消/长上下文测试，并明确“只读、无副作用”边界 |
| 自主编码 v0.1（读、改、测、修） | 不可以 | 统一 invocation pipeline、approval、sandbox、CAS/live identity、Unknown/reconciliation、process supervisor 尚未闭环 |
| 多用户共享 daemon/远程执行 | 不可以 | 当前威胁模型明确排除该部署；身份、租户、资源隔离和 IPC 认证均未设计为该目标 |

## 2. 审计范围、假设与方法

### 2.1 范围

- Rust：`yeux-protocol`、`yeux-core`、`yeux-runtime`、`yeuxd`。
- TypeScript：`packages/protocol`、`packages/tui`、`packages/plugin-host`。
- JSON-RPC/stdio/Unix socket、SQLite append/replay、Thread/Turn/Item 状态、provider OpenAI-compatible SSE、workspace list/read/search/apply_patch、process/sandbox 原语、plugin manifest/host、CI/release。
- 当前工作树的未提交改动也在范围内；历史审计目录只用于比较，不作为当前代码证据。

### 2.2 威胁模型口径

依据 `docs/THREAT_MODEL.md`：模型输出、仓库内容、工具输出、MCP/plugin 请求、prompt injection、symlink/path traversal、资源耗尽、取消/崩溃窗口均不可信且在范围内；root/equivalent、用户主动配置的恶意 provider，以及同一 daemon 服务互不信任用户则是 v1 范围外。文档没有单独定义同 UID 本地进程，因此本轮采用当前支持部署的保守假设：只有拥有额外 workspace 父目录写权限的本地 actor 才能触发根替换/中间目录竞态，这类条件性行为不冒充无条件跨用户漏洞，而列为 P1 发布阻断。若产品允许共享可写 workspace 或把同 UID 对立进程纳入模型，应在 M2 前更新威胁模型并重新分类。

### 2.3 执行步骤

1. 阅读 README、架构/路线图/P1 执行计划、威胁模型和 Run 1/Run 2 产物。
2. 建立组件、信任边界、数据流和输入面清单（见 `architecture.md`）。
3. 逐行追踪 provider → runner → tool → workspace → ledger/TUI 的生产路径，另查状态机、IPC、文件/进程/插件旁路。
4. 运行 Rust/TypeScript 编译、lint、测试和 schema drift 检查；对 loopback mock 的沙箱限制做单独记录。
5. 用独立 release harness 动态验证搜索最坏复杂度；用独立代理尝试推翻 DoS、identity/TOCTOU、TurnStart 污染与 interrupt race。
6. 只把具备可执行攻击者、明确入口到 sink、真实影响并通过独立复核的路径写入 confirmed findings。

### 2.4 工具限制

本机未找到 `cargo-audit`、OSV scanner、Semgrep 等依赖/静态安全扫描器，故不对第三方依赖的 CVE、许可证或 SBOM 做“无问题”声明；这些检查列入 `DEVELOPMENT_PLAN.md` 的发布门禁。默认沙箱禁止测试监听 `127.0.0.1:0`，provider httpmock 三项在获得本机回环监听许可后全部通过。

## 3. 架构与安全边界摘要

### 3.1 当前真实链路

```text
TUI/JSONL
  -> JSON-RPC server (8 MiB frame, UUIDv7, command gate)
  -> commands: workspace/thread/turn
  -> SQLite append-only ledger + projection
  -> TurnRunner
  -> configured OpenAI-compatible provider (SSE)
  -> workspace.list/read/search (当前唯一生产工具)
  -> ToolResult/Invocation events
  -> provider continuation + event subscription
```

`crates/yeux-runtime` 中的 `ProcessExecutor`、`SandboxBackend`、artifact、policy/approval 类型和 `crates/yeuxd/src/tools.rs` registry/plan/revalidate 原型是有价值的基础，但“存在于源码”不等于“生产 authority 已强制使用”。当前 `crates/yeuxd/src/lib.rs` 也没有把 `grants.rs` 纳入 module graph；capability 交集尚未成为 runner 的实际决策来源。

### 3.2 关键正向控制

- Rust protocol/core/yeuxd library 和 runtime crate 配置 `forbid(unsafe_code)`；本轮未发现生产 `unsafe` 块。
- SQLite events 追加式、Thread `seq` 单调、replay 不调用 provider/tool/network；golden trace 覆盖跨重启去重。
- JSON-RPC 单帧 8 MiB；provider 有错误体、SSE buffer/stream、事件、累计输出和 tool-call 状态上限。
- workspace 拒绝绝对路径、`..`、叶 symlink/多硬链接，并校验 canonical containment；只读工具对文件数、深度、单文件、累计扫描、匹配和输出有上限。
- 只读模型工具是固定注册集；未知/未协商工具不会落到 shell、写入、网络或插件执行器。
- TUI 人类 terminal sink 已清理 ANSI/OSC、C0/C1 和 bidi 控制字符，同时 JSONL 保留原始协议 payload；socket fallback 已改为私有 per-UID 目录并做 owner/mode/type/device/inode 检查。
- Process/Sandbox 原语默认 `shell=false`、`env_clear`、输出/超时上限，沙箱不可用时失败关闭；launcher 与目标环境已分离。

## 4. Confirmed finding

### YX-2026-003：`workspace.search` 朴素匹配导致 CPU 拒绝服务

**严重度：LOW（当前单用户本地 daemon）**
**CWE 参考：CWE-407（Inefficient Algorithmic Complexity）**
**置信度：HIGH**

#### 位置与链路

1. `crates/yeuxd/src/runner.rs:282-301` 在 provider 声明 tool calls 时建立 workspace 工具；`:447-463` 接收调用并执行每个 turn 累计最多 32 个工具调用的预算（同一模型响应可并发启动，后续 round 不重置）。
2. `crates/yeuxd/src/runner.rs:518-536` 为每个 invocation 创建 `spawn_blocking` worker；`:540-581` 逐个等待结果，没有执行 `ToolSpec.timeout_ms`。
3. `crates/yeux-runtime/src/workspace_tools.rs:558-587` 逐文件读取，约束 query 4 KiB、单文件 1 MiB、累计扫描 32 MiB 等资源，但把每个字节仍交给朴素 matcher。
4. `crates/yeux-runtime/src/workspace_tools.rs:900-929` 对每个 offset 比较完整 `needle`：`&haystack[offset..offset + needle.len()] == needle`。

#### 攻击场景与动态证据

攻击者把重复字符文件和诱导性提示放进仓库，使正常 provider 的模型输出 `workspace.search`。准备 32 个各 1,048,576 字节、内容全为 `a` 的普通文件，query 设为 `a` 重复 4,095 次后接 `b`（合法 4,096 字节、无匹配）。每个文件约有 1,044,481 个候选 offset，每次比较约失败在最后一个字节，合计约 `4.28×10^9` 次字节比较；`max_matches` 不会提前终止。

独立 aarch64 release harness（抽取等价循环，未修改仓库源代码）测得：1 MiB 约 110 ms，32 MiB 约 2.66 s；32 个等价并发扫描约 14.36 s。内存主要是每个 worker 的约 1 MiB 读缓冲，影响核心是 CPU。默认 `max_model_rounds=8`、`max_tool_calls=32`（`runner.rs:32-49`、`main.rs:52-62`）；interrupt 在 `runner.rs:541-553,584-595` 只停止结果继续入账，已运行的同步 worker 仍会计算。

#### 影响、边界与为什么不是 HIGH

可观察影响是 daemon CPU 饱和、后续 RPC/订阅延迟、turn 超时和取消止血失败；没有证据表明该路径越过 workspace containment、执行 shell、写文件或访问其他用户数据。攻击需要用户打开受影响仓库并启动 tool-capable turn，不是匿名远程入口，且当前产品是单用户 daemon，所以按“定向本地 DoS”评级 LOW。若未来一个 daemon 服务多个互不信任用户、后台 Job 无用户在场，或把本机可用性设为高保障目标，应升为至少 MEDIUM 并重新做资源隔离评估。

#### 修复要求

- 用 `memchr::memmem`、Two-Way 或 Aho–Corasick 等线性/子线性算法替换逐 offset 全量比较。
- 为扫描循环注入 cooperative deadline/cancellation 和操作计数；超时/预算耗尽返回稳定错误，不把仍在运行的 worker 标成完成。
- 在 daemon 统一 executor 增加 per-daemon/per-workspace semaphore、每 turn CPU/search budget，并真正执行 `ToolSpec.timeout_ms`。`tokio::time::timeout` 只能限制等待；硬截止需可终止的隔离 worker/子进程。
- 增加长共同前缀、32-call 并发、取消、超时和 replay 不变的 macOS/Linux 回归测试。

完整 trace、payload、验收标准见 [`FINDINGS-DETAIL.md`](FINDINGS-DETAIL.md) 和 [`findings.json`](findings.json)。

## 5. 经独立验证但不计为漏洞的项目

这些项目不是“忽略”，而是按当前威胁模型和实际调用路径分开标注，避免把设计边界或未来风险误报成漏洞。

| 项目 | 当前判定 | 发布前要求 |
|---|---|---|
| Turn interrupt race | REJECTED | `server.rs:521-529` 与 runner 共用 `command_gate`，正常路径被串行化；继续补充取消/崩溃窗口测试 |
| 根目录 identity 替换 | REJECTED（当前部署假设；P1 blocker） | `workspace_open` 保存 digest，但 runner 只按 root 字符串 reopen；同 UID/共享可写 actor 可稳定读到替换目录。应在 RunContext、每次 invocation 和 patch 前做 live device/inode/digest revalidate；若共享可写 workspace 纳入威胁模型，升级为条件性 confirmed finding |
| 中间目录 symlink TOCTOU | REJECTED（当前部署假设；P1 blocker） | `canonicalize` 后仅最终组件 `O_NOFOLLOW`；同 UID/共享可写 actor 可在两步之间把中间目录换成外部 symlink。应改为 dirfd/openat2 等价方案并做竞态测试；若该 actor 在威胁模型内，升级为条件性 confirmed finding |
| TurnStart forged blocks/capability override | REJECTED | override 当前不进入 policy，伪造 ToolCall 只会污染 provider 兼容性；增加 ContentBlock/lineage/总上下文预算和严格角色 schema |
| SSE decoder 重复 windows 扫描 | REJECTED（恶意 configured provider 出界） | 保留现有 8 MiB/64 MiB/事件上限；未来优化增量边界扫描、响应取消和 provider contract tests |

## 6. P1 发布阻断与根因分析

### P1-A：事件与恢复一致性

`EventLedger::append_invocation_outcome` 已能原子提交 terminal state + ToolResult（`crates/yeux-runtime/src/ledger.rs:315-421`），但生产 runner 仍在 `runner.rs:623-631` 先持久化状态、再在 `:777-801` 写 ToolResult。崩溃或第二次写失败会留下 terminal invocation 而没有模型可见结果。取消也可能在副作用仍未知时过早呈现 `Cancelled`。必须把 outcome batch、Started→Unknown 和 reconciliation 接入生产路径。

### P1-B/C：权威管线与能力交集

`crates/yeuxd/src/tools.rs` 已有 sealed registry、plan/revalidate 和 permit 方向，但 runner 在 `runner.rs:287-291,688-689,534-536` 直接持有并执行 `WorkspaceTools`。因此 ToolSpec timeout、effect template、grants 和 policy 测试不自动保护生产路径。必须让所有 built-in tools 经过：精确 registry/version → schema/normalize → concrete effects → capability intersection → approval → live revalidate → one-shot permit → sandbox/scheduler → bounded output → atomic outcome。

### P1-D：Approval/交互 broker

协议和 core 已有 ApprovalBinding 语义，daemon 尚无 outbound request queue、pending map、response 解复用和断线/超时策略。客户端不得提交自带 binding 来取得 authority；daemon 必须自己铸造并单次消费批准，JSONL/无头模式在需要人工批准时应失败关闭或进入明确等待状态。

### P1-E：安全写入

`workspace.apply_patch` 已做 base revision、临时文件、重解析和目录同步，但 `crates/yeux-runtime/src/workspace.rs:252-315` 仍是路径级检查到 rename 的 TOCTOU。接入前需要 dirfd/CAS、live root identity、权限/链接再校验、崩溃恢复和冲突不覆盖验收。

### P1-F：进程、沙箱、凭据和网络

`ProcessExecutor`/`SandboxBackend` 默认值是良好起点（无 shell、清空环境、输出/超时、fail closed、launcher 环境分离），但 daemon 尚未暴露 `process.run`；PGID kill 不能证明主动脱组的所有后代已终止。`CredentialBroker` 与统一网络代理也未接线。没有 sandbox-ready/go handshake、进程树证明和 Unknown 状态前，不应开放测试命令、任意环境、stdin 或网络。

### P1-G：插件与协议兼容

`packages/plugin-host/src/manifest.ts:52-55` 仍接受协议 major 1，而 host 在 `plugin-host.ts:119-128` 发送当前 `PROTOCOL_VERSION` 2；此外插件没有 daemon policy/approval/OS sandbox/完整进程树治理。它应继续标为实验宿主，直到协议、权限、摘要到执行的原子性和隔离闭环。

## 7. P2 工程与发布成熟度

- `turn/start` 接受有界单行但没有完整的总 context/lineage/response-byte 预算；长 fork 谱系可放大 CPU、SQLite 和 provider token 成本。
- `initialize` 目前把 `jobs:true`、`plugins:true` 作为能力返回，但 `job/run` 仍返回 `FEATURE_UNAVAILABLE`，插件也未接入 authority；应按真实可用性返回或加明确实验标志。
- `.github/workflows/ci.yml` 已有 macOS/Linux Rust 与 Ubuntu TypeScript 基础门禁，但 action 使用浮动 major/stable 引用，缺 RustSec/npm audit、license allowlist、SBOM/provenance、可复现构建、签名、发布 smoke 和 24 小时 soak。
- TS 协议类型仍是手工子集；Rust schema 与 TS 类型应生成/校验同源版本，避免兼容性和安全字段漂移。
- plugin-host/直接 daemon stderr 是独立终端 sink；进入受支持交互入口前应采用与 TUI 等价的清理或结构化日志。

## 8. 建议的优先顺序（概要）

详细周计划、依赖、负责人、验收和回滚策略见 [`DEVELOPMENT_PLAN.md`](DEVELOPMENT_PLAN.md)。最短安全路径是：

1. **立即（0–48 小时）**：修 YX-2026-003；加入算法、取消和并发预算回归；暂时把 workspace.search 并发上限压到安全值。
2. **第 1 周**：完成 P1-A outcome 原子性、Unknown/reconciliation、live workspace identity 和输入/context budget。
3. **第 2–3 周**：完成 P1-B/C，令 registry/policy/grants/permit 成为唯一工具路径；只读工具先迁移并保持兼容。
4. **第 4 周**：完成 P1-D approval broker 与 TUI/JSONL parity。
5. **第 5 周**：完成 P1-E apply_patch dirfd/CAS 和冲突恢复；此后才考虑打开写工具。
6. **第 6–7 周**：完成 P1-F process supervisor/sandbox-ready/Unknown；在真实测试仓库完成读、改、测、修 E2E。
7. **第 8 周及以后**：M3 provider/context/plugin、M4 jobs/subagents、M5 migration/SBOM/signing/soak，按阶段门槛而不是文件数量发布。

## 9. 验证结果

在当前工作树上完成或已安排的门禁：

| 检查 | 结果 |
|---|---|
| Rust `cargo fmt --all --check` | 通过 |
| Rust `cargo clippy --workspace --all-targets -- -D warnings` | 通过，0 warning |
| Rust `cargo test --workspace --all-targets` | 173 项通过（含 provider loopback mock；受限沙箱首次失败后在允许 localhost bind 的本机环境重跑） |
| TypeScript `pnpm typecheck` | 通过 |
| TypeScript `pnpm test` | 51 项通过：protocol 9、TUI 38、plugin-host 4 |
| TypeScript `pnpm build` | 通过 |
| 独立搜索复杂度 harness | 最坏输入与并发放大复现 |
| findings schema validator | 收尾阶段复跑，必须为 PASS |

还应在最终合并前复跑：`cargo run -p yeux-protocol --example export_schemas -- --check`、`git diff --check`、两平台 CI、依赖/许可证扫描和真实只读 E2E。上述测试通过只说明当前基线没有编译/单测回归，不替代 P1 安全门槛。

## 10. 限制与后续审计

本轮覆盖的是当前 M1 只读路径和 P1 原语接线状态，不是完整渗透测试；没有把恶意 configured provider、多用户 daemon、root 主机控制或远程沙箱纳入结论。每次接通 apply_patch、process、approval、MCP、plugin、jobs/subagents 都应新开独立审计运行，并重做崩溃、资源、权限继承和 replay 零外部调用测试。建议在 P1-G E2E 完成后立即运行下一轮，重点验证“批准后—启动前—副作用后—ledger commit”每个窗口。

## 最终意见

YeuX 的架构方向（Rust authority、append-only/replay、能力交集、OS sandbox、结构化工具）是可持续的，当前测试基础也明显优于仅有 UI demo 的项目。真正的交付风险在于“原语已经写好”与“生产执行路径不可绕过”之间仍有距离。按本报告和发展计划完成 P0 DoS 修复及 P1-A～G 门槛后，项目才具备把写入和进程能力交给模型的证据链；在此之前，应明确保持只读、隐藏副作用工具，并把任何能力声明与实际可用性对齐。
