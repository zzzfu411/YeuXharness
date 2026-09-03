# Run 4：项目与 GitHub 状态审计及执行计划

审计日期：2026-09-03（Asia/Shanghai）  
审计范围：本地工作区、`origin/main`、GitHub Actions、开放 Pull Request、发布保护设置  
执行目标：在不改变历史审计记录的前提下，确认 M2 合并后的真实能力，修正文档漂移，并为下一阶段建立可验收计划。

## 1. 可复现的基线

| 项目 | 结果 |
|---|---|
| 本地分支 | `main`，工作区干净，跟踪 `origin/main` |
| 当前主线 | `51c631ceca5a5cf07627226eb45ca8653b0c69a3` |
| 同步动作 | 本地原先落后远端 21 个提交，已 fast-forward，无额外 merge commit |
| 仓库 | [zzzfu411/YeuXharness](https://github.com/zzzfu411/YeuXharness)，公开、Apache-2.0、默认分支 `main` |
| 主线 CI | GitHub Actions run `33662133838`：Rust macOS、Rust Ubuntu、TypeScript 全部成功 |
| 分支保护 | `main` 未配置 branch protection/ruleset；合并不强制要求 CI |
| 开放 PR | #2（非 draft）、#4（draft）、#6（draft）均为 CLEAN 且最近检查成功；#1 为旧截图 PR，DIRTY，历史 Rust 检查失败/取消 |

本地门禁结果：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`pnpm typecheck`、`pnpm test`、`pnpm build` 均成功。Rust 全量测试在受限环境下有 5 个需要本机能力的 fixture 失败：3 个 provider HTTP fixture 无法绑定 loopback，2 个 sandbox mutation fixture 无法启动 OS sandbox；使用主机权限重跑后全部成功。该差异是执行环境权限，不是主线代码回归。

## 2. 当前实现结论

### 已进入 daemon 权威路径的能力

1. `yeuxd::InvocationPipeline` 已成为副作用工具的唯一 authority path：注册、参数规范化、effect 计划、四层 capability 交集、沙箱检查、审批绑定、执行前重计划/重验证、一次性 token 和 opaque `ExecutionPermit` 均在 Rust daemon 内完成。
2. `workspace.apply_patch` 已作为隐藏 adapter 注册，并在沙箱内通过受限 writer 发布替换内容；发布后再次读取并校验 revision，失败关闭。
3. `process.run` 已作为隐藏 adapter 注册，要求绝对可执行文件和 workspace 内 cwd，网络关闭、workspace 写入关闭，并通过异步 `ProcessExecutor` 执行。
4. runner 已在 sandbox 能力可用且 host ceiling 非 `observe` 时向 provider 广告写入/进程工具；无沙箱时不广告，直接拒绝副作用准备。审批请求由 daemon 发起，TUI 已实现 allow-once/deny/inspect（deny 默认）和 unified diff 展示。
5. Invocation 的 `proposed -> approved -> prepared -> started -> completed/failed/unknown` 生命周期及 ToolResult 原子入账已覆盖；取消、超时、崩溃后无法证明停止时进入 Unknown/reconciliation-required，而不是假装成功或自动重试。
6. `CredentialBroker`、opaque `CredentialLease` 和 provider 的 broker credential source 已存在；秘密不会进入普通事件、工具参数或 launcher 环境。

### 尚未达到 v0.1 发布门槛的能力

1. daemon CLI 仍通过 `OpenAiCompatibleProvider::without_credentials` 构造 provider，`CredentialBroker` 尚未从配置/密钥存储注入到实际 daemon。
2. `FileRevisionSnapshot` 尚未完整地从 mutation prepare 绑定到 execute；中间目录被替换、path-based rename 与严格 dirfd-relative CAS 仍是残余竞态。当前实现不能宣称可防护 hostile shared workspace 的全部竞态。
3. process 的 PGID 清理已覆盖常规子进程、超时和取消，但主动 `setsid`/`setpgid` 脱组的后代仍可能存活；Linux PID namespace/cgroup 与 macOS supervisor/job 机制未完成。
4. Unknown 目前可持久化并阻止自动继续，但完整 reconciliation 命令、外部状态读取、用户决策 UI 和安全 retry 规则仍未闭环。
5. 大型工具结果尚未统一落入 artifact store；网络代理、私网/metadata/DNS rebinding 防护、MCP/插件接入统一 policy 仍未实现。
6. TUI 已有审批与纸张风格 fixture，但 `--jsonl` 与交互 TUI 对同一真实 daemon trace 的完整 parity、真实仓库“读-改-测-修”E2E、崩溃注入矩阵仍需补齐。
7. GitHub 没有保护规则；CI 只有 fmt/clippy/schema/test 和 TypeScript 基线，缺少安全审计、依赖更新、构建产物/SBOM、发布签名及固定 action SHA 门禁。

## 3. 风险分级

| 优先级 | 风险 | 影响 | 处理阶段 |
|---|---|---|---|
| P0 | provider/daemon 没有真实 credential broker 注入 | 真实带鉴权 provider 无法安全使用，容易诱发环境变量旁路 | 阶段 1 |
| P0 | prepare→execute 的文件身份/目录 CAS 不完整 | 共享工作区竞态可能让获批目标与实际发布目标不一致 | 阶段 2 |
| P0 | `main` 无保护且开放 PR 重复 | 失败或过时 PR 可绕过审查进入主线 | 阶段 1（治理） |
| P1 | 进程树脱组与网络隔离不完整 | 恶意/异常命令可能留下进程或尝试外联 | 阶段 3 |
| P1 | Unknown 缺少完整 reconciliation UX | 重启/超时后无法安全恢复任务 | 阶段 4 |
| P1 | 真实仓库 E2E 与 crash matrix 不足 | 只读和 mutation 单测通过仍可能遗漏跨边界故障 | 阶段 5 |
| P1 | prepared/consumed token 集合当前无 TTL 回收 | 长期运行 daemon 可被大量准备调用逐步耗尽内存 | 阶段 2 |
| P2 | 发布、SBOM、依赖和 action 固定策略缺失 | 供应链和交付可重复性不足 | 阶段 6 |

## 4. 接下来六阶段计划

### 阶段 1：基线、文档和仓库治理（本轮执行）

- 更新 README、ROADMAP、ARCHITECTURE，使 M2 已合并能力与残余限制准确可见。
- 保留 Run 3 历史记录，新增本审计记录作为当前事实源。
- 建立 GitHub 分支/PR 清理清单：保留 #2/#4/#6 的取舍由维护者确认，不自动关闭或删除；为 `main` 配置必需 CI 检查和线性合并策略。
- 为 CI 增加后续门禁的占位任务设计（依赖审计、E2E、构建/SBOM），先不在本轮引入未验证的供应链工具。

验收：文档不再声称 M2 “未接入”；本地与远端 SHA、CI、PR、保护状态均可由命令复核；工作区门禁通过。

### 阶段 2：文件 CAS 与 mutation 安全闭环

- 为 `PreparedWorkspaceMutation` 绑定完整 `FileRevisionSnapshot`（device/inode/digest/权限），从 prepare 传到 execute。
- 持有 workspace root dirfd，使用逐组件 `openat`/Linux `openat2` 或等效安全实现，消除 canonicalize 后目录替换和 path rename 竞态。
- 为 prepared/consumed token 增加过期回收、并发安全的容量上限和指标；过期 token 必须保持 fail closed，不得重新激活。
- 增加同字节新 inode、目录替换、硬链接、并发修改和注入崩溃测试；证明批准后目标未改变才可发布。

验收：在 adversarial shared-workspace fixture 中，所有身份变化均 fail closed，用户内容不被覆盖。

### 阶段 3：provider 凭据和进程监督

- 将 `CredentialBroker` 注入 `DaemonConfig`/provider factory，增加 opaque handle 配置校验和 OS keychain 适配 seam。
- 增加 provider credential success/missing/rotation 测试，确保 lease 不进入事件、日志、工具结果或子进程环境。
- 引入 Linux PID namespace/cgroup、macOS supervisor/job 的能力探测与失败关闭；保留输出、超时和取消预算。

验收：带鉴权 provider 可运行；普通环境变量和工具参数无法提供凭据；脱组后代在平台能力范围内被回收。

### 阶段 4：reconciliation 与 artifact

- 增加 `invocation/reconcile`（或等价稳定方法）及事件，明确 Unknown 的外部状态读取、用户选择和幂等 retry 规则。
- 将超出 inline budget 的 stdout/stderr/tool result 写入内容寻址 artifact，ledger 只保存摘要、digest、引用和截断信息。
- TUI/JSONL 显示相同 reconciliation 状态，禁止把 Unknown 渲染成成功。

验收：重启、超时、取消和 worker 崩溃均能恢复到可解释状态；replay 零外部调用；大输出不突破账本/内存预算。

### 阶段 5：真实任务 E2E 与兼容性

- 建立临时测试仓库，覆盖“读→改→测→修”、审批拒绝、并发修改冲突、沙箱不可用、失败后恢复。
- 同一事件 trace 同时跑交互 TUI、`--jsonl` 和纯 replay，比较投影、seq 和终态。
- 用 faux clock/ID/provider、随机并发顺序和崩溃注入固定可重复证据。

验收：发布门槛中的真实任务和 crash matrix 全部绿色，并有可审计 trace。

### 阶段 6：发布与供应链

- 固定 GitHub Actions action SHA，增加 cargo audit/deny、pnpm 审计、许可证清单和 SBOM。
- 生成 macOS/Linux 可复现构建、校验和、签名/验证说明及安装器 smoke test。
- 设置 `main` required checks、CODEOWNERS、PR 模板和 release checklist。

验收：从干净 checkout 可重建、验证和安装；未通过 required checks 的 PR 无法合并。

## 5. 本轮实际执行记录

- 已读取并核对本地实现、历史审计、GitHub 仓库元数据、分支、PR、Actions 与保护规则。
- 已将本地 `main` 从旧基线 fast-forward 到远端 `51c631c`。
- 已运行 Rust/TypeScript 本地门禁；受限 shell 导致的 loopback/OS sandbox fixture 已用主机权限复核通过。
- 已创建本状态与计划文档；下一步应在当前工作区提交文档同步变更，待维护者确认后再进行 GitHub 分支保护/PR 操作。

## 6. 明确不在本轮执行的动作

- 不自动关闭、删除或合并开放 PR。
- 不修改 GitHub branch protection、Secrets、Actions 权限或远端分支。
- 不把未验证的 credential store、network proxy、PID namespace 实现伪装成已完成。
