# Run 5：P0 / M2.5 纵向切片执行记录

执行日期：**2026-09-04（Asia/Shanghai）**<br>
起始代码：**`82c13c570ac33bc5ad1dddadde40baa7c447b158`**<br>
目标：在不削弱 Rust authority、EffectSet、能力交集、审批绑定、OS sandbox、append-only ledger 与 Unknown 恢复语义的前提下，补出一条最小的 `read → plan → patch → approval → test fail → fix → test pass → diff` 路径，并把它变成可重复的测试和可操作的 TUI 控制面。

## 1. 结果判定

本轮完成了 **M2.5 的一个纵向切片**，没有把它标成 v0.1 发布完成：

- daemon runner 新增真实临时 Git 仓库 fixture。脚本 provider 先读取 `answer.txt`，持久化公开计划，提交一个 revision-bound 错误修改，经审批后运行检查并看到非零退出码，再按新 revision 修复、复测通过，最后通过受保护的 `process.run` 取得 `git diff`。最终文件从 `41` 变为 `42`。
- fixture 断言 7 次模型请求、6 个 Invocation、5 次副作用审批、两个 patch、三次 process、一次 read、退出码 `[0, 0, 1]`、最终 diff、assistant 终态和完整 ledger 终态。Linux 且 strict sandbox 可用时执行整条链；当前 macOS 因不具备可证明的 descendant process supervisor，明确跳过该 process E2E。
- daemon wire fixture 在真实 Seatbelt 写入能力可用时，必须先显式把 workspace 从 `untrusted` 提升为 `trusted`，随后才能看到 `approval/request`、允许一次、发布 patch 并从 ledger 重放 terminal evidence。没有 trust 时 policy 必须拒绝；测试不再借“工具已广告”推断 workspace 已获写权限。
- `initialize` 新增 `write_tools_reason` 与 `process_tools_reason`。它们只解释 capability 为何不可用，不构成授权；Rust schema 和 TypeScript wire 类型同步更新。
- 行式 TUI 新增确定性的 slash-command router：`/help`、`/model`、`/doctor`、`/context`、`/plan`、`/resume`、`/compact`、`/interrupt`、`/steer`、`/reconcile`、`/mode`、`/threads`、`/fork`、`/exit`。未知命令不会落入模型 prompt。
- TUI 在 Turn 运行期间继续读取控制命令，`/steer` 与 `/interrupt` 能抵达当前 Turn；审批和 `user/input` 会抢占空闲命令提示，再按队列恢复。EOF 正常收束，审批 EOF 按 deny-default 处理。
- requested mode 在界面和 Turn override 中都经过 `host ceiling ∩ workspace trust ∩ tool readiness` 收紧。`trusted` workspace 的当前 project ceiling 是 `build`，`untrusted` 是 `observe`；客户端不能把 `operate` 显示成已经获得的权限。
- 终端宽度按 grapheme cluster 处理常见 CJK、combining mark、ZWJ emoji、flag、variation-selector emoji 与 keycap；人类输出继续清理控制序列，JSONL 继续保留原始协议 payload。
- TypeScript `RuntimeCommandMap` 补齐当前稳定命令面；Job 内嵌结构保持与 Rust schema 一致的 snake_case wire 字段。

## 2. Authority 不变量检查

| 不变量 | 本轮证据 |
|---|---|
| Rust daemon 是唯一执行 authority | TUI 只发 JSON-RPC；patch、process、trust、reconcile 都由 `yeuxd` 处理。TUI 的 `/plan` 只是明确标注的本地 scratchpad。 |
| 能力只能缩小 | TUI 用与 daemon 相同的 host/project 最小模式显示 effective mode；每个 Turn 发送显式 narrowing override；runner 仍在 policy 中重新求四层交集。 |
| 副作用必须审批 | wire fixture 观察真实 `approval/request`；runner fixture 统计 5 次非只读调用审批。read 自动批准仍只适用于 proven read-only EffectSet。 |
| 审批不能代替 trust/sandbox | wire fixture 先执行 identity-bound `workspace/trust`；sandbox capability 不足时 initialize 给出原因且 runner 不广告/不启动对应工具。 |
| patch 绑定对象与 revision | 两次 patch 分别绑定 `41\n` 和 `forty-two\n` 的 BLAKE3 revision；已有 stale revision、same-bytes/new-inode、symlink/hardlink 和 root identity 负向测试继续覆盖。 |
| Started 后不确定不能伪装 Failed/Cancelled | worker panic、cancel settlement、restart recovery 与 reconciliation 测试继续要求 `Unknown`，且不自动重放 provider/tool。 |
| 账本是事实源 | ToolCall、ToolResult、Invocation 状态、公开计划、退出码、diff、assistant Item 和 Turn 终态均从 projection 断言。 |

## 3. 正向与负向验收矩阵

| 场景 | 证据 | 当前结论 |
|---|---|---|
| read → plan → bad patch → failed check → repair → passing check → final diff | `runner::tests::real_repository_read_patch_test_fix_loop_is_durable` | Linux strict sandbox capability-gated；macOS 明确跳过 process 段。 |
| client → daemon → read → patch → wire approval → terminal evidence | `server::tests::protected_mutation_loop_requires_wire_approval_and_replays_terminal_evidence` | 当前主机真实 Seatbelt 路径通过；workspace 必须先 identity-bound trust。 |
| sandbox 不可用 | `pipeline::tests::mutation_cannot_start_when_sandbox_is_unavailable` | 执行边界前失败关闭。 |
| stale revision / inode / path redirect | runtime workspace、workspace-tools、pipeline 与 tools 测试组 | 不覆盖并发用户内容；同字节新 inode 也拒绝。 |
| 审批被拒、过期、重放或篡改 | core semantics、pipeline token/approval 测试组 | Invocation 绑定、一次性 token 与 TTL 均失败关闭。 |
| worker panic / cancellation crossing Started | runner Unknown 与 settlement 测试组 | 结果不确定时保留 Unknown，并阻断安全重试。 |
| daemon restart | `restart_fails_orphaned_turn_and_allows_a_new_turn`、`restart_recovers_invocations_before_turn_without_replaying_started_work` | 不自动重放外部工作；Turn/Invocation 获得可解释终态。 |
| evidence-only reconciliation | `invocation_reconcile_is_evidence_only_idempotent_and_unblocks_thread` | 只追加 operator evidence，不调用原 provider/tool。 |
| TUI 命令、EOF、宽字符、审批抢占 | `commands.test.ts`、`prompter.test.ts`、`renderer.test.ts` | 交互入口可控制、可降级且 deny-default。 |

## 4. 验证记录

最终门禁使用仓库锁文件；需要 loopback 或真实 Seatbelt 的测试在获准的主机权限下运行，以免把宿主沙箱的 `Operation not permitted` 误报成产品失败。

| 命令 | 结果 |
|---|---|
| `cargo fmt --all --check` | 通过 |
| `cargo check --workspace --all-targets` | 通过 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通过 |
| `cargo run --locked -p yeux-protocol --example export_schemas -- --check` | 通过；提交 schema 与 Rust source 一致 |
| `cargo test --locked --workspace --all-targets --no-fail-fast` | 主机权限复验通过：273 项 Rust 测试（2 core、20 semantics、7 wire、154 runtime、89 yeuxd、1 golden trace） |
| macOS launcher environment 定向复验 | release gate 暴露 2 秒 handshake 调度抖动后，将 fail-closed 上限调整为 5 秒；目标测试连续 10 次通过，完整 Rust 套件随后再次通过 |
| `pnpm typecheck` | 通过 |
| `pnpm test` | 通过：92 项 TypeScript 测试（9 protocol、4 plugin-host、79 TUI） |
| `pnpm build` | 通过 |
| `git diff --check` | 通过 |

## 5. 明确保留的 residual

这些项目仍是后续 M2.5/M3 或 release gate，不能从本轮结果推断已经完成：

1. **任务规模**：只有一个最小 Git fixture；计划目标仍是 10 个跨语言真实任务，并以至少 8 个完整成功作为 gate。
2. **Git authority**：本轮 diff 由受保护 `process.run` 调用绝对路径 Git 取得。生产 registry 还没有专用 read-only `git.status`/`git.diff`、checkpoint、revert 或 worktree adapter。
3. **artifact 数据面**：artifact store 原语存在，但 stdout/stderr 大输出自动 spill、敏感数据跨 chunk 删改、引用、配额与 GC 尚未接入 runner。
4. **平台进程治理**：Linux fixture 只在 bubblewrap/PID namespace 能力通过时执行；macOS 任意进程继续关闭，直到有可证明的 descendant supervisor。
5. **文件最终名称 CAS**：dirfd/no-follow/identity/revision 已关闭大部分重定向窗口，但 POSIX `renameat` 仍不能对最终名称执行 inode/hash 条件发布。
6. **context 与 compaction**：`/context` 是有界 ledger 视图，`/plan` 是非持久本地 scratchpad；`thread/compact` 仍返回 feature unavailable，没有 checkpoint summary、token meter、FTS 或项目规则层。
7. **TUI 产品层**：控制命令已经可用，但仍是 readline 行式界面；没有 alternate screen、viewport/focus、鼠标、resize、搜索、palette 或 hunk 导航。交互与 JSONL 也没有完成全状态 parity gate。
8. **provider 与发行**：仍只有 OpenAI-compatible adapter 和手工配置；keychain/OAuth、多 provider、安装器、签名、SBOM、升级/回滚和常驻服务尚未完成。
9. **协议生成**：TypeScript 命令面本轮人工同步，尚未由 Rust schema 自动生成并在 CI 中强制完整漂移门禁。

因此，本轮把 YeuX 从“只有安全原语”推进到“有一条受保护、可审查、可恢复的最小闭环和诚实控制面”，但尚未达到成熟 coding harness 的默认成功率、Git 工作流、长上下文、全屏交互与发行体验。
