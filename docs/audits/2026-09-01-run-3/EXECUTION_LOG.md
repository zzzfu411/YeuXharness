# Run 3 完善计划执行记录

日期：2026-09-01（Asia/Shanghai）
执行对象：当前工作树（审计基线 `05e02ea59f088e4f0731df3dcd94499509a64107` 之后的未提交改动）
关联计划：[`DEVELOPMENT_PLAN.md`](DEVELOPMENT_PLAN.md)

## 执行结论

本轮已完成 P0 搜索止血、P1-A 的账本/恢复基础，以及 P1-B 的 sealed registry 只读迁移基础。当前 daemon 仍保持副作用能力关闭：`workspace.apply_patch`、`process.run`、网络、MCP、插件执行、Job 调度和子智能体均未向模型广告或分派。因此本记录不把 v0.1“读、改、测、修”宣布为完成；后续 P1-B 完整 capability/policy/permit、P1-D approval、P1-E dirfd/CAS、P1-F supervisor/sandbox 和 P1-G E2E 仍是发布阻断项。

## 已执行工作

| 计划项 | 状态 | 实施与证据 |
|---|---|---|
| P0-01 搜索算法 | 已完成 | `crates/yeux-runtime/src/workspace_tools.rs` 使用 KMP，保留重叠匹配、稳定 offset/行列语义，并限制 prefix/scan 操作数；runner 将 matcher 计数汇聚到每 Turn 共享 operation budget，硬上限由 32 MiB 扫描上限推导。 |
| P0-02 取消、deadline、CPU budget | 已完成（协作式） | `WorkspaceSearchControl` 在遍历、匹配和 dispatch 边界检查取消/截止/操作预算；稳定错误码为 `workspace_search_cancelled`、`workspace_search_deadline_exceeded`、`workspace_search_budget_exceeded`。同步 worker 无法被 Tokio 强杀时改报 `Unknown`，不伪造成功；真正的硬终止仍依赖后续隔离 supervisor。 |
| P0-03 并发止血 | 已完成（daemon + workspace 级） | `TurnRunner` 共享有界 `Semaphore`（默认 4）；`workspace.search` 以 `(canonical_root, identity_digest)` 为 key 使用单槽 gate，Weak map 会清理已退出 worker 且有 256 项上限；闸门在 worker 启动前 fail closed，turn 仍有累计调用/结果预算及共享 search operation budget。 |
| P0-04 timeout 语义 | 部分完成 | `ToolSpec.timeout_ms` 驱动等待截止；超时后向 worker 发协作取消并给出有限 grace，未能证明停止时持久化 `Started -> Unknown`。真正可终止的隔离进程/硬截止留给 P1-F。 |
| P0-05 adversarial regression | 已完成（本机） | 长共同前缀、重叠匹配、取消、deadline、操作预算、并发/结果预算测试已加入 Rust 测试；双平台 CI 仍需复跑。 |
| P0-06 基线冻结 | 已完成 | 本文件、README、ROADMAP、ARCHITECTURE、PROTOCOL 和 THREAT_MODEL 同步当前只读边界、错误码、能力广告及残余限制。 |
| P1-A atomic outcome | 已完成（只读路径） | `EventLedger::append_invocation_outcome` 在同一 SQLite 事务写 ToolResult + terminal state，并做 scope/causation/invocation/polarity/state precondition 校验；runner 的正常结果、准备失败、调度失败、结果预算早退和重启前置恢复均使用该批次；重复批次按 event id 幂等，部分批次拒绝。 |
| P1-A Unknown/recovery | 已完成（基础） | 新增 `append_invocation_unknown`、`append_invocation_unknown_outcome`、显式 `tool/reconciled` 校验和启动恢复扫描；Started 不自动重试，runner 在重启/取消/超时无法证明外部结果时以同事务 Unknown+有界诊断收束。启动恢复对 Started 保留 marker-only（没有可证明结果），而对 Proposed/Approved/Prepared 使用原子 ToolResult+Failed。取消若伴随 Unknown 不再把 Turn 伪装成 clean `Cancelled`，而是以 reconciliation-required 失败诊断收束。 |
| P1-A context/identity | 已完成（基础） | runner 增加消息、block、字节、粗略 token、轮次/调用/结果硬上限；workspace 保存 canonical root/device/inode/digest，并在打开、路径操作、读和发布边界复核。 |
| P1-B sealed registry | 已完成（只读基础） | `crates/yeuxd/src/tools.rs` 提供 exact id/version、schema/参数限制、effect template subset、plan/revalidate、单次 opaque permit；新增 bound executor，把 proposal 的 workspace identity、规范化参数和 concrete effects 与执行前重新验证的值逐项比较；runner 的 `list/read/search` 已迁移到 registry。apply_patch 仅可 hidden 注册，未广告。 |
| P1-C 只读迁移 | 已完成（核心路径） | provider tool specs 从 registry 单源生成；未知/未协商工具仅形成有界错误结果；结果按模型调用顺序入账，实际 read effect 持久化。 |

本轮收尾还修正了 interrupt 的状态竞态：正常执行模式下控制面先持久化
`Cancelling`，由 runner 在所有 worker 收束后依据可证明的停止结果选择
`Cancelled` 或 reconciliation-required `Failed`；重复 interrupt 在已处于
`Cancelling` 时只记录幂等 receipt，不再提交空事件批次。未知工具结果不会再
进入下一轮 provider 请求。

收尾阶段又补上了四个 fail-closed 边界：worker `JoinError`/panic/abort 不再被
当作普通失败，而是以 `Started -> Unknown` 和有界错误结果记录；
`Prepared -> Started` 只有在父 turn 仍为 `Executing` 时才可提交；父 turn 的
`fail_current` 在仍有未收束 invocation 时拒绝终态化；typed invocation outcome
在同一事务内复核 persisted proposal 的工具/版本、规范化参数摘要、effect
digest、idempotency、thread/turn/agent scope 与 `call_id`。这些检查不改变通用
raw append/import 的 append-only 性质，但会阻止不可信历史继续被 typed outcome
或 recovery 扩展。

## 尚未执行/仍为发布阻断

1. P1-B 完整 capability intersection、用户/项目/turn grant 接线和不可旁路的通用 `InvocationPipeline` 尚未成为所有工具的 authority；`grants.rs` 目前保留为待接线原语。
2. P1-D approval/interaction broker（pending map、oneshot response、断线/超时、TUI/JSONL parity）尚未实现。
3. P1-E `apply_patch` 仍需要 root dirfd、逐组件 `openat`/`openat2` 等价能力和严格 CAS；当前 live identity/revision 检查不能封闭最后的 path rename 竞态，隐藏 mutation adapter 也尚未把 `FileRevisionSnapshot` 从 prepare 传到 execute，故同字节新 inode 的 prepare→execute 竞态仍是开放写入前的阻断项。
4. P1-F process supervisor、进程树证明、sandbox ready/go、CredentialBroker 和网络代理尚未接入 daemon；不能开放测试进程或网络。
5. P1-G 真实 `read → apply_patch → test → fix`、故障注入、跨平台 E2E、依赖/许可证/SBOM、签名、可复现构建和 soak 尚未通过。

## 验证记录

以下命令在当前工作树执行；loopback provider 测试在允许本机 localhost bind 的环境重跑。

| 门禁 | 结果 |
|---|---|
| `cargo fmt --all --check` | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS（0 warning） |
| `cargo run -p yeux-protocol --example export_schemas -- --check` | PASS |
| `cargo test --workspace --all-targets --no-fail-fast` | PASS（211/211：core 22、protocol 6、runtime 118、yeuxd 64、golden 1）；provider httpmock 的 3 个 loopback 用例已在允许绑定 `127.0.0.1:0` 的本机环境复跑通过。workspace_tools 36/36、yeuxd runner 19/19 与 `cargo check --all-targets` 同样通过；仍需在 macOS/Linux CI 双平台复跑 |
| `pnpm typecheck` / `pnpm build`（pnpm 9.15.9） | PASS |
| `pnpm test` / TypeScript Vitest | PASS（9 files / 51 tests） |
| 搜索最坏复杂度独立 harness | 修复前复现；修复后由 KMP/预算回归覆盖 |
| `git diff --check` | PASS |

本轮 `pnpm` 使用仓库声明的 9.15.9 并在现有依赖上直接完成 typecheck/test/build；此前受限环境中若依赖未安装，安装阶段可能需要网络和 `esbuild` 生命周期脚本，未改变锁文件或依赖版本。干净环境仍应按 CI 的锁文件安装流程复跑。

## 回滚与运行策略

- `YEUX_ENABLE_MUTATIONS=0`、`YEUX_ENABLE_PROCESS=0`、`YEUX_ENABLE_PLUGINS=0` 继续保持默认关闭；当前 runner 仅构造三项只读注册。
- 若出现越权、stale 覆盖、Unknown 被重试、replay 外部调用或搜索资源回归，应关闭新增注册并回退到只读 registry；保留 ledger/artifact 证据，不删除或重写历史事件。
- 开放任一副作用工具前，必须完成上面的 P1-B/D/E/F/G 阻断并重新进行独立安全审计。
