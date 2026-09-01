# YeuX Harness 项目完成计划与发展规划

版本：Run 3 审计后的执行版
日期：2026-09-01
目标：把当前 M1 只读工程基线推进到可审计、可恢复的 v0.1“读、改、测、修”闭环，再按 M3–M5 扩展 provider、插件、自动化和发行成熟度。

## 1. 完成定义（Definition of Done）

项目不能以“模块存在”或“demo 能跑”作为完成标准。v0.1 只有同时满足以下条件才算完成：

1. 用户在真实测试仓库中可从 TUI 和 JSONL 完成 `read → apply_patch → test → fix`。
2. 所有工具（包括未来插件/MCP）都走同一条不可旁路的 `registry → validate/normalize → prepare effects → capability intersection → approval → live revalidate → one-shot permit → sandbox/scheduler → bounded output → atomic ledger outcome` 管线。
3. stale revision、workspace identity 变化、沙箱不可用、审批拒绝、超时、取消、断线和 daemon 崩溃都不会静默扩大权限、覆盖用户内容或重复未知非幂等副作用。
4. replay 只读取事件，不调用 provider、工具、网络、审批客户端或进程；重启后遗留 Started 调用会进入可解释的 `Unknown`/reconciliation，而不是自动重试。
5. TUI 人类终端与 JSONL 的投影语义一致；敏感输出有删改、截断、artifact 引用和审计证据。
6. macOS/Linux CI 通过格式、lint、schema drift、单元/集成/E2E、故障注入、依赖/许可证/SBOM、签名和 release smoke 门禁。

当前状态（执行记录见 [`EXECUTION_LOG.md`](EXECUTION_LOG.md)）：M1 只读 loop 已贯通，`workspace.search` 的 CPU DoS 已以线性 KMP、协作式预算/取消和 daemon 级并发闸门止血；P1-A 账本原子 outcome、Unknown/recovery、上下文预算和 workspace live identity 已接入，P1-B/C 已完成只读 registry 迁移基础。副作用统一管线仍未闭环，因此距离 v0.1 仍是多阶段工程，不应提前开放写入或进程工具。

## 2. 交付原则

- **安全边界先于功能数量**：先固定状态、权限、取消和证据，再增加工具。
- **单一路径**：禁止 runner、测试专用入口或插件 host 直接调用 runtime primitive 绕过 policy/approval/sandbox。
- **有界且可取消**：所有输入、CPU、内存、并发、输出、lineage 和进程树都有硬预算；“取消”必须对应可证明的停止或 `Unknown`。
- **默认隐藏副作用**：任一阶段门槛失败，工具保持未注册/不可协商；不以 prompt 约束代替执行层约束。
- **可回滚的小步提交**：协议先兼容扩展，数据迁移可逆，feature flag 默认关闭；每阶段独立 PR 与审计证据。
- **证据驱动**：每项工作都有代码位置、测试、失败模式和可复现验收，不用“以后补测试”作为完成理由。

## 3. 依赖关系与路线总览

```text
P0-Search/资源止血 ─┬─> P1-A 状态/恢复/预算 ─> P1-B Registry/Pipeline
                    │                         ├─> P1-C 迁移只读工具
                    │                         └─> P1-D Approval Broker
                    └─> P1-E Workspace CAS/patch ─┐
                                                  └─> P1-F Process/Sandbox
P1-A/B/C/D/E/F ─────────────────────────────────────> P1-G 真实 E2E + 发布门禁
P1-G ─> M3 Provider/Context/Plugin ─> M4 Jobs/Subagents ─> M5 Release hardening
```

以下日程按 1 名全职工程师约 8–10 周估算；2–3 人并行可缩短日历时间，但每个安全门槛仍需独立 reviewer 和双平台验证。若只有零碎时间，应按依赖顺序切片，不并行开放未完成的副作用能力。

## 4. 0–48 小时：P0 止血与基线冻结

### 4.1 必做任务

| ID | 工作 | 代码/文档区域 | 完成标准 |
|---|---|---|---|
| P0-01 | 替换 `collect_literal_matches` 朴素算法 | `crates/yeux-runtime/src/workspace_tools.rs` | 长共同前缀不再产生乘 query 长度的复杂度；结果/行列号兼容 |
| P0-02 | 搜索 cooperative budget/cancellation | `workspace_tools.rs`、`yeuxd/src/runner.rs` | budget、deadline、取消均返回稳定错误/状态；无法证明停止时收束为 `Unknown`，硬终止留给 supervisor |
| P0-03 | 限制搜索并发 | runner scheduler | per-daemon/per-workspace semaphore；32 个恶意调用不耗尽 blocking pool（当前默认 daemon 4 槽、同一 workspace search 1 槽） |
| P0-04 | 让 `ToolSpec.timeout_ms` 生效 | runner/统一 executor | 超时测试证明状态和资源回收；不只包 `JoinHandle` 等待 |
| P0-05 | 固定 adversarial regression | runtime/daemon tests、CI | 1 MiB/32 MiB 长前缀、32-call、取消/超时用例在 macOS/Linux 通过 |
| P0-06 | 冻结当前基线 | `docs/audits/...`、CHANGELOG/issue | 记录修复前后基准、错误码、兼容性和 feature flag；未修复时保持只读提示 |

### 4.2 止血期间的产品开关

- 默认将同一 workspace 的 `workspace.search` 并发设为 1，并给每个 turn 设置较保守的累计搜索 operation budget；修复合入后再通过配置逐步放宽（daemon 总闸门仍为 4）。
- 保留 `apply_patch`、`process.run`、网络、MCP、plugin tool 未注册状态；不要用命令行开关绕过统一管线。
- 若无法在 48 小时内证明硬截止，宁可把超时调用置为 `Unknown` 并交由后续隔离 supervisor 终止，也不要报告为成功/已取消；`spawn_blocking` 本身不可强杀。

## 5. P1-A（第 1 周）：契约、账本与恢复

### 目标

使每个 invocation 的事实、结果和恢复状态可重放、可审计、可解释。

### 工作包

1. 统一 `InvocationOutcome` batch：生产 runner 使用 `EventLedger::append_invocation_outcome`/`append_batch`，terminal state 与 ToolResult 同事务提交。
2. 明确状态机：`Proposed → Approved → Prepared → Started → Completed|Failed|Cancelled|TimedOut|Unknown`；只有副作用停止证据充分时才可进入 Cancelled。
3. 启动时扫描遗留 Started：只读调用可安全标失败；未知非幂等调用进入 reconciliation queue，禁止静默重试。
4. 给每个事件保存 tool/version、call ID、normalized args digest、effect digest、grant/approval/sandbox/attempt 和 causation；projection 校验 thread/turn/agent 一致性。
5. 为 TurnStart、steer、title、lineage、响应和 provider context 增加独立的元素数、字节数、深度和累计 token 预算；超限返回稳定错误。
6. RunContext 在初始化、每次 invocation prepare/revalidate/commit 前比对持久化 workspace identity；记录 device/inode/digest 变化原因。
7. 增加 faux clock、可注入 ID、crash point 和 replay counter，证明 deterministic trace。

### 验收门槛

- 在 terminal/result 任意提交边界注入崩溃，重启后只看到完整 batch 或完整 Unknown，不出现 terminal 无 ToolResult。
- replay 外部调用计数严格为零；重复 event ID 幂等，部分 batch 重放被拒绝。
- 修改 workspace、tool、args、effects、turn/agent 任一 approval-bound 字段都会失效；第二次 invocation 不能复用一次性批准。
- 根目录替换、中间目录 symlink 竞态、stale revision 测试均失败关闭，不读写 workspace 外。

## 6. P1-B（第 2 周）：Sealed Tool Registry 与统一 Invocation Pipeline

### 目标

让 authority 真正强制执行能力交集，而不是依赖调用方守规矩。

### 设计与实施顺序

1. 将 `crates/yeuxd/src/tools.rs` 的 registry 原型收敛成不可变 `ToolRegistry`：唯一 `tool_id + version`、schema、effect template、concurrency、timeout、output budget。
2. 把 `grants.rs` 纳入 module graph；从 host ceiling、user profile、project trust、turn override 计算交集。缺省层是 identity，不得因为缺失而意外扩大权限。
3. 实现 `InvocationPipeline::invoke`，内部顺序固定为：resolve exact → validate/normalize → prepare effects → template subset → persist proposal → policy → approval → live revalidate → one-shot permit → scheduler/sandbox → bounded/redacted output → atomic outcome。
4. 将 permit 设计为不可 Clone/Deserialize、单次消费、绑定 invocation/effect/expiry；任何客户端传来的 binding 只作为决策输入，不能铸造 authority。
5. 删除 runner 对 `WorkspaceTools::prepare_effects/execute` 的直接依赖；测试和内部 helper 也只能通过 pipeline。

### 验收门槛

- 未注册、重复版本、schema 错误、effect 扩权、过期 permit、错误 workspace/agent/turn 都稳定失败。
- 静态检查/代码审查证明生产 runner 没有直接 runtime tool call；可用 grep/deny-list 作为 CI 辅助门禁。
- 所有工具均产生完整 proposal/approval/start/terminal/evidence；没有“只读所以跳过账本”的旁路。

## 7. P1-C（第 2–3 周）：迁移 M1 只读工具

### 工作

- 将 `workspace.list/read/search` 包装为 sealed registered tools；保留现有严格 JSON、路径/链接防护、资源预算和稳定错误码。
- read-only policy 可自动 allow，但仍创建 PreparedInvocation 和 effect evidence；统一 scheduler 管理并发、timeout、cancel 和 output。
- provider 可见 ToolSpec 从 registry 单源生成，保持版本/字段兼容；未知工具只形成有界错误结果。
- 为结果按模型 call 顺序入账、乱序完成、重复 call ID、provider 断流和重启补发增加 E2E。

### 验收门槛

真实 `client → daemon → provider → list/read/search → provider → answer` 通过；TUI/JSONL 投影一致；取消、背压、replay、schema drift 不回归；搜索 P0 基准持续通过。

## 8. P1-D（第 3–4 周）：Approval/Interaction Broker

### 工作

1. daemon 每连接 outbound request queue、pending request map、唯一 request ID、oneshot response 和超时清理。
2. 把 approval 请求绑定到 workspace identity、invocation、thread、turn、agent、mode、tool/version、args/effects digest、expiry；daemon 自己铸造并校验。
3. TUI 显示 workspace、工具、规范化参数、effect、timeout、网络/写权限和风险；继续在 human sink 清理控制字符，JSONL 只输出结构化原值。
4. 处理 allow/deny/timeout/断线/重复或未知 response ID、turn interrupt、daemon shutdown；无交互客户端在需要批准时失败关闭或明确进入 waiting 状态。

### 验收门槛

- 客户端伪造 binding、改参数或重放 response 都不能取得 permit。
- 断线/超时后 invocation 不执行；审批结果与 ledger 事件同事务可追溯。
- 同一用户同时开多个 turn 时不会串用 pending approval。

## 9. P1-E（第 4–5 周）：安全工作区写入 `workspace.apply_patch`

### 工作

- 输入仅允许 workspace-relative path、base revision、有界 UTF-8 replacement/diff。
- 当前先落地 `Workspace::identity_snapshot`/`live_identity`/`revalidate_identity` 与文件 `revision_snapshot`/`revalidate_revision`：每次路径操作和 patch 发布边界验证 root canonical path、device、inode、digest，并在文件哈希前后复核对象身份。它能拒绝根目录替换、同字节新 inode 和读入期间变化，但不能封闭最后一次路径检查到 rename 的竞态。
- 发布前仍必须使用持有 root dirfd 的相对打开、`openat2(RESOLVE_BENEATH|NO_SYMLINKS)` 或 macOS 等价能力，避免 intermediate symlink/rename 竞态；每次 invocation 验证 root device/inode/digest。
- 在同一安全边界内完成 read → compare-and-swap → temp write → fsync → atomic rename → directory fsync；外部修改始终返回 stale conflict，不覆盖。
- 结果包含 previous/new revision、bytes、bounded diff summary 和 artifact URI；大 diff 受配额/删改保护。
- 将 workspace trust 只作为 capability layer，不允许仓库自身内容提升 trust。

### 验收门槛

绝对路径、`..`、符号/硬链接、非 UTF-8、超大 replacement、并发修改、崩溃/取消、权限变化全部稳定失败或返回冲突；批准后到 rename 前再次校验 args/effects/identity/revision。

## 10. P1-F（第 5–7 周）：进程监督、沙箱与凭据

### 工具首版约束

- executable 必须为绝对路径或受控 toolchain ID；argv、cwd、timeout、stdout/stderr 上限均有协议硬上限。
- 初版禁止任意 environment、stdin、network；后续能力必须由 policy/approval 明确授予。
- sandbox requirement 由 daemon policy 生成，调用方只能收紧，不能自由放宽。

### 底层工作

1. Process supervisor 使用可证明的进程树隔离/终止；仅 PGID kill 不足以覆盖 `setsid`/`setpgid` 逃逸。
2. sandbox 内 trusted init 完成 ready/go handshake；隔离未 ready 前不写入 Started，不启动 target。
3. 超时/取消时终止所有后代并取得证明；无法证明则进入 Unknown/reconciliation。
4. macOS Seatbelt、Linux bubblewrap/namespaces（可选 Landlock/seccomp）能力不足时 fail closed；网络默认禁用。
5. CredentialBroker 只向受控 provider/network proxy 提供短期句柄；日志、prompt、artifact 和跨 chunk stream 做敏感数据删改。
6. 网络代理阻断 loopback/private/link-local/cloud metadata、DNS rebinding 和 HTTP proxy 环境绕过。

### 验收门槛

沙箱不可用时目标进程从未启动；read-only 进程不能写 workspace；timeout/cancel 后所有后代终止或明确 Unknown；环境注入、重定向、子 shell、setuid、超大 argv、输出洪泛均有测试。

## 11. P1-G（第 7–8/10 周）：真实闭环与发布门禁

### 必须通过的端到端剧本

```text
TUI/JSONL
  -> turn/start
  -> provider asks workspace.read
  -> provider asks workspace.apply_patch
  -> approval/request + exact binding
  -> patch committed with diff evidence
  -> provider asks process.run test
  -> approval + sandbox ready/go
  -> bounded test output
  -> provider final answer with diff/test/reconciliation evidence
```

### 故障注入矩阵

在以下每个窗口杀掉 daemon 或断开客户端：proposal、approval、prepared、sandbox ready、started、子进程副作用后、terminal outcome 前、artifact 发布前、SQLite commit 前。每个结果必须是完整终态、明确 Unknown 或可安全重试的幂等操作；不得静默重复非幂等副作用。

### 发布门禁

- Rust/TS 编译、fmt、clippy/typecheck/test/build/schema drift 全绿。
- 两平台真实 E2E、随机并发/背压/重连、24 小时 soak 无 fd、进程、artifact、ledger 泄漏。
- RustSec/cargo-audit、OSV、npm/pnpm audit、许可证 allowlist、锁文件漂移和生命周期脚本审计。
- 生成 SPDX/CycloneDX SBOM、构建 provenance；干净环境重复构建 hash 一致或记录可解释差异。
- macOS/Linux 二进制、TUI、daemon、plugin host 版本匹配，签名、校验和、安装/卸载/recovery smoke 通过。
- `initialize` 能力只反映真实可用功能；jobs/plugins 未接入前不报 true。

## 12. M3–M5 发展规划

### M3：provider、上下文与扩展（P1-G 后 4–8 周）

- OpenAI Responses、Anthropic、Gemini adapter 和契约测试；兼容 provider 不跨供应商隐式 fallback。
- context compaction 带 source seq 范围；SQLite FTS5；策展记忆需用户批准，默认关闭向量库。
- Skills/MCP 延迟发现、摘要和来源信任；一万个工具不得一次性进入 context。
- plugin host 接入 registry/policy/approval/OS sandbox/ledger；协议 major 2 单源生成，拒绝不兼容插件。
- `yeux doctor`、`yeux policy explain`、可诊断的预算/权限/恢复状态。

### M4：后台 Job 与子智能体（M3 稳定后 4–8 周）

- launchd/systemd 生命周期、固定 Job snapshot、DST/休眠/错过周期恢复、默认不重入。
- 无交互批准时进入 `waiting_for_approval`，不静默扩大能力；凭据失效可恢复。
- 一层子智能体，默认并发 4；只读共享 workspace，写任务强制独立 Git worktree。
- token/cost/time/cancel/capability 向下级联；父级审查后显式合并，冲突进入 review。

### M5：v1 发行成熟度（M4 beta 后）

- SQLite migration、backup/restore、consistency check、artifact GC/quota、性能回归。
- 完整协议兼容套件、安装器、Homebrew/系统包、签名/校验和、SBOM/provenance、可复现构建。
- 用户安全指南、扩展作者文档、故障排查；遥测默认关闭并本地删改。
- Windows、远程/云沙箱、消息平台、语音、企业控制面、插件市场、自动合并子智能体明确留到 v1.x+。

## 13. 横向测试矩阵

| 领域 | 必测场景 | 阶段 |
|---|---|---|
| 协议 | schema drift、版本不匹配、重复 command ID、冲突参数、8 MiB 边界、慢客户端、背压、seq gap、重连 | A–G |
| Provider | tool JSON 分片/重复 ID、429、超时、断流、SSE 溢出、取消后 delta、context overflow | A–G/M3 |
| 文件 | `..`、绝对路径、叶/中间 symlink、hardlink、TOCTOU、stale hash、Unicode/非 UTF-8、超大树 | A/C/E |
| 搜索 | 长共同前缀、空/4 KiB query、32 MiB scan、32-call 并发、CPU budget、取消回收 | P0/C |
| 进程 | shell/重定向、最小环境、输出洪泛、timeout、setsid/setpgid、孤儿进程、sandbox unavailable | F/G |
| 网络/密钥 | private/metadata、DNS rebinding、代理绕过、跨 chunk 泄漏、credential expiry | F/M3 |
| 崩溃 | 每个 invocation/ledger/artifact 窗口、restart、Unknown/reconcile、replay 零外部调用 | A/E/F/G |
| Job/agent | 重启、休眠、DST、missed run、重入、父取消、worktree 冲突、预算级联 | M4 |
| 发行 | 干净构建、签名验证、安装/升级/回滚、SBOM/license、24h soak | G/M5 |

## 14. 风险登记与应对

| 风险 | 触发信号 | 应对/决策门 |
|---|---|---|
| 只读工具旁路持续存在 | runner 出现直接 `WorkspaceTools` 调用 | P1-B 完成前不开放任何副作用；CI deny-list + code review |
| 取消语义被误报 | CPU/进程在状态 Cancelled 后仍活动 | 改为 Unknown，补 supervisor/worker 证明；不以 UI 状态掩盖事实 |
| workspace race 未封闭 | identity/device/inode 在 prepare→execute 改变 | fail closed；禁止 apply_patch/process release |
| provider/context 成本失控 | token、ledger、SSE 或 lineage 逼近上限 | 多层预算、artifact/compaction、稳定错误；不自动扩大配置 |
| 双平台行为分叉 | macOS/Linux sandbox/文件 API 测试不同 | 平台能力矩阵；缺能力就隐藏工具，不降级到无沙箱执行 |
| 协议/文档漂移 | TS 手写类型、能力 true 但不可用 | Rust schema 单源生成；CI drift；initialize 能力由运行时注册集派生 |
| 供应链不可追溯 | lockfile/action/构建 hash 变化 | 固定 action commit、audit/SBOM/provenance/signing 作为发布 blocker |
| 范围膨胀 | M3/M4 功能抢先于 P1 | 每阶段 exit gate；未满足门槛的功能保持 hidden/experimental |

## 15. 执行与回滚机制

### 分支/提交

- 每个 ID 一个短分支（例如 `codex/p0-search-budget`、`codex/p1-invocation-pipeline`），每个 PR 同时包含实现、测试、schema/doc 更新和风险说明。
- 先合入纯协议/ledger 兼容变更，再合入暗开关下的 executor；删除旧旁路代码后才打开新工具。
- PR 模板必须附：威胁模型变化、数据流、失败状态、资源预算、replay 计数、macOS/Linux 结果和 rollback 点。

### Feature flags

- `YEUX_ENABLE_MUTATIONS=0`、`YEUX_ENABLE_PROCESS=0`、`YEUX_ENABLE_PLUGINS=0` 默认关闭；只在对应阶段 gate 通过后由 daemon 配置开启。
- 搜索并发、CPU budget、provider context budget 可收紧不可放宽；配置值不能突破编译期硬上限。
- 新 ledger schema 先双读/单写，保留可逆 migration 和备份；未知事件进入明确诊断，不猜测执行。

### 回滚条件

出现越权、stale 覆盖、沙箱未 ready 即启动、Unknown 被重试、replay 外部调用、进程孤儿或资源预算回归时：立即关闭相应 feature flag，回退到只读 registry 版本，保留 ledger/artifact 供取证；绝不用数据库删除或重写来“修复”证据。

## 16. 每周检查点

- **每日**：运行受影响包测试、`git diff --check`、更新 finding/风险状态；不让未验证的旁路进入 main。
- **每周**：在干净 macOS/Linux 环境运行全门禁；复查 threat model、协议 schema、能力广告和文档；记录性能/资源曲线。
- **每阶段**：独立 reviewer 复核数据流，故障注入至少覆盖一条真实 E2E；通过后才将下一阶段 feature flag 从 hidden 改为 opt-in。
- **P1-G 后**：重新开启独立安全审计（重点是 approval、write/process、plugin、MCP、jobs），并把本 Run 3 的 rejected 候选按新的多用户/共享 workspace 假设重新评级。

## 最短可执行清单

```text
[x] 修复 workspace.search 算法复杂度 + 取消/并发预算（dirfd/CAS 之外的只读边界）
[x] 生产 runner 使用 atomic invocation outcome（只读 terminal outcome）
[ ] live workspace identity + dirfd/CAS 完成（live identity 已接入；dirfd/CAS 待完成）
[ ] registry/policy/grants/approval/permit 成为唯一工具路径（sealed read-only registry 已接入；policy/grants/approval 待完成）
[x] 迁移并验证 list/read/search（核心只读路径）
[ ] approval broker + TUI/JSONL parity
[ ] apply_patch 冲突/崩溃/审计证据
[ ] process supervisor + sandbox ready/go + Unknown
[ ] 真实读改测修 E2E + 故障注入
[ ] 依赖/许可证/SBOM/签名/可复现构建/soak
[ ] 新一轮独立安全审计与 v0.1 go/no-go 评审
```
