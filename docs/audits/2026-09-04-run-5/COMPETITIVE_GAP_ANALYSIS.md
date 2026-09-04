# Run 5：YeuX Harness 与成熟 coding harness 的当前差距

审计日期：**2026-09-04（Asia/Shanghai）**
代码基线：**`0a68ae7`**（本地 `main`，工作树干净）
比较对象：终端版 **Grok Build**，以及 Claude Code、OpenAI Codex、Pi；“Grok Build Mode”网页应用构建器单独说明。
文档性质：基于当前源码、文档、CLI smoke 和官方公开资料的工程判断，不是模型能力或性能 benchmark。

> **执行状态说明**：本文保留为 Run 5 开始前的差距快照，正文中的“当前”均指 `0a68ae7`。团队随后以 `82c13c570ac33bc5ad1dddadde40baa7c447b158` 为起点执行了 P0/M2.5 最小纵向切片；已实现的 Git fixture、wire approval/trust、TUI 控制面、能力诊断、验收结果与仍保留的 residual 见 [Run 5 执行记录](EXECUTION_LOG.md)。这份分析不据此回写历史判断。

## 结论

YeuX 现在更准确的定位是：**安全内核优先的本地 agent runtime 原型，已经有可执行的只读多轮 loop 和首版受保护副作用管线，但还没有完成可交付的“读—改—测—修” coding-agent 产品。**

Grok Build、Claude Code 和 Pi 的共同优势不只是工具数量，而是把默认路径闭合了：用户安装后进入会话，模型能发现项目规则，读取代码，提出或执行修改，运行测试，展示 diff，接受接管，失败后继续或恢复。YeuX 的事件账本、effect/capability 交集和 fail-closed 语义在狭窄内核层已经很强；差距集中在这些语义还没有被包装成一条顺滑、可恢复、可验证的默认工作流。

可以用两条轴理解现状：

| 轴 | 当前判断 | 依据 |
|---|---|---|
| 可信内核 | 中高 | Rust daemon 独占 authority；SQLite WAL append-only ledger；纯 replay；显式 EffectSet；能力只能收紧；副作用绑定审批、沙箱和 revision。 |
| 用户可用产品 | 低 | 真实闭环仍缺 E2E；TUI 是 readline 行式 REPL；context/compact、provider onboarding、扩展执行、后台任务和发布安装尚未完成。 |

这不是“再加几个工具”就能解决的问题。首要工作是把已有安全原语穿过 agent loop、UI、Git、测试和恢复路径，形成一条可以反复成功的纵向切片。

## 比较口径

本文比较的是 Grok Build 的**终端 coding agent**。官方文档把它定义为可交互 TUI、headless 脚本或 ACP 嵌入，并明确提供全屏、鼠标交互体验；其源码 README 还列出文件编辑、shell、web 搜索、长任务、workspace/VCS/checkpoint 等产品面（[官方 overview](https://docs.x.ai/build/overview)，[源码仓库](https://github.com/xai-org/grok-build)）。

Grok Build Mode 是另一类产品：网页/移动端从自然语言生成网站、应用或游戏，带实时预览、发布和分享。如果用户指的是这个产品，YeuX 的差距还包括 preview/runtime、部署、域名、分享和云端任务编排；这些不应混进终端 harness 的近期路线图（[Grok Build Mode 公告](https://x.ai/news/grok-build-mode)）。

外部能力均来自截至本日可访问的官方页面，属于“公开可达能力”比较；没有把厂商未公开的内部安全实现当成事实，也没有把一次 demo 当成成功率证据。

## 当前 YeuX 的真实能力

当前 README 已明确写出：只读 Agent loop 与首版受保护写入/进程管线已贯通，但仍不是完成发布门槛的 v0.1 版本（[README 当前状态](../../../README.md#当前状态)）。具体情况如下：

| 层 | 已有 | 当前边界 |
|---|---|---|
| Authority / 状态 | Rust `yeuxd` 是数据库、provider、工具、策略和沙箱的唯一权威；SQLite WAL 事件账本、单调 `seq`、fork lineage、补发和纯 replay。 | 迁移、备份、长时间 soak、完整崩溃注入和所有客户端的 parity 尚未达到 release gate（[架构不变量](../../ARCHITECTURE.md#1-产品不变量)）。 |
| Agent loop | OpenAI-compatible Chat Completions SSE；最多 8 个模型轮次、32 个工具调用、4 MiB 工具结果；`list/read/search` 多轮读工具可执行。 | 没有原生 Responses/Anthropic/Gemini；没有 planner、稳定 todo、工具错误修复策略、成本/重试和 code graph。 |
| Mutation / process | `workspace.apply_patch`、`process.run` 已进入统一 pipeline，但只有 sandbox ready、host ceiling 非 `observe` 时才广告；patch 使用 dirfd/no-follow/revision；Linux 有 bubblewrap/PID namespace probe，macOS 任意进程保持关闭。 | POSIX 最终名称没有 inode/hash 条件 CAS；跨平台 supervisor、网络 endpoint 代理、真实仓库 E2E 尚未完成。 |
| Recovery | `Unknown`、幂等 receipt、`invocation/reconcile` evidence-only 收束，不自动重试外部副作用。 | reconcile 没有外部状态读取和完整引导式 UX；artifact 输出/GC 和 crash-window matrix 未完成。 |
| TUI | Paper Signal 主题、Session Bar、timeline/Inspector/approval formatter、JSONL、deny-default 审批框。 | 实际入口仍是 `readline/promises` 行循环，只有 `/exit` 和 `/quit` 被识别；没有 alternate screen、raw key、鼠标、滚动、焦点、command palette 或完整 diff viewer（[app loop](../../../packages/tui/src/app.ts#L113)，[命令解析](../../../packages/tui/src/args.ts#L5)）。 |
| Context | fork 按 `parent_seq` 继承，历史事件可重建。 | `thread/compact` 明确返回 feature unavailable；没有项目指令层级、FTS、长期 memory、token meter 或 context breakdown。 |
| Extension / jobs | skill/MCP/plugin/job 的协议描述和列表查询；plugin host 有独立进程、manifest/hash 基线。 | `jobs/subagents/plugins` capability 为 false；`job/run` 和 `thread/compact` 返回不可用；MCP/Skills 没有执行路径，plugin host 尚未接入 Rust policy/ledger（[daemon dispatch](../../../crates/yeuxd/src/commands.rs#L71)）。 |
| Onboarding / release | stdio、Unix socket、源码构建、JSON Schema 产物和本地门禁。 | CLI 要求手工提供 base URL 与 model；独立 daemon 使用 `NoCredentialBroker`；没有 keychain/OAuth、安装器、升级/回滚、签名二进制、SBOM、launchd/systemd 服务。 |
| Contract | Rust protocol 类型、56 份 schema、基本 JSON-RPC client。 | TypeScript `RuntimeCommandMap` 只覆盖客户端子集，和 Rust 方法/目标架构存在表面不一致（[TS command map](../../../packages/protocol/src/types.ts#L376)）。 |

已有设计文档的判断仍然有价值，但 `docs/COMPETITIVE_GAP_ANALYSIS.md` 是 2026-08-31 的历史快照，部分“P1 尚未接入”的措辞早于当前 M2；本文件按 `0a68ae7` 重新核对。

## 差距矩阵

| 能力维度 | YeuX 当前 | Grok Build / 成熟基线 | 用户影响 | 优先级 |
|---|---|---|---|---|
| 首次成功 | 需要 daemon、provider URL、model 和环境准备 | Grok 一条命令安装、首次启动浏览器认证；Claude/Pi 也有安装与登录路径 | 试用成本高，别人无法快速复现 | P0 |
| 真实编码闭环 | 读工具已工作；写/进程有条件且缺真实仓库 E2E | 文件编辑、shell、测试、diff、Git 和长任务是默认路径 | 只能解释代码，不能稳定交付改动 | P0 |
| 长任务控制 | 8 轮/32 调用硬上限；steer 进入下一安全点 | plan、context、compact、rewind、background、resume、usage | 长会话容易失忆或直接失败 | P0/P1 |
| 交互 TUI | 行式输出与少量 formatter | 全屏、鼠标、键盘快捷键、面板、滚动、palette、inline diff | 用户看不清状态，也无法低成本接管 | P0 |
| 项目规则与记忆 | 无 AGENTS/CLAUDE 类规则加载，无 FTS memory | Grok skills/AGENTS，Claude `CLAUDE.md`/auto memory，Pi session tree | 每次都要重复说明约定 | P1 |
| Git / checkpoint | revision patch 原语，无 Git-aware checkpoint/worktree | Grok workspace/VCS/checkpoints，Claude commit/branch/PR，Codex 多客户端 review | 修改不可视、不可快速回退或交接 | P0/P1 |
| Provider / 凭据 | 一个 adapter；CLI broker 是 no-op | 多 provider、模型发现、切换、浏览器登录和安全存储 | 真实 provider 无法安全上手 | P1 |
| 扩展 / 外部工具 | descriptor 基线，执行关闭 | Skills、MCP、plugins、hooks、LSP、marketplace | 没有生态和团队工作流复用 | P2 |
| 后台 / 并行 | job 元数据，无 `job/run`；无子 agent | Grok subagents/workflows/background，Claude background agents，Codex app-server | 长任务不能离开终端，复杂任务不能分解 | P2 |
| 恢复与运维 | durable ledger/Unknown 强；UX、迁移、GC、诊断不完整 | 成熟 session dashboard、resume、release/upgrade 体系 | 出错后只能读日志或手工处理 | P1/P3 |
| 发布 | 源码工程基线 | Grok/Claude/Pi 提供预构建、安装、校验和/更新路径 | 不能被当作可安装产品 | P3 |

## 关键差距的因果分析

### 1. 最大断点是“闭环”，不是“工具数量”

YeuX 已经具备安全的 `list/read/search` 和受保护的 `apply_patch/process` 原语，但“存在原语”不等于“完成编码能力”。一个成熟 harness 至少要让下面的链路在默认路径反复成立：

```text
理解任务 → 读取约定与代码 → 形成计划 → 产生 patch
→ 展示影响与审批 → 应用 patch → 运行目标测试
→ 读取失败 → 修正 → 生成可审查 diff / checkpoint
→ 中断、重连或恢复后继续
```

当前缺少计划状态、语义化编辑、Git checkpoint、测试结果 artifact、错误分类和真实仓库 E2E；因此安全管线虽然存在，产品仍停在“能证明一次调用”而不是“能交付一个变更”。Grok 的公开产品页把 plan、multi-file edits、terminal、code review、Git、background tasks 放在同一工作流里；Claude 文档直接以“写测试、运行并修复失败”“创建 commit/PR”为示例；Pi 的项目定位则是小而完整的 agent loop、coding CLI 和多 provider API（[Grok Build 产品页](https://x.ai/build)，[Claude Code overview](https://code.claude.com/docs/en/overview)，[Pi 项目](https://github.com/earendil-works/pi)）。

这里最容易犯的错误是先增加更多 MCP、模型或 agent 数量。那些能力只会扩大未验证状态空间；应先用一个 provider、一个受限测试 runner、一个 Git diff/checkpoint 贯通一条可重复的纵向切片。

### 2. Paper Signal 已经是设计资产，但还不是交互产品

YeuX 的 Paper Signal 方向是可识别的：纸面/夜墨、timeline 而非聊天气泡、Session Bar、Turn Score、Inspector、Approval Drawer，以及“状态同时显示 glyph 和文字”的纪律（[美学设计基线](../../design/AESTHETIC.md#yeux-harness-美学系统)）。这比常见的霓虹渐变控制台更有个性，也与 ledger/replay 的产品语义相吻合。

差距在实现层：

- `runTui` 使用 `readline/promises`，交互循环只特殊处理 `/exit`、`/quit`；`/help`、`/model`、`/plan`、`/context`、`/resume` 等输入会被当作普通 prompt。
- `EventRenderer` 的 Inspector 只保留最近 12 个事件，策略和事件摘要主要是一行字符串；没有可折叠、可搜索、可定位的审计视图。
- 没有 raw mode、alternate screen、鼠标、resize/focus/scroll 状态机；Approval Drawer 目前是格式化文本，不是可导航的 modal。
- `displayWidth` 以 JavaScript code point 数量计算，不能表达 CJK/emoji 的终端单元宽度；需要 wcwidth fixture，否则纸面边框在真实 locale 可能错位。
- 模型流只对 `text_delta` 做主阅读层，其余 reasoning/tool-call 状态容易退化为密集的通用行；统一 diff 还没有 hunk 导航、语法层次和文件级摘要。

Grok 文档把 TUI 定义为 rich、mouse-interactive、fullscreen，并提供 command palette、plan review、context/compact、session/fork 等命令；Pi 的 TUI 文档展示了 viewport、滚动、键盘焦点、autocomplete、差分重绘和同步输出（[Grok overview](https://docs.x.ai/build/overview)，[Grok modes and commands](https://docs.x.ai/build/modes-and-commands)，[Pi TUI](https://github.com/earendil-works/pi/blob/main/packages/tui/README.md)）。所以要补的不是更多颜色，而是“事件流 → view model → viewport/focus → input router → RPC action”这条交互引擎。

### 3. 长上下文能力决定 agent 是否像工具而不是一次性聊天

当前 runner 每轮从 ledger 重载上下文，这对 steer 和可重放性是正确的；但历史没有优先级、摘要、项目规则或检索层。`thread/compact` 现在明确不可用，FTS 和记忆仍在路线图里。硬上限能防资源耗尽，却不能替代 context engineering。

成熟产品已经把这层做成用户可见能力：Grok 有 `AGENTS.md`、skills、`grok inspect`、`/context` 和 `/compact`；Claude 在每次会话读取 `CLAUDE.md`，并维护 auto memory；Pi 把 session tree、compaction 和扩展作为 coding-agent 工作流的一部分（[Grok skills/plugins](https://docs.x.ai/build/features/skills-plugins-marketplaces)，[Claude Code overview](https://code.claude.com/docs/en/overview)，[Pi 项目](https://github.com/earendil-works/pi)）。

YeuX 应采用可信来源分层，而不是无条件复制这些文件名：项目规则先作为只读、可追溯的 context item 进入账本；规则来源、路径、摘要和生效范围可见；压缩生成带 `seq` 范围的 checkpoint，原始事件永不删除；检索和 token 预算仍由 daemon 统一限制。

### 4. YeuX 的安全模型有机会领先，但当前以“不可用”呈现

YeuX 的差异化在于把规范化参数、实际 workspace identity、EffectSet、capability 来源、审批 digest、sandbox 证据和终态绑定到同一个 Invocation，并规定 Unknown 不得静默重试。这个模型比“提示词里说不要运行危险命令”可靠得多，也比 Pi 默认继承启动进程权限的模型更适合高信任本地工具；Pi 官方明确说明其内置 harness 没有 filesystem/process/network/credential permission system（[Pi README](https://github.com/earendil-works/pi)）。

但当前仍有三个层次的安全残余：

1. POSIX `renameat` 不能提供最终名称的 inode/hash 条件替换；非合作写者在最后检查后仍可能改变命名空间。
2. 跨平台进程树治理、网络 endpoint/private-network/metadata 代理和 artifact 输出策略没有完整证据；macOS 因不能证明隔离而关闭任意进程是正确的安全选择，却降低了产品覆盖。
3. plugin host 目前只是独立进程和 manifest/hash 基线，没有接入 Rust policy/ledger；把它标成“可扩展”会让用户误以为第三方代码已经受同一边界保护。

解决方向不是放宽 fail-closed，而是增加**可解释的降级和恢复**：Session Bar 显示 requested/effective mode、host ceiling、sandbox backend、工具可用性和原因；`--mode build` 不要静默退回后直接退出，而应给出 `yeux doctor`/配置动作；Unknown 进入可导航的 evidence/reconcile 面板，永远不自动重试未知外部副作用。

Grok 的公开权限文档也把 permissions 与 sandbox 分开，并提供 ask/auto/always-approve、allow/deny 规则；这说明“可用性与安全是两个正交的 UX 维度”，但不能据此断言其内部安全保证与 YeuX 等价（[Grok permissions](https://docs.x.ai/build/features/permissions)）。

### 5. Provider 和安装路径是产品的第一道门

当前 `yeuxd` CLI 要求 `--provider-base-url` 和 `--model` 成对出现；独立 daemon 使用 `NoCredentialBroker`，没有浏览器登录、keychain、模型目录或配置检查。源码仓库能构建，不代表新用户可以在一分钟内得到第一条有意义的结果。

Grok 公开提供一条命令安装、首次浏览器认证、`~/.grok/config.toml` 自定义模型、`grok inspect` 和 headless/ACP；Claude 有 native/Homebrew/WinGet 安装、首次登录和自动更新；Pi 有独立二进制、SHA256 source archive 与供应链门禁（[Grok overview](https://docs.x.ai/build/overview)，[Claude overview](https://code.claude.com/docs/en/overview)，[Pi README](https://github.com/earendil-works/pi)）。

YeuX 的安全定位反而要求更好的 onboarding：用 OS keychain 或明确的 CredentialBroker 存储密钥，提供本地 Ollama/LM Studio 预设和一个 hosted provider 适配器，`yeux doctor` 检查 socket、sandbox、provider、权限和版本；最终安装包同时携带匹配版本的 `yeux`、`yeuxd` 和 plugin host。可以保留本地优先和默认无遥测，避免复制 `curl | bash` 而没有校验的交付方式。

### 6. 扩展生态必须排在 authority 之后

YeuX 协议已经有 skill、MCP、plugin、job、subagent 的名称，但 capability 明确为 false，执行方法也会返回不可用。这种诚实的 fail-closed 状态比“看起来支持但绕过 policy”更好；问题是它还没有形成可供用户使用的扩展面。

Grok 的扩展文档将 skills、plugins、hooks、MCP、LSP 和 subagents 放进统一 extensions modal，并支持 marketplace；其 workflow 公告进一步展示了可暂停、可恢复、分阶段验证的多 agent 编排。Codex 用 app-server 支撑 skills、MCP、approval 和多客户端；Claude 提供 MCP、skills、hooks、background agents；Pi 用 TypeScript extensions/packages 保持小内核（[Grok extensions](https://docs.x.ai/build/features/skills-plugins-marketplaces)，[Grok workflows](https://x.ai/news/workflows)，[Codex app-server](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)，[Claude overview](https://code.claude.com/docs/en/overview)）。

推荐顺序：先做无副作用的 Skills context；再做 MCP lazy discovery，将每个 tool 映射到 EffectSet、capability、timeout、artifact 和 ledger；然后把 plugin host 接入同一 policy/sandbox；最后做持久 Job、worktree subagent 和 workflow。每一步都要有“扩展不能提权、不能直接写 ledger、server 崩溃可收束、恢复不重复副作用”的负向测试。

### 7. 协议与文档的“目标能力”需要和“可用能力”分开

Rust protocol 声明的方法面比 TypeScript 当前 command map 宽，`thread/compact`、`job/run` 等方法存在名称但返回 feature unavailable。目标架构文档也同时描述 v1 目标和当前基线。对开发者来说，这会造成三种误判：把 descriptor 当成 executable、把 capability 当成 promise、把 schema 存在当成客户端可用。

应建立单一 machine-readable capability manifest：每个方法记录 `stable | experimental | unavailable`、前置条件、side effects、客户端支持和测试 fixture；Rust schema 自动生成 TypeScript map；CI 遍历 manifest，要求每个 `stable` 方法至少有 daemon、TUI/JSONL（若适用）和失败路径测试。文档和 `--help` 从同一 manifest 生成。

## 建议路线图

时间以小型团队为假设，优先级比日历更重要。

### P0：M2.5 纵向切片（当前最应该做）

目标是让一个真实仓库任务完整结束，而不是增加架构占位：

1. 选一个无凭据本地 provider 和一组 10 个小型 fixture repo，贯通 `read → plan → apply_patch → approval → test/process → fix → diff`。
2. 将 Git read-only status/diff、checkpoint 和 revert 作为受控工具；提交、push、外部写继续单独的 operate 权限。
3. 给 patch/process/test 统一记录 ToolCall、EffectSet、revision、stdout/stderr artifact、终态和失败原因；把大输出按预算落入 artifact store。
4. 在 Linux 完成进程树治理证据；macOS 要么实现可证明的 supervisor，要么在 UI 中明确显示 observe-only 平台能力，不把关闭伪装成成功。
5. 完成一次真实仓库 E2E 和批准前后、取消、超时、崩溃、并发修改的最小 matrix。
6. 把行式 TUI 做到诚实且可用：`/help`、`/model`、`/plan`、`/context`、`/resume`、`/compact`（不可用时给出原因）、`/interrupt`、`/reconcile`；EOF 正常退出；CJK/emoji 宽度 fixture。

### P1：长会话与恢复

- deterministic context assembler：项目规则层级、token meter、优先级和来源显示；checkpoint compaction 保留原始 `seq`。
- FTS 会话搜索、resume/fork/rewind、断线自动订阅和最后 `seq` 恢复；重复事件不重复副作用。
- 引导式 reconciliation UI、诊断 bundle、artifact 引用/配额/GC；外部状态探测与重试策略分开建模。
- 完成 POSIX final-name CAS 策略决策、跨平台 process supervisor、network endpoint proxy 和 credential backend。

### P2：可控生态和自动化

- 原生 Responses/Anthropic/Gemini adapter、模型 catalog、keychain/OAuth 和 cost accounting。
- Skills、MCP stdio/HTTP、plugins、hooks，全部走同一个 ToolSpec/effect/policy/ledger；lazy discovery，不能一次把海量工具塞进 context。
- PTY、后台 Job、queue、worktree subagent、父级 review/handoff；自动 merge 默认关闭。
- ACP/IDE adapter，保持 daemon authority 不被客户端绕过。

### P3：发布与团队使用

- macOS/Linux signed release、checksum、SBOM、可复现构建、installer/upgrade/rollback、launchd/systemd。
- main required checks、CODEOWNERS、依赖/action 固定、migration/backup/restore、24 小时 soak 和资源泄漏门禁。
- 项目/线程 dashboard、分享与诊断导出；默认无遥测，用户显式 opt-in 才发送统计。

### P4：另一个产品轨道

若目标是追赶 Grok Build Mode 的网页/移动应用构建能力，应另立产品边界：preview runtime、浏览器沙箱、部署、域名、分享、云端长任务和多租户数据生命周期都不是终端 harness 的自然增量。它可以复用事件、权限和 artifact 语义，但不应挤占 P0/P1 的编码闭环工作。

## 可验收指标

以下是建议的 release gate，不是当前已达到的数字：

| 类别 | 目标 |
|---|---|
| 任务闭环 | 10 个跨 Rust/TypeScript/Python/Go 的 fixture 任务中至少 8 个无需人工直接编辑文件即可完成 read→edit→test→fix；每个任务有可审查 diff、测试结果和 ledger trace。 |
| 副作用安全 | 负向测试中未授权写入/进程/网络为 0；100 次随机 crash/restart 注入中无重复非幂等副作用；Unknown 永不被自动重试。 |
| 恢复 | 断线后从 `afterSeq` 恢复不产生重复 `seq` 或重复 ToolCall；daemon 重启后每个活动 Turn 都有明确终态或 reconciliation-required。 |
| UI | 80/120 列布局、resize、CJK/emoji、无色/ASCII 模式均通过 snapshot；8 个核心命令可由 palette 或 help 发现；EOF、SIGINT 和 approval modal 无未收束 promise/warning。 |
| 上下文 | compaction 生成带来源 `seq` 范围的 checkpoint；原始事件保留；context/token 使用量可见，秘密不进入 summary、artifact 或普通日志。 |
| 扩展 | 每个 MCP/plugin tool 都有 effect/capability/timeout/ledger 证据；扩展负向测试不能访问 SQLite、凭据或越权路径；server 崩溃可暂停并收束。 |
| 发布 | 干净 macOS/Linux checkout 可安装匹配版本的 `yeux`/`yeuxd`，可校验签名/checksum，`yeux doctor` 能解释 provider、socket、sandbox 和版本问题。 |

## 应保留与不应复制的东西

应保留：

- Rust daemon 单一 authority 和客户端只依赖协议的边界；
- append-only ledger、纯 replay、durable receipt、fork lineage 和 Unknown 分类；
- EffectSet、四层 capability intersection、invocation-bound approval 和 revision/identity 绑定；
- sandbox/credential fail-closed、默认无遥测、本地优先；
- Paper Signal 的纸面/仪器语言，以及把安全状态持续显示给人的设计纪律。

暂时不要复制：

- Grok 的 marketplace、成百上千 agent workflow、云端/网页 Build Mode；
- provider 数量、企业控制面和跨平台表面积；
- 只在 prompt 或 UI 上承诺安全、却没有 daemon authority 的“快速插件”；
- 在 P0 闭环、P1 恢复和发布门禁之前追求功能数量或大规模 dashboard。

## 产品定位建议

最合适的路线是把 YeuX 定位成**高信任的本地工程仪器**：对安全敏感、需要可审计会话、偏好本地模型或受控 provider 的开发者，提供比通用 coding agent 更清楚的副作用边界和恢复证据。竞争维度应是“每次改动都能说明、审查、回放和恢复”，不是模型排行榜或 agent 数量。

如果未来要成为广谱 coding harness，先完成 P0/P1 再扩展生态；如果要成为网页应用 builder，则另立 P4 产品轨道。两者共享安全内核，但默认用户旅程、运行时和分发方式不同。

## 直接证据索引

- YeuX：[README](../../../README.md)、[架构](../../ARCHITECTURE.md)、[路线图](../../ROADMAP.md)、[威胁模型](../../THREAT_MODEL.md)、[Paper Signal](../../design/AESTHETIC.md)。
- 当前 Run 4 状态与门禁：[STATUS_AND_PLAN.md](../2026-09-03-run-4/STATUS_AND_PLAN.md)。
- Run 5 实现与最终门禁：[EXECUTION_LOG.md](EXECUTION_LOG.md)。
- 历史竞争分析（2026-08-31）：[COMPETITIVE_GAP_ANALYSIS.md](../../COMPETITIVE_GAP_ANALYSIS.md)。
- Grok：[产品页](https://x.ai/build)、[overview](https://docs.x.ai/build/overview)、[modes/commands](https://docs.x.ai/build/modes-and-commands)、[permissions](https://docs.x.ai/build/features/permissions)、[skills/plugins](https://docs.x.ai/build/features/skills-plugins-marketplaces)、[source](https://github.com/xai-org/grok-build)、[workflows](https://x.ai/news/workflows)。
- Claude：[overview](https://code.claude.com/docs/en/overview)、[common workflows](https://code.claude.com/docs/en/common-workflows)。
- Codex：[app-server README](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)。
- Pi：[repository](https://github.com/earendil-works/pi)、[TUI README](https://github.com/earendil-works/pi/blob/main/packages/tui/README.md)。
