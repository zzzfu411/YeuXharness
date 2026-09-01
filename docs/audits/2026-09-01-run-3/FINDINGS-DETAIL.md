# YeuX Harness Findings Detail（Run 3）

审计日期：2026-09-01
本轮仅有一个 confirmed finding。它按“单用户本地 daemon、恶意仓库/模型输出在威胁模型内”的口径评级为 LOW；若未来一个 daemon 服务多个互不信任用户，或把本机可用性列为更高保障目标，应重新评估为 MEDIUM。

> 注：本文件保留修复前的可复现链路与行号，作为审计证据归档；当前实现
> 的修复、验证结果和未闭环风险请以 [`EXECUTION_LOG.md`](EXECUTION_LOG.md)
> 为准。

## YX-2026-003：`workspace.search` 朴素逐偏移匹配导致 CPU 拒绝服务

### 结论

`workspace.search` 对每个文件的每个可能偏移执行一次完整的 `needle` 切片比较。查询长度和扫描字节虽有上限，但没有线性匹配算法、CPU 操作预算或可中断的硬截止。构造“几乎完全相同、只在最后一个字节不匹配”的查询即可得到近似 `O((N-M+1)×M)` 的工作量；同一模型响应最多可并发启动 32 个调用（每个 turn 的累计上限），并使用 blocking worker。

这不是以恶意 provider 为前提：恶意仓库内容可以通过 prompt injection 诱导正常 provider 返回该 tool call；`docs/THREAT_MODEL.md` 明确将模型输出、仓库内容和资源耗尽纳入范围，同时把用户主动配置的恶意 provider 排除在外。

### 精确数据流（entrypoint → sink）

1. **Entrypoint — provider tool call 被接受**
   `crates/yeuxd/src/runner.rs:282-301` 根据 provider capability 建立 `workspace.list/read/search`；`runner.rs:447-463` 接收模型组装的调用并按每个 turn 的累计预算放行，最多 32 个调用（同一模型响应可并发启动，后续 model round 不重置）。
2. **Propagation — blocking worker**
   `crates/yeuxd/src/runner.rs:518-536` 为每个已准备调用执行 `tokio::task::spawn_blocking(move || tools.execute(...))`。`ToolSpec.timeout_ms` 虽在 `workspace_tools.rs:1137` 声明为 5 秒，但 runner 没有用 `tokio::time::timeout` 或等价的 CPU 截止包装。
3. **Propagation — bounded scan but unbounded per-byte cost**
   `crates/yeux-runtime/src/workspace_tools.rs:558-587` 逐文件读取并限制单文件 1 MiB、累计扫描 32 MiB、文件数/深度/匹配数；随后把原始字节和 query 传给 matcher。
4. **Sink — quadratic-ish matcher**
   `crates/yeux-runtime/src/workspace_tools.rs:900-929` 在 `0..=haystack.len()-needle.len()` 的每个 offset 执行 `&haystack[offset..offset + needle.len()] == needle`。对共同前缀很长而最终不匹配的输入，每个 offset 都会重复比较几乎整个 query。

### 可复现输入

工作区准备 32 个普通文件，每个文件恰好 1,048,576 字节，内容为 ASCII `a`。向 provider 返回合法的 tool call：

```json
{
  "name": "workspace.search",
  "arguments": {
    "path": "",
    "query": "aaaa...aaab"
  }
}
```

其中 `query` 是 4,096 字节：`"a"` 重复 4,095 次，最后一个字节为 `"b"`。该值通过 4 KiB query 上限，且没有任何匹配，因此不会触发 `max_matches` 提前退出。

对单个 1 MiB 文件，候选 offset 约为 1,044,481；每次相等比较约比较 4,095 个 `a` 后才失败，约产生 `4.28×10^9` 字节比较。独立 aarch64 release harness（抽取等价循环，未修改仓库源代码）观察到：

| 场景 | 观测 |
|---|---:|
| 1 MiB haystack / 4,096-byte query | 约 110 ms |
| 32 MiB haystack / 4,096-byte query | 约 2.66 s |
| 8 个并发、各 32 MiB | 约 3.9 s |
| 32 个并发、各 32 MiB（模拟一轮 32 calls） | 约 14.36 s |

测量用于证明复杂度和可达性，不把具体毫秒数当作所有机器的 SLA。单个 worker 同时只保留约 1 MiB 文件缓冲；主要影响是 CPU 饱和，而非一次性分配 1 GiB 内存。

### 线性攻击步骤

1. 攻击者将含有重复字符的大量普通文本文件的仓库交给用户，或在仓库文档/注释中放入提示，诱导模型调用 `workspace.search`。
2. 用户用配置了 tool calls 的正常 provider 启动一次 turn；不需要攻击者控制 provider。
3. 模型输出一个或多个上述 `workspace.search` 调用。当前 runner 将只读调用直接交给 `WorkspaceTools`；同一模型响应可并发启动，且每个 turn 累计最多 32 个调用。
4. daemon 读取文件并在 blocking pool 中反复执行朴素比较。用户按下 interrupt 时，runner 只跳过尚未回收的 handle 并标记状态；已经开始的同步 worker 仍继续消耗 CPU，直到自然结束。
5. 观察到 daemon 响应变慢、后续 turn/订阅延迟和本机 CPU 长时间饱和；在 32-call 场景下可持续十几秒，重复 turn 可继续放大。

### 攻击者获得的影响

- 夺取当前用户 daemon 的 CPU 时间，造成交互卡顿、订阅延迟和 turn 超时；
- 令取消操作不能立即止血，增加恢复时间；
- 在未来多会话/后台 Job 复用 daemon 的部署中，可能扩大为跨任务可用性影响。

没有证据表明该路径读出 workspace 外文件、执行 shell、写文件或绕过 capability。它是算法复杂度型 availability finding，而非权限提升。

### 修复建议与验收

**短期止血（发布前）**

- 将 matcher 替换为线性/子线性实现（例如 `memchr::memmem`、Two-Way 或 Aho–Corasick），并保留现有路径/输出预算。
- 在扫描循环中加入 cooperative deadline/cancellation 检查；超时返回稳定错误码，不把“已取消但 worker 仍在跑”伪装为完成。
- 在 daemon 侧增加 per-workspace/per-daemon blocking semaphore 和每 turn 的 CPU/搜索预算，避免 32 个昂贵调用同时占满 Tokio blocking pool。
- 让 `ToolSpec.timeout_ms` 真正驱动调度器；注意 `tokio::time::timeout` 只能终止等待，不能终止已经运行的同步代码，硬截止应放入可终止的隔离 worker 或受控进程。

**建议代码区域**

| 文件 | 变更 |
|---|---|
| `crates/yeux-runtime/src/workspace_tools.rs` | 替换 `collect_literal_matches`；增加操作计数、deadline/cancellation 注入点和最坏输入测试 |
| `crates/yeuxd/src/runner.rs` | 统一执行调度、并发 semaphore、实际 timeout、worker 状态与取消/Unknown 语义 |
| `crates/yeux-protocol` / `spec/schema` | 如新增错误码或预算字段，更新 schema 与 drift tests |
| `crates/yeuxd/tests`、`crates/yeux-runtime` tests | 32×1 MiB、长共同前缀、并发、取消和超时回归 |

**验收标准**

1. 对 query 长度 ≤ 4 KiB、扫描量 ≤ 32 MiB，运行时间随扫描字节近似线性，长共同前缀不再乘上 query 长度。
2. 预算耗尽在有界时间内返回稳定 `workspace_search_budget_exceeded`/等价错误；取消会协作通知 worker 并在无法证明停止时记录 `Unknown`，不把 `spawn_blocking` 误报为已终止；硬终止留给隔离 supervisor。
3. 一轮 32 个恶意调用不会耗尽 daemon 全局 worker；正常 read/list/search 的顺序、结果和 ledger replay 保持兼容。
4. 动态 adversarial test 在 CI 的 macOS/Linux 两个平台均通过，且不依赖固定毫秒阈值。

## 已独立验证但不计为 confirmed finding

| 候选 | 判定 | 原因与必须修复的后续 |
|---|---|---|
| Turn interrupt 与 runner 的竞态 | REJECTED | `server.rs:521-529` 与 `open_with` 传入的同一 `command_gate` 串行化正常 RPC；竞态假设忽略了这把锁。仍需补充 crash/取消状态测试。 |
| workspace 根目录替换 | REJECTED（当前部署假设；P1 blocker） | `commands.rs:144-161` 持久化 identity，但 `runner.rs:282-301,1039-1050` 只用 root 字符串重新打开；拥有 workspace 父目录写权限的同 UID/共享可写 actor 可读到替换目录。需做 P1 live-identity revalidate；若产品把该 actor 纳入威胁模型，应升级为条件性 finding。 |
| 中间目录 symlink TOCTOU | REJECTED（当前部署假设；P1 blocker） | `workspace.rs:369-379` canonicalize 后，`469-478` 只对最终组件使用 `O_NOFOLLOW`；拥有中间目录写权限的 actor 可在两步之间替换 symlink。需用 dirfd/openat2 等修复；若该 actor 在威胁模型内，应升级为条件性 finding。 |
| TurnStart 伪造 ToolCall/ToolResult/Reasoning 或 capability override | REJECTED | `commands.rs:516-525` 原样入账，但 `runner.rs:1133-1156` 固定按 ItemKind 映射，override 不进入 policy；provider 最坏收到 role=user 的不兼容字段，不能触发 runner 执行器。应增加 schema/上下文总量上限。 |
| provider SSE `drain` 重复扫描 | REJECTED（恶意 provider 不在 v1 范围） | `provider.rs:648-691` 仍可做线性扫描优化，但攻击者需控制用户主动配置的 provider；当前已有 8 MiB buffer/64 MiB stream 等预算。 |
