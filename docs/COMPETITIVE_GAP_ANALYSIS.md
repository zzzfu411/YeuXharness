# YeuX Harness 竞争差距分析与 P0–P4 执行计划

状态日期：**2026-08-31**
适用范围：YeuX Harness 本地开源实现，对比 Grok Build、OpenAI Codex、DeepSeek Harness 与 Pi Agent Harness
文档性质：产品工程决策与阶段门槛，不是营销排行，也不以仓库体量或功能数量代替可验证能力
Goal 后状态：**P0 有界只读 Agent loop 已落地并通过仓库门禁；Run 3 已补上 P1-A 账本/恢复与 P1-B/C 只读 registry 基础，但写入/进程安全闭环仍未开放**

## 1. 结论先行

YeuX 当前最准确的定位是：**安全语义和事件语义优先、已经具备有界只读 Agent loop 的 v0.1 架构基线，但还不是可完成真实“读、改、测、修”的 coding-agent harness。**

本次 Goal 已经贯通并验证：

```text
用户请求
  -> 模型推理
  -> 结构化工具调用
  -> 有界 workspace.list/read/search
  -> 工具结果持久化
  -> 再次模型推理
  -> 最终答复
```

`yeuxd` 现在能够汇聚碎片化 tool-call JSON、并发执行多个结构化只读调用、按模型调用顺序持久化 ToolCall/ToolResult 和 Invocation 状态，并在下一轮 provider 请求前重载 ledger，使 `turn/steer` 在安全点进入上下文。未注册或未协商工具不会落入 Shell、写入、网络或插件执行器。

与四个对标项目相比，首要差距因此已经从“没有工具 loop”转移为：写文件、进程、补丁、sandbox、artifact、policy/approval、凭据和长期上下文仍未形成统一、不可绕过、可恢复的产品闭环。因此：

- **产品能力**：只读纵向闭环已经从架构占位变为可执行能力，但仍明显落后于 Grok Build、Codex、DeepSeek Harness 和 Pi 的真实编码、长任务和扩展闭环。
- **工程成熟度**：落后于 Codex、Grok Build 与 Pi 的安装、发布、兼容、长期会话和产品化闭环；也落后于 DeepSeek Harness 的完整插件化运行时与 Web 产品面。
- **架构潜力**：append-only ledger、纯投影 replay、解析后的 read effect、确定性结果顺序、effect/capability 类型、Rust authority、资源上限和安全 IPC 已经在 P0 形成第一条可运行证据链；精确审批、policy 与 OS sandbox 的差异化仍需 P1 才能转化为写入/进程能力。

建议依赖顺序只有一条主线：

```text
[x] P0 可工作的只读 Agent loop
  -> P1 受保护的读改测修闭环
  -> P2 Provider / Context / Skills / MCP 生态
  -> P3 产品体验、后台任务与隔离子智能体
  -> P4 发布、迁移、供应链与长期稳定性
```

P0 已完成后，任何绕过 P1、先扩展插件市场、远程能力、复杂多智能体或全平台 UI 的工作，仍会扩大未闭环的可信计算基，并延迟 YeuX 真正成为 coding harness。

---

## 2. 比较口径与可复现快照

### 2.1 状态符号

| 符号 | 含义 |
|---|---|
| ✅ | 在所列快照中存在可工作的产品路径或正式内置能力 |
| ◐ | 只有部分路径、架构原语、实验/可选 overlay、扩展示例，或尚未形成默认产品闭环 |
| ❌ | 所列快照中没有该内置能力 |
| — | 不适用或官方材料不足以作出更强结论 |

“✅”不代表没有缺陷；“◐”也不代表设计较差。例如 Pi 明确选择不内置 sandbox、MCP、subagents 和 permission popup，这是产品哲学，不是实现遗漏，但对 YeuX 的安全目标而言仍构成功能差距。

### 2.2 版本、commit 与来源口径

| 项目 | 2026-08-31 比较快照 | 版本/发布口径 | 备注 |
|---|---|---|---|
| **YeuX Harness** | 本地 `HEAD` `05e02ea59f088e4f0731df3dcd94499509a64107`，加 2026-08-31 P0 Goal 后已验证工作树 | workspace `0.1.0` 开发基线；P0 只读 loop 增量 | 对标基线 commit 保持不变；本文 YeuX 能力列采用 Goal 后状态。P0 增量已通过仓库门禁，但尚未形成 release；以 [README](../README.md)、[ARCHITECTURE](ARCHITECTURE.md)、[ROADMAP](ROADMAP.md) 和测试为准 |
| **Grok Build** | `xai-org/grok-build@bc7f02eddd3d84085849dc19ed216f11c23b0571`，commit 时间 2026-08-28 | crate `1.0.12`；`SOURCE_REV=d5a0335a47221e8c9519936cb693e9b6450227ec`；该镜像仓库当日无 GitHub Release/tag | 官方发行二进制有独立发布渠道，不能用镜像仓库“无 Release”推断产品未发布 |
| **OpenAI Codex** | `openai/codex@d58d0e5841e0de08e251673db2d5af8cf3a1ad51`，commit 时间 2026-08-31 | 最新 GitHub Release `rust-v0.151.0`，发布于 2026-08-29 | 源码 manifest 使用开发占位版本，产品版本采用正式 release tag |
| **DeepSeek Harness** | `deepseek-ai/deepseek-harness@0a53fb55bea101816fa226bb964ae2bed71c343b`，commit 时间 2026-08-30 | `0.1.2-alpha.2` / tag `dsh-v0.1.2-alpha.2` | 官方明确标记 developer preview、会发生破坏性兼容变化，且尚未完成安全审计 |
| **Pi Agent Harness** | `earendil-works/pi@853a80d26c90a14c1886f0ebb8ffaae133ca2185`，commit 时间 2026-08-28 | 最新 Release `v0.84.4`；coding-agent package `0.84.4` | release tag commit 与随后 main HEAD 不同，因此同时记录 release 与源码快照 |

### 2.3 方法限制

1. 比较对象是公开仓库、官方文档和官方发布页能核实的能力，不评价闭源服务端内部实现。
2. Codex 同时有 CLI、app-server、IDE、桌面与云产品；矩阵主要评价开源仓库可见的本地 harness 和由 app-server 明确支撑的官方客户端面，不把未知云端能力算入 YeuX 的必须追赶项。
3. Grok Build 仓库是从上游 monorepo 周期同步的镜像；以固定 commit 和 `SOURCE_REV` 为准。
4. DeepSeek Harness 和部分 Codex 能力明确为 experimental；矩阵会给出能力存在性，但阶段计划不会把实验接口当作稳定规范照搬。
5. 该比较不以 star 数、模型排行榜或单次 demo 为证据。YeuX 的验收必须由本仓库自动化测试、故障注入和真实仓库任务证明。

为避免对标对象随执行过程漂移，Grok Build、Codex、DeepSeek Harness 和 Pi 仍使用上表固定快照；下方矩阵中的 YeuX 列则明确更新为 **2026-08-31 Goal 后状态**。

---

## 3. 能力矩阵

### 3.1 Agent 核心与编码闭环

| 能力 | YeuX | Grok Build | Codex | DeepSeek Harness | Pi |
|---|---:|---:|---:|---:|---:|
| 多步模型↔工具 Agent loop | ✅ Goal 后：有界只读多轮 loop | ✅ | ✅ | ✅ | ✅ |
| 内置文件 list/read/search | ✅ Goal 后：`workspace.list/read/search` | ✅ | ✅ | ✅ | ✅ `read`，搜索可用 shell/扩展 |
| 结构化 edit/patch/write | ◐ patch 原语未接 daemon | ✅ | ✅ | ✅ | ✅ `write`/`edit` |
| Shell/进程执行 | ◐ ProcessExecutor 未接 daemon | ✅ | ✅ | ✅ | ✅ `bash` |
| PTY、后台命令、长任务 | ❌ | ✅ | ✅/◐ 部分接口实验 | ✅ job/terminal seam | ❌ 内置，官方建议 tmux/扩展 |
| 工具并行且结果顺序确定 | ✅ 只读并发；按模型调用顺序入账 | ✅ | ✅ | ✅ | ✅，完成可并行、持久化按源顺序 |
| steer / follow-up / interrupt | ◐ steer 已在下一请求安全点注入、interrupt 可传递；follow-up 仍为普通后续 Turn | ✅ | ✅ | ✅ | ✅ |
| Plan/Goal/Todo 工作流 | ◐ 类型/Job 元数据，执行未闭环 | ✅ | ✅ | ✅ | ❌ 内置，可用扩展 |
| Git-aware diff/checkpoint/worktree | ❌ | ✅ | ✅ | ✅/◐ provider/插件组合 | ❌ 内置专用层，依赖 bash/扩展 |
| Web search / fetch | ❌ | ✅ | ✅ | ✅/◐ 组合决定 | ❌ 内置，可用 skill/扩展 |

**Goal 后核心判断**：YeuX 已经跨过“只能单次请求”的运行时骨架阶段，具备一条刻意收窄、预算化、可入账的只读工具 loop。它仍不是四个对标项目的低配完整 coding agent：edit/patch/process、审批、sandbox、凭据、compaction 和产品面仍然缺失，下一决定性差距是 P1。

### 3.2 会话、上下文与恢复

| 能力 | YeuX | Grok Build | Codex | DeepSeek Harness | Pi |
|---|---:|---:|---:|---:|---:|
| 持久会话 resume/list/archive | ✅ | ✅ | ✅ | ✅ | ✅ |
| fork/branch | ✅ `parent_seq` 谱系 | ✅ | ✅ | ✅ | ✅ 单文件树、fork/clone |
| append-only 事实源 | ✅ SQLite trigger 强制 | ◐ 有持久 session，未按 YeuX 同一口径声明 | ◐ rollout/thread store | ✅ SessionEvent log；JSONL/SQLite backend | ◐ JSONL session tree |
| 纯 replay 不重新执行 provider/tool/network | ✅ 设计与测试要求明确 | — | — | ◐ 事件投影/恢复语义完整 | — |
| 跨重启 command 去重 | ✅ durable receipt | — | ◐ app-server/thread 层有多类幂等接口 | ◐ persistence/恢复 | — |
| 项目指令 AGENTS.md/规则 | ❌ | ✅ | ✅ | ✅ | ✅ |
| token-aware context assembly | ❌ 仅历史 user/assistant | ✅ | ✅ | ✅ | ✅ |
| 自动/手动 compaction | ❌ | ✅ | ✅ | ✅ | ✅ |
| 会话搜索/长期记忆 | ❌ | ✅ memory/context UI | ✅ memory/thread search | ✅/◐ 组合能力 | ❌ 内置长期记忆 |
| usage/cost/context 可视化 | ❌ | ✅ | ✅ | ✅ token meter/Web UI | ✅ |

**YeuX 的局部优势**是事件顺序、command receipt、fork 截断和 replay 的规则更早被固定；Goal 后这些语义已经承载真实只读 ToolCall/ToolResult 与 Invocation。**主要差距**转为 compaction、项目指令、token 预算、FTS 和长期会话产品面，因此尚未转化成长会话生产力。

### 3.3 Provider、扩展与集成

| 能力 | YeuX | Grok Build | Codex | DeepSeek Harness | Pi |
|---|---:|---:|---:|---:|---:|
| 原生多 Provider | ❌ 仅无凭据 OpenAI-compatible Chat Completions | ✅ custom OpenAI/Responses/Anthropic/local | ◐ OpenAI 产品优先，存在 provider/config seam | ✅ DeepSeek、Anthropic、OpenAI、云/自定义 | ✅ 大量内置 provider 与动态 catalog |
| 凭据存储、OAuth、轮换 | ❌ broker 未接 | ✅ | ✅ | ✅ write-only UI + credential seam | ✅ API key/OAuth/provider auth |
| 模型发现与运行时切换 | ❌ | ✅ | ✅ | ✅ | ✅ |
| Skills | ◐ descriptor 查询，无加载执行 | ✅ | ✅ | ✅ | ✅ |
| MCP client | ◐ status 类型，无连接/执行 | ✅ | ✅ | ✅ stdio/Streamable HTTP，可选启用 | ❌ 内置，官方建议扩展或 CLI skill |
| 插件/扩展 API | ◐ 独立 host 基线，未接 policy/ledger | ✅ plugins/hooks/marketplace | ◐ skills/MCP/apps/app-server 扩展面 | ✅ everything-is-a-plugin | ✅ TypeScript extensions/packages |
| 不可信扩展 OS 隔离 | ❌ | ◐ 与 trust/sandbox 组合 | ◐ 取决于扩展类型和执行环境 | ◐ 官方明确 developer preview 风险 | ❌ 扩展与主进程同权限 |
| Headless/JSON/RPC | ◐ JSONL/stdio 基线 | ✅ | ✅ `codex exec`/app-server | ✅ CLI/ACP/SDK/Web host | ✅ print/JSON/RPC/SDK |
| ACP/IDE/可嵌入 server | ❌ | ✅ ACP | ✅ app-server、IDE、Desktop | ✅ ACP/Python SDK/Web API | ✅ RPC/SDK；不主打 ACP |

YeuX 不需要在短期追平 Pi 的 provider 数量或 DeepSeek 的插件数量。正确顺序是先定义一个严格 provider contract 和 sealed tool pipeline，再扩展 adapter；否则每增加一个 provider 或插件都会把未验证状态空间成倍扩大。

### 3.4 安全、策略与可信边界

| 能力 | YeuX | Grok Build | Codex | DeepSeek Harness | Pi |
|---|---:|---:|---:|---:|---:|
| 副作用结构化 `EffectSet` | ✅ 协议/核心层 | ◐ 工具/权限模型存在，口径不同 | ◐ permissions/sandbox/approvals | ◐ tool/approval/capability seams | ❌ 统一 effect 类型 |
| 精确审批绑定 | ✅ 核心算法；只读 P0 固定免交互，写入路径尚未接 | ✅ 权限模式 | ✅ approval + exec policy | ✅ approval/permission presets | ❌ 内置 popup；扩展可实现 |
| capability 只能收紧 | ✅ policy 原语；P0 工具集 sealed，尚未覆盖写入/扩展路径 | ◐ | ✅/◐ profile/requirements | ◐ composition/permission seam | ❌ 内置能力上限 |
| 内置 OS sandbox | ◐ Seatbelt/bubblewrap 原语未接工具 | ✅，但默认 off | ✅ macOS/Linux/Windows 策略 | ◐ Linux Landlock 等，组合决定 | ❌；依赖容器/VM/扩展 |
| sandbox 不可用时失败关闭 | P0 无 process/write；P1 副作用路径仍待接入 | ✅ 对请求的非 off profile | ✅ 按策略/profile | ◐ 仍为 developer preview | 由外部运行环境决定 |
| IPC/终端输出安全边界 | ✅ 已修复并审计 | ✅ 成熟 TUI/runtime | ✅ 成熟多客户端面 | ◐ Web/CLI 多边界，官方未声称已审计 | ◐ 本地同用户信任模型 |
| Provider 流/累计输出硬上限 | ✅ provider、tool-call、遍历和结果均有预算 | — | ✅/◐ 多层限额 | ✅/◐ | — |
| 正式安全审计记录 | ✅ 首轮代码审计，2 个 MEDIUM 已关闭 | — | ✅ 官方安全项目/长期产品化 | ❌ 官方明确尚未审计 | 明确安全边界，不承诺内置隔离 |

YeuX 最值得保留的差异化不是“提示更多确认框”，而是将**规范化参数、工具版本、workspace identity、effect digest、capability 来源、审批和 terminal result**绑定到同一调用事实中，并使 replay 永不重放副作用。P1 必须把这一设计从类型层变成唯一可达执行路径。

### 3.5 产品体验、自动化与发布

| 能力 | YeuX | Grok Build | Codex | DeepSeek Harness | Pi |
|---|---:|---:|---:|---:|---:|
| 成熟交互 TUI | ◐ 行式 REPL | ✅ 全屏/鼠标/面板/主题 | ✅ | ◐ 主要为 Web UI，另有 CLI 面 | ✅ 可扩展 TUI |
| 文件引用、diff review、tool timeline | ❌ | ✅ | ✅ | ✅ Web UI | ◐ 文件引用/tool UI 强，diff 可由工具/扩展 |
| Web/Desktop/IDE | ❌ | ❌ Web UI；✅ ACP editor embedding | ✅ | ✅ Web UI | ❌ 官方主产品面；SDK 可自建 |
| 后台 Job/调度/webhook | ◐ metadata，无 `job/run` | ✅ background tasks | ◐ background terminals/queue，通用调度不作为 CLI 核心承诺 | ✅ jobs/schedule/webhook overlay | ❌ 内置 background bash |
| 内置多智能体 | ❌ | ✅ | ✅ | ✅ subagent；agent teams 实验 | ❌ 内置；扩展示例可用 |
| 一键安装/升级 | ❌ | ✅ macOS/Linux/Windows | ✅ installer/npm/Homebrew/releases | ✅ alpha npm/CLI | ✅ npm/installer/self-update |
| 签名、校验和、SBOM/供应链门禁 | ❌ | ◐ 官方发行 | ✅ 成熟发行链 | ◐ alpha release gates | ✅ 依赖固定、shrinkwrap、签名审计、release smoke |
| migration/backup/GC/soak/perf gate | ❌ | ◐ | ✅/◐ | ✅/◐ 多种工程 gate | ◐ release smoke 与测试成熟 |

---

## 4. 各对标项目真正领先在哪里

### 4.1 Grok Build：完整本地产品面和长任务体验

领先点：

- 已把文件编辑、shell、搜索、web、模型、session、compaction、memory、MCP、skills、plugins、hooks、subagents、background tasks 和权限模式放进统一全屏 TUI。
- 同时提供交互、headless/CI 与 ACP editor embedding；用户无需理解内部架构即可完成任务。
- session dashboard、fork、usage、context breakdown、模型/权限切换等降低了长任务操作成本。
- 有 kernel sandbox，但官方文档明确默认关闭，说明其产品选择更偏向“能力齐全 + 用户选择隔离”。

YeuX 应学习：纵向闭环、可观察工具时间线、长任务控制、延迟扩展发现、ACP 作为后续集成层。
YeuX 不应照搬：在安全执行路径未闭环前复制 marketplace、hooks 数量或复杂 dashboard；也不应把 sandbox 默认关闭作为普通开发默认值。

### 4.2 Codex：生产级 agent runtime、协议服务器和多客户端生态

领先点：

- 真实工具 loop、shell/patch、approval、sandbox、network controls、项目指令、skills、MCP、compaction、memory、多智能体和成熟 TUI 已经形成完整产品。
- `codex app-server` 提供 Thread/Turn/Item、stdio/Unix socket、schema 生成、backpressure、thread start/resume/fork、turn steer/interrupt、approvals、skills、apps 等广泛协议面，支撑 IDE 和桌面客户端。
- 安装器、npm、Homebrew、跨平台 release 与官方安全边界显著领先。
- 能力广度包括 review、队列、后台终端、项目/线程组织等，说明其竞争优势不仅是模型，也是完整本地 agent 平台。

YeuX 应学习：app-server 风格的稳定协议面、生成式跨语言类型、sandbox 与 approval 正交、客户端不越过 daemon authority、非交互模式一致性。
YeuX 不应照搬：OpenAI 产品专用、企业/云/实时语音等巨大表面积；P0–P2 也不应复制其实验 API 数量。YeuX 要保留 provider-neutral ledger 与较小的可信内核。

### 4.3 DeepSeek Harness：capability seam、插件组合与 Web 产品层

领先点：

- agent loop、session event log、tool pipeline、filesystem/subprocess/sandbox/approval、credentials、compaction、subagent、jobs、schedule、MCP、ACP、Web UI 和 Python SDK 已形成可组合系统。
- “everything-is-a-plugin”让 provider、tool、UI 和 background capability 能通过 Cordis composition 替换或叠加。
- 文档、生成 catalog、类型等价、snapshot、E2E、Web perf/stress 和 package invariant gates 非常全面。
- append-only SessionEvent、fork/resume、crash recovery 与 session projection 与 YeuX 的事件语义目标高度相关。

YeuX 应学习：capability provider/consumer seam、可撤销注册、工具 schema catalog、Web/host 分层、插件级测试契约。
YeuX 不应照搬：把 policy、ledger、审批 authority 或最终 UI 也变成可替换插件。YeuX 的核心安全承诺要求这些边界 sealed；同时避免在可工作 loop 前引入 DeepSeek 级别的模块图和文档生成复杂度。

### 4.4 Pi：小内核、多 Provider、可嵌入性和高效终端工作流

领先点：

- 最小但完整：默认四个工具 `read`、`write`、`edit`、`bash` 已能闭环完成真实任务。
- 多 Provider、OAuth/API key、模型 catalog、推理等级、成本统计和跨模型 handoff 是同类项目中最完整、最直接的参考之一。
- interactive、print/JSON、RPC、SDK 四种模式，加上 extension/skill/prompt/theme/package 系统，扩展体验简单。
- session JSONL tree、fork/clone、compaction、steer/follow-up 和强 TUI 让单智能体工作流非常高效。
- 供应链门禁具体：精确依赖、shrinkwrap、生命周期脚本 allowlist、release smoke、签名审计。

YeuX 应学习：先做小而完整的 loop、provider contract、程序化 embedding、TUI 中的文件引用/usage/context、简单明确的扩展 API。
YeuX 不应照搬：Pi 明确没有内置 sandbox、permission popup、MCP、subagents、plan mode 或 background bash；扩展与主进程同权限。YeuX 的产品承诺恰好要求这些副作用进入 daemon 的强制安全边界。

---

## 5. YeuX 应保留的优势

以下首先是**架构/基线优势**。其中事件事实源、只读 effect、确定顺序和有界执行已经在 P0 成为可运行能力；涉及写入、进程、审批和 sandbox 的条目仍不得包装成已交付产品功能。

1. **事件事实源先于功能扩张**
   SQLite WAL、append-only trigger、Thread 内单调 `seq`、durable command receipt 和纯 projection replay 为崩溃恢复、客户端补发和审计提供统一基础。

2. **副作用有显式类型，而不是仅凭工具名猜风险**
   `EffectSet` 能描述 read/write/delete/process/network/secrets/external write，并携带幂等性、可逆性和并发语义。P1 接通后，可比“命令字符串黑名单”形成更可靠的审批与调度依据。

3. **审批绑定对象更精确**
   workspace identity、thread、agent、mode、tool version、规范化 arguments/effects digest 与 expiry 已进入核心模型。目标是批准“这一次准备好的调用”，而不是宽泛批准一个工具名。

4. **权限只能收紧**
   `host ceiling ∩ user profile ∩ project trust ∩ turn override` 是适合本地安全 harness 的清晰不变量；子智能体、插件和 TUI 都不能绕过 Rust authority 扩权。

5. **Replay 的承诺可测试**
   replay 只折叠事件，不调用 provider、工具、网络或外部系统。未知状态的非幂等操作进入 reconciliation，而不是“为了继续运行”自动重试。

6. **Rust authority 与 TypeScript surface 隔离清晰**
   UI 和 plugin host 不直接打开 ledger、不直接执行受保护工具，为未来多客户端和插件生态保留单一可信入口。

7. **默认边界已接受一次正式审计**
   私有 Unix socket、终端控制字符清理、provider 资源预算和 sandbox launcher 环境分离已经形成可信起点；审计详情见 [`docs/audits/2026-08-30-run-1`](audits/2026-08-30-run-1/REPORT.md)。

8. **P0 已把确定性审计语义落到真实只读工具**
   三个结构化工具记录实际解析的 workspace-relative read effect；同轮调用可以并行，但 ToolResult 严格按模型调用顺序入账并回灌，未知工具不会落入其他 executor。

P0 已让一部分优势成为真实只读竞争力；P1 仍必须证明写入和进程副作用也只能通过统一 invocation pipeline。若后续为追赶功能而允许 TUI、MCP、插件或特殊工具绕过该管线，上述优势会立即失效。

---

## 6. 差距优先级

| 优先级 | 定义 | 当前最重要的结果 | Goal 后状态 |
|---|---|---|---|
| **P0** | 没有它就不能称为 coding harness | 可完成真实只读 `read/search -> tool result -> answer` 多步任务 | **✅ 已落地并通过仓库门禁** |
| **P1** | v0.1 发布阻断 | 在 sandbox、policy、approval 下完成真实 `read -> edit -> test -> fix` | **下一主线** |
| **P2** | 核心竞争力补齐 | 多 Provider、context/compaction、Skills、MCP、受限插件 | 待 P1 |
| **P3** | 长任务与产品体验 | 成熟 TUI、后台 Job、schedule、worktree 子智能体、embedding server | 待 P2 |
| **P4** | v1 生产成熟度 | migration/backup/GC、soak/perf、签名/SBOM/可复现发行、稳定兼容 | 待 P3 |

### 6.1 依赖图

```text
P0 core [DONE]
  structured workspace tools + hard budgets
    -> fragmented tool-call assembly
      -> bounded multi-round Agent loop
        -> deterministic concurrent results + steer/cancel
          -> JSON-RPC vertical E2E + repository gates

M1 follow-ups [OPEN]
  CredentialBroker
  TypeScript / interactive-JSONL parity
  minimal FTS projection + expanded real-repository eval

P0 core
  -> P1.1 唯一 invocation pipeline
      -> P1.2 policy + approval
      -> P1.3 patch/diff/checkpoint
      -> P1.4 process + OS sandbox
      -> P1.5 artifact/redaction/network
          -> P1.6 crash/reconciliation matrix
              -> P1.7 v0.1 coding E2E

P1
  -> P2 provider/context 分支
  -> P2 Skills/MCP/plugin 分支
      -> P3 TUI/automation/subagents
          -> P4 release hardening
```

---

## 7. P0：可工作的只读 Agent loop

**目标**：一条真实任务能从 `yeux` 进入 `yeuxd`，由模型调用结构化 `workspace.list/read/search`，工具结果进入 ledger，再次调用模型并返回答案。P0 期间不开放任何写文件、shell、network 或插件工具。

**Goal 后状态：✅ 本次定义的 P0 核心纵向闭环已落地，并通过 Rust、TypeScript 与 JSON-RPC 纵向门禁。** 下表保留制定 Goal 时的完整拆分，便于追踪没有被“完成”措辞吞掉的后续工作；其中完整 TS 生成、`CredentialBroker`、交互/JSONL parity、FTS 和更大真实仓库 eval 已明确留在 M1/P2 收尾，不冒充本次已实现能力。

| ID | 交付物与依赖 | 验收指标与测试 | 主要风险 | 明确不照搬 |
|---|---|---|---|---|
| **P0.1** | 从 Rust schema 生成完整 TypeScript protocol；补齐 command/event/tool/invocation 类型。依赖：现有 56 schema。 | CI 中 Rust schema、生成 TS、提交产物三者 byte-for-byte 无漂移；TS 能覆盖 daemon 全命令；旧客户端对兼容字段可忽略、主版本不匹配明确拒绝。 | 生成类型过于宽松；Rust/TS `null`、union、整数范围语义不一致。 | 不复制 Codex 当前庞大的实验协议面；只生成 YeuX 已实现或本阶段需要的稳定 surface。 |
| **P0.2** | 在 `yeuxd` 建立唯一 `ToolRegistry`、`ToolExecutor` port 和 tool schema catalog；runner 不直接调用具体工具。依赖：P0.1、现有 `ToolSpec`/Invocation 类型。 | 未注册、重复名称、schema 无效、版本冲突均有稳定错误；tool schema 进入 provider 请求；任何 executor 调用都有 invocation/event identity；单元测试覆盖 registry 生命周期。 | 为赶进度在 runner 中写 `match tool_name`，以后绕过 policy。 | 不照搬“everything is a plugin”到 authority：registry 可扩展，但 ledger/policy/approval 不能替换。 |
| **P0.3** | 将 `workspace.list/read/search` 包装成只读工具，并加入路径、单文件字节、文件数、深度、总扫描字节、结果条数和时限预算。依赖：P0.2。 | `..`、绝对路径、symlink、Unicode、超大文件、二进制、深目录、海量文件测试；所有预算命中时返回稳定、可恢复的裁剪结果；工具 effect 必须严格为 read-only。 | 当前 `read_to_end`、递归 list、全仓顺序 search 可被大仓库耗尽内存/时间；TOCTOU。 | 不复制通用 shell `find/grep/cat` 作为 P0 工具；先用结构化、可预算、可审计原语。 |
| **P0.4** | 将单请求 runner 改为有上限的多步 loop：request→stream→tool calls→execute→results→next request。依赖：P0.2/P0.3。 | 限制每 Turn 最大 step、tool call、并发、总工具输出、总 provider bytes 与 wall time；碎片 tool JSON、重复 call ID、未知工具、部分失败、模型无终止响应均有测试；不会无限循环。 | 重复调用、结果顺序漂移、取消后残余事件落账、模型制造无限 tool loop。 | 不直接复制某 provider 的 tool-call wire；内部始终使用 provider-neutral `ModelEvent`/`PreparedInvocation`。 |
| **P0.5** | 接通 `CredentialBroker` 和 provider config；凭据只以引用/句柄进入配置，每次请求解析，不进入 ledger/诊断。依赖：P0.4 可并行开发。 | provider 缺凭据、轮换、撤销、401、429、超时、断流均测试；日志/事件/JSONL/TUI 中无 secret；最小可用 OpenAI-compatible endpoint 保持。 | 环境变量继承泄漏、错误体回显 key、凭据缓存跨请求失效。 | 不一次追平 Pi 的 provider 数量；先建立一个严格 contract 和 faux-provider suite。 |
| **P0.6** | 让 steer 在 step 安全点进入 loop；interrupt、follow-up、并行 tool result 的事件与模型顺序固定。依赖：P0.4。 | 并行完成顺序随机时，持久化和下一请求仍按模型 call 顺序；steer 不丢失、不重复；interrupt 后 provider/tool delta 不再提交；至少 100 次随机调度测试一致。 | live queue 与 durable ledger 双重真源；取消竞态产生孤儿执行。 | 不复制仅 UI 队列；所有模型可见输入必须有 durable causation。 |
| **P0.7** | 建立只读 golden/E2E/eval 套件。依赖：P0.3–P0.6。 | 至少 10 个真实仓库只读任务，要求 10/10 完成；daemon crash/restart 后 subscribe 无 seq 缺口；replay provider/tool/network 调用计数为 0；现有 Rust/TS 测试继续全绿。 | 测试只验证 mock happy path，无法发现上下文和大仓库问题。 | 不用模型单次回答质量代替 harness 验收；分别记录工具正确性、恢复正确性和答案质量。 |

### Goal 后实施结果

| 原计划项 | 当前状态 | 已验证结果 / 明确保留项 |
|---|---|---|
| P0.1 协议与类型 | ◐ | Rust 稳定 schema 与 drift gate 保持通过；完整 TypeScript 自动生成仍待完成，不在文档中伪装为已交付。 |
| P0.2 注册与执行边界 | ✅ 核心范围 | daemon 只向支持 tool calls 的 provider 发布三个固定 `ToolSpec`，未知/未协商工具不执行。通用可扩展 ToolRegistry/Executor seam 延后到不破坏 sealed authority 的后续阶段。 |
| P0.3 有界只读工具 | ✅ | `workspace.list/read/search` 已接入；严格 JSON、稳定顺序、实际 resolved read effect、路径/symlink/hardlink 防护，以及文件数、深度、字节、匹配和输出硬上限均有测试。 |
| P0.4 多轮 loop | ✅ | tool-call JSON 分片汇聚、模型轮次/调用/结果预算、ToolCall/ToolResult 回灌和再次 provider 请求已接通。 |
| P0.5 凭据 | ⏳ | 无凭据 OpenAI-compatible endpoint 保持可用；`CredentialBroker`、轮换与 secret conformance 仍待完成。 |
| P0.6 顺序、steer、cancel | ✅ 核心范围 | 多个只读调用并发执行但按模型调用顺序入账；每轮请求前重载 ledger 注入 steer；取消后不提交残余 provider delta 或工具结果。 |
| P0.7 验证 | ✅ 核心门禁 / ⏳ 扩展 eval | 真实 JSON-RPC 纵向测试覆盖 `client -> daemon -> provider -> workspace.read -> provider -> answer`，Rust/TS 仓库 gate 通过；10 个真实仓库的长期 eval scorecard 继续作为累计指标。 |

### P0 退出门槛

- [x] 真实执行 `client/yeux -> yeuxd -> provider -> read/search -> provider -> answer`。
- [x] `tools` 不再为空，模型 tool call 不再以 `tool_use_unsupported` 终止。
- [x] 只读模式在执行层只分派三个 fixed built-in tool；无 write/process/network/secrets effect。
- [x] ToolCall/ToolResult、Invocation 和 resolved effect 进入 append-only ledger；纯 replay 不实例化 provider/tool/network。
- [x] `apply_patch`、process、MCP 和 plugin tool 仍未暴露给模型。

因此，本次 P0 Goal 可以关闭。`CredentialBroker`、完整 TypeScript/JSONL parity、FTS 和扩大后的真实仓库 eval 仍是明确开放项，但不会把当前实现倒退描述成“单次无工具请求”。

---

## 8. P1：受保护的“读、改、测、修”闭环

**目标**：形成 YeuX 的真正差异化——所有写文件与进程副作用只能通过同一条强制管线，sandbox/approval 不可由 TUI、插件或特殊工具绕过。

| ID | 交付物与依赖 | 验收指标与测试 | 主要风险 | 明确不照搬 |
|---|---|---|---|---|
| **P1.1** | 实现唯一 invocation lifecycle：`validate -> prepare effects -> policy -> approval -> sandbox -> execute -> normalize/redact -> persist`。依赖：P0 ToolExecutor。 | 所有 built-in tool 只能通过同一入口；状态严格 `proposed/approved/prepared/started/terminal`；每个 terminal 结果可追溯 tool version、args/effect digest、policy 来源和 causation。 | 特殊“内部工具”绕过入口；prepared 与实际执行参数不一致。 | 不复制工具各自实现权限逻辑；tool 只声明 effect，authority 统一决策。 |
| **P1.2** | 接通 capability intersection、project trust、turn override 和精确 ApprovalBinding；完成 TUI approval/request。依赖：P1.1。 | 修改参数、tool version、workspace identity、effect 或过期时间后旧审批全部失效；deny/timeout/cancel 有稳定事件；审批永不突破 host ceiling。 | “允许本次”被错误缓存成宽泛 grant；UI 展示与实际 effect 不一致。 | 不照搬 Pi 的字符串危险命令示例作为安全边界；不以弹窗数量代表安全。 |
| **P1.3** | 接入 base-hash `workspace.apply_patch`、原子替换、Git status/diff/checkpoint 和冲突响应。依赖：P1.1/P1.2。 | 人工并发修改触发 stale base hash，不覆盖用户内容；symlink/hardlink/rename/大小写/Unicode/跨设备写测试；每次改动提供可审核 diff。 | TOCTOU、硬链接修改 workspace 外对象、错误 rollback 丢失用户工作。 | 不直接让模型自由 `write whole file` 作为默认编辑路径；优先 patch + revision guard。 |
| **P1.4** | 将 `ProcessExecutor` 与 Seatbelt/bubblewrap 接入统一管线；完善进程树监督、PTY、stdout/stderr 分离、timeout。依赖：P1.1/P1.2。 | sandbox 不可用时 fail closed；shell 重定向、子 shell、`setsid`/`setpgid`、超时、取消、输出洪泛、最小环境测试；父取消后无孤儿进程。 | launcher 环境注入、脱离 PGID、sandbox profile 与 effect 不一致。 | 不照搬 Grok 的 sandbox 默认 off，也不照搬 Pi 依赖用户自行容器化作为默认本地安全模型。 |
| **P1.5** | artifact、输出裁剪、跨 chunk secret redaction、quota；工具网络代理与 private/metadata/DNS rebinding/proxy bypass 防护。依赖：P1.1/P1.4。 | stdout/stderr/artifact 各有上限；裁剪后保留 hash/长度/引用；secret 跨边界分片仍可删改；默认无 network capability，网络策略有 DNS 与 IP 二次验证测试。 | 裁剪破坏可诊断性；敏感值在删除前已写入 ledger；代理与子进程直连绕过。 | 不让每个 provider/tool 自己决定 redaction；不在 P1 引入通用浏览器自动化。 |
| **P1.6** | 为非幂等副作用实现 unknown/reconciliation，并建立 crash-window matrix。依赖：P1.1–P1.5。 | 在批准后、prepared 后、started 后、副作用完成后、artifact 发布前后、SQLite commit 前后注入崩溃；未知外部 effect 不自动重试；幂等调用按策略恢复。 | 把“没有 terminal event”误判为“未执行”；重启后重复副作用。 | 不照搬简单“重启继续 loop”；YeuX 必须优先事实一致性而非表面连续运行。 |
| **P1.7** | 真实编码 E2E、TUI diff/approval/tool timeline 与 headless parity。依赖：全部 P1。 | 至少 20 个隔离测试仓库任务，≥18 个完成读改测修；100% 显示最终 diff 与验证命令结果；交互与 JSONL 对同一 ledger 投影一致；无未经批准 effect。 | 只测小仓库；模型偶然成功掩盖执行边界错误。 | 不先做视觉复杂度；TUI 首先服务于可审核 diff、effect、approval 和恢复。 |

### P1 / v0.1 发布门槛

- 用户能在真实仓库安全完成“读、改、测、修”。
- 所有写与 process effect 均有 `PreparedInvocation`、policy decision、必要审批、sandbox evidence 和 terminal event。
- base hash 冲突不覆盖用户内容；sandbox 缺失不降级为无隔离执行。
- crash 注入不会静默重复未知非幂等副作用。
- 此阶段完成前，不接入不可信插件、通用 MCP server、后台 schedule 或子智能体写任务。

---

## 9. P2：Provider、上下文与受限扩展生态

**目标**：在 P1 安全边界之上补齐主流 provider、长期上下文、Skills、MCP 和插件能力，使 YeuX 从“可工作的安全 agent”成为可扩展 harness。

| ID | 交付物与依赖 | 验收指标与测试 | 主要风险 | 明确不照搬 |
|---|---|---|---|---|
| **P2.1** | 定义 provider conformance suite；实现 OpenAI Responses、Anthropic Messages、Gemini 和 OpenAI-compatible profiles。依赖：P0 provider contract、P1 稳定 loop。 | 每类 adapter 通过 tool JSON 分片、重复 ID、reasoning、usage、429/retry-after、超时、断流、取消、overflow、图片能力协商测试；默认不跨 provider fallback。 | 为兼容网关在核心层加入大量 provider 特判；usage/cost 口径漂移。 | 不追求 Pi 的 provider 数量；adapter 特性必须由 capability negotiation 表达。 |
| **P2.2** | token-aware context builder：system prompt、项目指令、文件上下文、tool result budgets、provider cache hints。依赖：P0 loop。 | 每次 request 有可解释 token breakdown；AGENTS/override 继承与 trust 测试；大工具输出不挤掉最新用户意图；同一 ledger 范围可重建输入。 | 未受信项目指令自动提权；token 估算与真实 provider 差异过大。 | 不把所有仓库文件预加载；按需工具读取并保留 source seq。 |
| **P2.3** | checkpoint compaction、FTS5 session search 和用户批准的 curated memory。依赖：P2.2、现有 ledger。 | compaction 记录覆盖的 source seq range、prompt、model 和摘要 hash；原始事件永不删除；context overflow 自动压缩后可重试；memory 默认关闭或显式批准。 | 摘要漂移、隐私泄漏、错误记忆长期污染。 | 不先上向量数据库；使用 ledger + FTS + 可追溯摘要，保持删除和审计简单。 |
| **P2.4** | 实现 `SKILL.md`/agentskills.io 加载、分层发现、来源 digest、project trust 和延迟正文加载。依赖：P2.2/P1 policy。 | 未信项目 skill 不执行；变更 digest 使旧信任失效；一万个 skill 只注入摘要索引；技能正文有字节/文件预算。 | skill 内容成为隐式系统权限；符号链接和包更新替换受信内容。 | 不把 skill 当作能力 grant；它只能提供说明，真正工具仍受 policy。 |
| **P2.5** | MCP stdio/Streamable HTTP、lazy tool discovery、attachment/resource/prompt 映射。依赖：P1 pipeline、P2.4 trust。 | 10,000 MCP tools 不一次性进入上下文；server crash/reconnect/cancel/timeout/重复名称测试；MCP tool effect 必须映射到 YeuX capability，无法声明则拒绝或最小权限。 | MCP server 在 agent sandbox 外启动；工具 schema 或返回值资源耗尽；远端 auth 泄漏。 | 不照搬“连接即信任”；server process、网络和 tool effect 分别授权。 |
| **P2.6** | 将 plugin host 接入 Rust policy/approval/sandbox/ledger；限制插件只能贡献 tools/providers/commands。依赖：P1/P2.5。 | 摘要变化、未声明 capability、超时、崩溃、协议违规均 fail closed；插件不能替换 policy、ledger、approval 或 UI authority；host 退出不损坏 daemon。 | hash-to-exec TOCTOU、插件供应链、同进程快捷路径绕过。 | 不照搬 DeepSeek 的“所有东西都是插件”；YeuX 的安全内核保持 sealed。 |
| **P2.7** | `yeux doctor`、`yeux policy explain`、provider/MCP/plugin 诊断与本地安全报告。依赖：P2.1/P2.5/P2.6。 | 用户可看到有效 capability 的每一层来源、sandbox 可用性、provider/MCP 连接状态和被拒原因；输出自动删改 secret；JSON 与人类模式一致。 | 诊断本身泄漏路径、环境或凭据；解释与实际 evaluator 不一致。 | 不维护第二套解释逻辑；解释直接来自执行所用 decision object。 |

### P2 退出门槛

- 四类 provider 通过同一 conformance suite。
- context/compaction 的每个模型可见事实都可追溯到 ledger 或受信配置 digest。
- Skills 不授予能力；MCP/plugin 不能绕过 P1 pipeline。
- 10,000 工具/技能规模下仍采用延迟发现，启动和首请求不会线性注入所有 schema。

---

## 10. P3：产品体验、后台任务与隔离子智能体

**目标**：补齐 Grok Build/Codex 的长任务操作体验、DeepSeek 的后台 capability 和 Pi 的高效 TUI，同时仍让 daemon 保持唯一 authority。

| ID | 交付物与依赖 | 验收指标与测试 | 主要风险 | 明确不照搬 |
|---|---|---|---|---|
| **P3.1** | 用 OpenTUI 或等价架构重建产品 TUI：composer、`@file`、tool timeline、diff review、approval detail、thread browser、model/mode、usage/context。依赖：P1/P2 稳定协议。 | 80×24、宽屏、无彩色/低能力终端均可用；1 万事件线程滚动无明显阻塞；所有不可信文本走统一 sanitizer；JSONL 保持原始协议值。 | UI 为方便直接访问文件/执行命令；渲染大输出耗尽内存。 | 不复制 Grok/Codex 的全部面板；优先完成 YeuX 的 effect、approval、replay 可解释性。 |
| **P3.2** | launchd/systemd daemon、持久 Job runtime、预算/取消/输出收集。依赖：P1 process、P2 credentials。 | daemon 重启后 Job 状态明确；父取消终止资源；并发、token、cost、wall time、artifact quota 可级联；无交互 approval 时进入 waiting 状态。 | daemon service 获得过宽环境和凭据；重启重复任务。 | 不把后台命令作为无主进程；每个 Job 必须有 owner、snapshot 和 terminal fact。 |
| **P3.3** | session-local schedule、loopback webhook、休眠/DST/missed-run/reentrancy 语义。依赖：P3.2。 | 固定时区测试；错过多个周期最多补跑一次；默认不重入；webhook 鉴权和重放防护；未预授权 external write 不执行。 | 时钟漂移、重复触发、后台隐式扩大能力。 | 不复制通用云自动化平台；v1 只做本地、有限、可审计触发。 |
| **P3.4** | 一层子智能体、默认并发 4；只读共享 workspace，写任务强制独立 Git worktree。依赖：P1/P3.2。 | capability、token、cost、time、model 与取消向下级联；子级不能提权或写父 worktree；父取消清理子级与进程；有改动 worktree 不自动删除。 | 多 agent 同仓竞争、孤儿 worktree、预算放大、循环委派。 | 不先实现无限 agent team；只做一层、显式 spawn、严格预算的最小闭环。 |
| **P3.5** | 结构化 `AgentResult`、父级 review、handoff 和显式 merge。依赖：P3.4。 | handoff 包含结论、证据、artifact/diff/test、未决风险；merge 冲突返回 review 状态；任何自动合并默认关闭。 | 子级自然语言结果缺证据；父级盲信；重复 handoff。 | 不照搬“子 agent 完成即合并”；YeuX 以父级审查和显式 merge 为边界。 |
| **P3.6** | 稳定 headless/RPC SDK 或轻量 app-server client kit，支撑编辑器和 CI。依赖：P3.1 协议稳定。 | 同一 Thread 在 TUI、JSONL、SDK 中投影一致；生成 TS client 通过兼容套件；背压、重连、afterSeq、版本协商 E2E；示例 IDE 只走 daemon。 | 再造第二运行时；SDK 绕过用户 approval。 | 不复制 Codex app-server 的全部产品/企业接口；只公开已通过阶段门槛的核心 API。 |

### P3 退出门槛

- 长任务可从 TUI 观察、steer、暂停/取消、恢复，并能解释成本、上下文、工具和审批。
- Job/schedule/subagent 的权限和预算只向下收紧。
- 写子任务从不共享父工作树；merge 始终显式。
- 所有客户端都只驱动 daemon，不出现 TUI/SDK 特权执行路径。

---

## 11. P4：v1 发布与长期工程成熟度

**目标**：把功能完整的本地 harness 做成可安装、可升级、可恢复、可审计的稳定 v1，而不是只在源码 checkout 中运行。

| ID | 交付物与依赖 | 验收指标与测试 | 主要风险 | 明确不照搬 |
|---|---|---|---|---|
| **P4.1** | SQLite schema migration、备份/恢复、一致性检查、损坏隔离。依赖：P0–P3 event vocabulary 稳定。 | 从每个已发布 schema 升级到当前版本；故障注入不会半迁移；备份可恢复并校验 seq/hash；不支持的新版本明确拒绝。 | 自动迁移破坏 append-only 语义；回滚后新事件不可解释。 | 不无限维护 pre-release 草稿格式；v1 前明确支持窗口并提供导出工具。 |
| **P4.2** | artifact GC/quota、FTS maintenance、性能基准、24h soak、资源泄漏检查。依赖：P2/P3。 | 24h 多线程/多 Job soak 无进程、FD、内存、artifact 泄漏；大线程分页/订阅 p95 有固定预算；GC 不删除被 ledger 引用对象。 | GC 与并发发布竞态；性能优化破坏确定性。 | 不为 benchmark 绕过持久化或安全检查；生产路径与基准路径一致。 |
| **P4.3** | 依赖固定、RustSec/npm audit、license allowlist、SBOM、provenance、可复现构建。依赖：依赖图冻结。 | CI 阻止未知许可证/生命周期脚本/高危漏洞；生成 SPDX/CycloneDX；两台干净构建机产物 hash 一致或记录受控差异；第三方代码 NOTICE 完整。 | 供应链门禁长期失修；动态下载绕过 lockfile。 | 学习 Pi 的具体门禁，但不复制其 npm-only 假设；Rust/TS/打包产物统一追踪。 |
| **P4.4** | 签名 macOS/Linux release、校验和、installer、Homebrew；打包匹配版本 `yeux`/`yeuxd`/plugin host。依赖：P4.3。 | 干净 macOS/Linux 一条命令安装、升级、降级/卸载；TUI-daemon 协议不兼容拒绝；无需预装 Node/Bun；发布资产签名和 checksum 验证。 | 三组件版本错配；自动更新供应链；codesign/notarization 差异。 | P4 不顺带承诺 Windows；先把 v1 明确平台做到可重复发布。 |
| **P4.5** | 本地 observability、诊断 bundle、用户/扩展作者/安全/恢复文档。依赖：P2.7/P3。 | telemetry 默认关闭；诊断 bundle 经过 secret/path redaction；文档命令每次 release 做 smoke；扩展示例通过 policy negative tests。 | 日志成为第二数据泄漏面；文档与协议漂移。 | 不默认上传会话或遥测；任何远端指标必须显式 opt-in。 |
| **P4.6** | 固定竞争回归与真实任务 eval scorecard。依赖：全阶段。 | 每个 release 执行只读、编码、恢复、安全、长任务五类套件；报告 task success、未授权 effect、crash recovery、token/cost、latency；关键指标不得静默回退。 | 针对固定题过拟合；把模型质量误归因于 harness。 | 不做单一总分排名；分别报告 harness correctness、model quality 和安全/恢复指标。 |

### P4 / v1 发布门槛

- P0–P3 的所有阶段门槛保持通过。
- 24 小时 soak 与故障注入无静默数据损坏、孤儿进程或重复副作用。
- 安装、升级、备份恢复和协议版本错配在干净机器上通过。
- SBOM、许可证、签名、校验和、provenance 与 release smoke 完整。
- 真实编码任务成功率、安全违规率、恢复正确性和成本均有版本化报告。

---

## 12. 建议的 Goal 执行组织

### 12.1 单一 active goal

本次 Goal objective 为：

> 将 YeuX 从单请求运行时推进到通过 P0 核心门槛的只读 coding harness：完成预算化只读工具、多步 provider-tool loop、确定性并发结果、steer/cancel 顺序语义与真实 JSON-RPC E2E；在所有测试通过前不开放写、process、MCP 或 plugin tool。

**Goal 后状态：✅ objective 已达到，代码与文档门禁通过。** 下一主线应单独创建 P1 Goal；`CredentialBroker`、完整协议/JSONL parity、FTS 和扩大 eval 作为明确收尾项进入对应里程碑，而不是继续用一个无限期 Goal 混合 P1–P4。

### 12.2 Agent 团队分工

本次 P0 使用的工作流分解仍可复用于后续阶段，始终由根任务负责集成：

| 工作流 | 负责范围 | 不得触碰 |
|---|---|---|
| A：协议与生成 | schema/TS、wire fixtures、JSONL parity | 不改 runner 执行语义 |
| B：工具与预算 | workspace 工具、effect、资源上限 | 不开放 write/process |
| C：Agent loop | provider-tool loop、steer/cancel、确定顺序 | 不引入第二执行 authority |
| D：验证与对抗性回归 | faux provider、E2E、crash/replay、扩展 eval | 不替实现者降低验收标准 |

本次已验证的集成顺序是：只读 schema/spec → 有界工具 → loop → steer/cancel/顺序 → wire E2E → 全量 gate。P1 仍必须先建立唯一 invocation pipeline，再接 policy/approval、patch 和 process，不能引入临时旁路。

### 12.3 每个 Goal turn 的纪律

1. 先读取当前 Goal 与剩余预算，再选择一个最小可验证 work item。
2. 每项先写/更新测试，再接入生产路径；禁止用 README 勾选代替 daemon 可达性。
3. 所有新工具必须回答五个问题：参数如何规范化、effect 是什么、预算是什么、取消如何收敛、结果如何持久化。
4. 每次合并前运行目标测试；每个阶段退出前运行全量 Rust/TS/schema/golden/E2E。
5. 出现重复三轮的同一外部阻塞时才标记 Goal blocked；工作困难、测试慢或尚未实现不是 blocked。
6. 只有阶段退出门槛全部满足、文档同步且无必需工作剩余时才能标记 complete。

---

## 13. 统一验收仪表板

以下指标从 P0 开始持续累积，避免后续以功能数量掩盖安全或恢复回退。

### 13.1 Goal 后 P0 门禁快照

| 门禁 | 2026-08-31 当前值 |
|---|---|
| 只读纵向链路 | ✅ JSON-RPC `client -> daemon -> provider -> workspace.read -> provider -> answer` |
| 工具边界 | ✅ 仅 `workspace.list/read/search`；未知/未协商工具无副作用 |
| 确定顺序 | ✅ 并发只读结果按模型调用顺序持久化并回灌 |
| steer / cancel | ✅ steer 在下一模型请求安全点进入上下文；取消后残余结果不提交 |
| Rust gate | ✅ `cargo fmt --all --check`、`cargo test --workspace --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings` |
| TypeScript gate | ✅ typecheck、49/49 tests、build |
| 仍开放 | `CredentialBroker`、完整 TypeScript/JSONL parity、FTS、10-task 持续 eval |

### 13.2 跨阶段累计目标

| 维度 | P0 | P1 | P2 | P3 | P4 |
|---|---:|---:|---:|---:|---:|
| 真实任务套件 | 核心 wire E2E 已通过；10 个只读 eval 持续补齐 | 20 个编码，≥18/20 | 四类 provider 同题一致性 | 长任务/多 agent 场景 | 每 release 全套 |
| 未授权副作用 | 0 | 0 | 0 | 0 | 0 |
| replay 外部调用 | 0 | 0 | 0 | 0 | 0 |
| crash-window 覆盖 | provider/ledger | write/process/artifact | provider/compaction/plugin | job/schedule/subagent | migration/update |
| schema/type drift | 0 | 0 | 0 | 0 | 0 |
| orphan process/worktree | N/A | 0 process | 0 plugin/MCP child | 0 process/worktree | 24h soak 为 0 |
| secret 泄漏 fixtures | provider | tool/artifact/network | credential/MCP/plugin | job/handoff | diagnostic/release |
| 资源预算 | provider + read tools | process + artifact | context + catalog | jobs + agents | 全系统 soak |

成功率之外必须单独报告：

- harness 是否选择了正确工具；
- 工具是否按规范执行；
- ledger/replay 是否正确；
- 是否发生未经授权 effect；
- 模型答案是否正确；
- token、费用、耗时和重试次数。

这样可以避免把“模型更强”误认为 harness 设计更好，也避免把“最终答案看起来对”误认为执行路径安全。

---

## 14. 战略取舍

### v0.1 必须有

- P0 核心只读 loop（已完成并持续回归）。
- `CredentialBroker`、交互/JSONL parity 等 M1 收尾门禁。
- P1 全部。
- 一个成熟交互 TUI 的最小 diff/approval/tool timeline 子集。
- 可安装的开发预览包可以晚于 P1，但不可把源码 checkout 当正式 v0.1。

### v0.1 不需要追平

- Grok Build 的插件市场、完整 dashboard、memory consolidation。
- Codex 的 Desktop/IDE/云端/企业/实时接口广度。
- DeepSeek Harness 的所有 Cordis plugin seam、Web UI 全功能和 Python SDK。
- Pi 的数十个 provider catalog。

### v1 继续明确排除

- Windows 正式支持。
- 远程/云 sandbox。
- 消息平台与语音。
- 企业控制面与插件市场。
- 向量记忆。
- 子智能体自动合并。

这些取舍不是长期否定，而是为了保证 YeuX 的核心承诺先在较小状态空间内得到证明。

---

## 15. 官方来源

### 15.1 YeuX 本地事实源

- [README：当前实现与明确限制](../README.md)
- [ARCHITECTURE：authority、ledger、runner 与执行边界](ARCHITECTURE.md)
- [ROADMAP：现有 M0–M5 门槛](ROADMAP.md)
- [PROTOCOL：daemon 命令与事件面](PROTOCOL.md)
- [THREAT_MODEL：信任边界](THREAT_MODEL.md)
- [2026-08-30 安全审计报告](audits/2026-08-30-run-1/REPORT.md)

### 15.2 Grok Build

- 固定源码快照：<https://github.com/xai-org/grok-build/tree/bc7f02eddd3d84085849dc19ed216f11c23b0571>
- 固定 commit：<https://github.com/xai-org/grok-build/commit/bc7f02eddd3d84085849dc19ed216f11c23b0571>
- README / 产品与仓库结构：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/README.md>
- crate 版本：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager-bin/Cargo.toml>
- `SOURCE_REV`：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/SOURCE_REV>
- 官方在线文档：<https://docs.x.ai/build/overview>
- MCP：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager/docs/user-guide/07-mcp-servers.md>
- Skills / Plugins / Hooks：<https://github.com/xai-org/grok-build/tree/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager/docs/user-guide>
- Subagents：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager/docs/user-guide/16-subagents.md>
- Sessions：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager/docs/user-guide/17-sessions.md>
- Sandbox：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager/docs/user-guide/18-sandbox.md>
- Background tasks：<https://github.com/xai-org/grok-build/blob/bc7f02eddd3d84085849dc19ed216f11c23b0571/crates/codegen/xai-grok-pager/docs/user-guide/20-background-tasks.md>

### 15.3 OpenAI Codex

- 固定源码快照：<https://github.com/openai/codex/tree/d58d0e5841e0de08e251673db2d5af8cf3a1ad51>
- 固定 commit：<https://github.com/openai/codex/commit/d58d0e5841e0de08e251673db2d5af8cf3a1ad51>
- 2026-08-29 Release `rust-v0.151.0`：<https://github.com/openai/codex/releases/tag/rust-v0.151.0>
- 官方 Codex 文档：<https://developers.openai.com/codex>
- CLI 功能：<https://developers.openai.com/codex/cli/features>
- 非交互执行：<https://developers.openai.com/codex/noninteractive>
- Sandbox、approvals 与安全：<https://developers.openai.com/codex/security>
- AGENTS.md：<https://developers.openai.com/codex/guides/agents-md>
- Skills：<https://developers.openai.com/codex/skills>
- app-server 协议：<https://github.com/openai/codex/blob/d58d0e5841e0de08e251673db2d5af8cf3a1ad51/codex-rs/app-server/README.md>

### 15.4 DeepSeek Harness

- 固定源码快照：<https://github.com/deepseek-ai/deepseek-harness/tree/0a53fb55bea101816fa226bb964ae2bed71c343b>
- 固定 commit：<https://github.com/deepseek-ai/deepseek-harness/commit/0a53fb55bea101816fa226bb964ae2bed71c343b>
- tag `dsh-v0.1.2-alpha.2`：<https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.1.2-alpha.2>
- package 版本：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/package.json>
- 官方文档：<https://deepseek-harness.github.io/deepseek-harness/>
- 架构与 turn flow：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/docs/architecture.md>
- Agent loop：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/docs/subsystems/core.md>
- Tool pipeline：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/docs/subsystems/tools.md>
- Web UI 使用指南：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/docs/user/guide/index.md>
- Provider 配置：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/docs/user/guide/providers.md>
- Session persistence：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/docs/subsystems/persistence.md>
- 安全声明：<https://github.com/deepseek-ai/deepseek-harness/blob/0a53fb55bea101816fa226bb964ae2bed71c343b/SAFETY.md>

### 15.5 Pi Agent Harness

- 固定源码快照：<https://github.com/earendil-works/pi/tree/853a80d26c90a14c1886f0ebb8ffaae133ca2185>
- 固定 commit：<https://github.com/earendil-works/pi/commit/853a80d26c90a14c1886f0ebb8ffaae133ca2185>
- Release `v0.84.4`：<https://github.com/earendil-works/pi/releases/tag/v0.84.4>
- coding-agent package 版本：<https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/package.json>
- 官方项目文档：<https://pi.dev/docs/latest>
- coding-agent README：<https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/README.md>
- Agent loop：<https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/agent/README.md>
- 多 Provider API：<https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/ai/README.md>
- Extensions：<https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/docs/extensions.md>
- Sessions/compaction：<https://github.com/earendil-works/pi/tree/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/docs>
- Security / 无内置 sandbox：<https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/docs/security.md>

---

## 16. 最终决策

YeuX 不应试图在短期成为“功能最多的 harness”。它应成为：

> **一个先证明副作用、恢复和审计语义，再以小步方式补齐产品能力的本地 coding-agent runtime。**

P0 已完成，证明 YeuX 能用 append-only ledger、resolved read effect 和确定性顺序承载真实只读工具 loop。短期胜负手现在是 P1：用比 Pi 更强的默认安全边界、比纯 UI agent 更清晰的 event/replay 语义，交付一个真正能完成读改测修的最小产品。P2 以后才扩展 provider、MCP、Skills、插件和上下文；P3 再进入长任务、自动化和子智能体；P4 用可安装、可升级、可审计的 release 把架构优势转化为用户信任。

在每一个阶段，唯一可接受的“完成”定义都是：**daemon 执行路径已接通、失败与崩溃路径已测试、协议和文档已同步、全量 gate 通过。**
