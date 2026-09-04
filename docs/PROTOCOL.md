# YeuX Protocol 2.0

状态：开发基线  
规范源：`crates/yeux-protocol`  
当前传输：stdio、Unix socket

YeuX Protocol 是 `yeux` 客户端、`yeuxd` 和后续受限扩展之间的公共边界。协议采用 JSON-RPC 2.0，但为可重连命令、事件排序和 replay 增加了明确约束。

本文区分“协议已经声明”和“daemon 已经执行”。声明某个方法或类型不代表其业务能力已经完成。

## 1. 传输与握手

stdio 和 Unix socket 使用相同 framing：每行一个完整 UTF-8 JSON 对象，以 `\n` 结束。当前 daemon 将单行上限设为 8 MiB。

客户端建立连接后，第一条命令必须是 `initialize`：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "command_id": "0195f6cc-0a3a-7a4d-8c9e-123456789abc",
  "method": "initialize",
  "params": {
    "protocolVersion": { "major": 2, "minor": 0 },
    "clientInfo": { "name": "yeux", "version": "0.1.0-alpha.1" },
    "capabilities": {
      "event_replay": true,
      "server_requests": true,
      "rich_content": false
    }
  }
}
```

成功响应：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": { "major": 2, "minor": 0 },
    "serverInfo": { "name": "yeuxd", "version": "0.1.0-alpha.1" },
    "capabilities": {
      "unix_socket": true,
      "jobs": false,
      "subagents": false,
      "plugins": false
    },
    "hostCeiling": "operate"
  }
}
```

能力标志只表示当前 daemon 已接通并可安全执行的闭环。当前 `jobs`、`plugins` 和 `subagents` 均为 `false`：Job 规格仍可由兼容客户端读取/管理，但后台调度、插件 authority 和子智能体执行尚未开放；`job/run` 返回 `feature unavailable`。

副作用工具不会仅因协议类型存在就自动开放：`workspace.apply_patch` 与 `process.run`
只在 host ceiling、workspace trust 和已探测的 sandbox 能力满足要求时出现在 provider
ToolSpec 列表中；每次 process spawn 还必须通过一次有界 launcher handshake。Unix patch 的 descriptor-relative publication 关闭路径重定向，但
POSIX 最终名称没有 inode/hash 条件 `rename`；该残余由 invocation revision evidence
和 reconciliation 语义承接。macOS Seatbelt 不声明 arbitrary process isolation，因而
`process.run` 在该平台保持隐藏。

## 2. 命令包络

每个客户端请求使用以下包络：

```json
{
  "jsonrpc": "2.0",
  "id": "client-local-request-id",
  "command_id": "UUIDv7",
  "method": "thread/read",
  "params": {}
}
```

- JSON-RPC `id` 只关联当前连接中的请求和响应，可以是字符串或整数。
- `command_id` 必须是 UUIDv7，用于跨重试识别逻辑命令。
- 相同 `command_id` 只能与完全相同的 method 和规范参数一起重用。
- 同一 ID 被不同输入重用时返回 command conflict。

当前 daemon 只使用 SQLite durable command receipt，不维护无界的进程内回执缓存。产生事件的成功命令会在同一事务中提交事件与响应；不产生事件的成功命令也会在向客户端确认前保存响应。重启后，相同 `command_id` 与相同输入返回原响应且不追加事件；不同输入复用该 ID 会被拒绝。连接局部的 `thread/subscribe` 是唯一例外：相同命令重试时会校验原 receipt，但按当前 ledger 水位重新创建 subscription ID 与补发窗口。失败响应目前不作为成功 receipt 保存。该机制防止已提交命令被重复分派，但未来外部工具的 exactly-once 语义仍取决于 invocation 状态机、幂等键和 reconciliation。

## 3. 版本兼容

当前稳定协议版本是 `2.0`。P1 为 invocation、approval 和 recovery 增加了
授权所需的必填证据，因此该 major 与缺少这些证据的 1.0 ledger/wire 不兼容：

- major 不同：`initialize` 失败，错误码 `-32001`。
- major 相同：服务端接受不高于自身支持范围的客户端 minor。
- 实验方法必须使用 `experimental/` 前缀，不进入稳定兼容承诺。
- 每个事件独立携带 `schema_version`，replay 再次验证兼容性。

Rust `stable_schema_bundle()` 从公共类型生成 JSON Schema map，当前 56 份稳定 schema 已提交到 `spec/schema/`。`export_schemas --check` 与协议测试执行字节级漂移检查。Rust wire 类型仍是规范源；从 schema 自动生成完整 TypeScript 类型尚未完成，`packages/protocol` 目前是客户端使用部分的镜像。

## 4. 领域标识与事件顺序

状态层级为：

```text
Workspace -> Thread -> Turn -> Item
```

Workspace、Thread、Turn、Item、Event 和 Command 的核心 ID 使用 UUIDv7。Agent ID 是稳定的人类可读名称，causation ID 是用于关联来源的不透明字符串。

每个 Thread 有自己的事件序列：

- 第一条事件 `seq = 1`；
- 后续事件必须严格 `+1`；
- 不承诺跨 Thread 全局序列；
- 子 Thread 通过 `parent_thread_id` 和 `parent_seq` 引用父历史点；
- `causation_id` 连接命令、模型调用、工具调用和由其产生的事件。

## 5. 事件通知

事件以 JSON-RPC notification 发送，method 固定为 `event`：

```json
{
  "jsonrpc": "2.0",
  "method": "event",
  "params": {
    "schema_version": { "major": 2, "minor": 0 },
    "event_id": "0195f6cd-11f0-75b2-a187-123456789abc",
    "thread_id": "0195f6cc-b894-7e62-9f10-123456789abc",
    "turn_id": "0195f6cd-0211-7c91-850f-123456789abc",
    "agent_id": "root",
    "seq": 3,
    "time": "2026-08-30T10:00:00Z",
    "causation_id": "0195f6cc-0a3a-7a4d-8c9e-123456789abc",
    "kind": "item/added",
    "payload": {
      "item": {}
    }
  }
}
```

稳定事件族当前包括：

- `workspace/opened`、`workspace/trust_changed`
- `thread/started`、`thread/forked`、`thread/archived`
- `turn/started`、`turn/state_changed`、`turn/steered`
- `item/added`
- `model/requested`、`model/event`
- `tool/proposed`、`tool/state_changed`、`tool/reconciled`
- `job/created`、`job/state_changed`
- `agent/spawned`、`agent/completed`
- `runtime/diagnostic`

客户端不能用 wall-clock 时间排序同一 Thread，必须使用 `seq`。终态、工具参数摘要、工具版本、effect digest、审批决定和副作用结果不能因流式压缩而丢弃。

## 6. 补发、订阅与背压

历史读取和实时订阅是分离但连续的：

1. 客户端保存每个 Thread 最后应用的 `seq`。
2. 重连后发送 `thread/subscribe { threadId, afterSeq }`。
3. daemon 在接受订阅时固定当前 Thread 水位，先返回含 `replayedThroughSeq` 的订阅结果。
4. daemon 再按内部页补发 `afterSeq < seq <= replayedThroughSeq` 的全部事件，然后从 `replayedThroughSeq + 1` 发送实时 `event` 通知。

如果客户端落后于广播缓冲，daemon 发送 `runtime/diagnostic`，code 为 `event_backpressure`，然后关闭连接。若实时事件出现序列缺口，则发送 `event_sequence_gap` 并要求客户端从最后已应用的 `seq` 重连。

`thread/read` 每页 `limit` 必须在 1–1000，默认 100，结果用 `nextAfterSeq` 表示还有后续；`thread/resume` 固定最多返回 1000 条事件，也可通过 `nextAfterSeq` 接续 `thread/read`。`thread/list` 的 `limit` 同样为 1–1000，用不透明 `nextCursor` 继续。只有 `thread/subscribe` 建立实时订阅。

## 7. 稳定方法与当前实现

| 方法 | 协议 | 当前 daemon 行为 |
|---|---|---|
| `initialize` | 稳定 | 已实现；必须为首条命令 |
| `workspace/open` | 稳定 | 已实现；规范化 root，初始为 untrusted |
| `workspace/trust` | 稳定 | 已实现；绑定 workspace identity digest |
| `workspace/status` | 稳定 | 已实现 |
| `thread/start` | 稳定 | 已实现 |
| `thread/resume` | 稳定 | 已实现；返回 Thread + 最多 1000 条事件及 `nextAfterSeq` |
| `thread/fork` | 稳定 | 已实现；记录父 Thread 和 seq，runner 按多级谱系继承各父分支点之前的消息 |
| `thread/read` | 稳定 | 已实现；`afterSeq` + `nextAfterSeq`，每页 1–1000 |
| `thread/list` | 稳定 | 已实现；`cursor` + `nextCursor`，每页 1–1000 |
| `thread/archive` | 稳定 | 已实现；active Turn 时拒绝 |
| `thread/compact` | 稳定声明 | 未实现，返回 feature unavailable |
| `thread/subscribe` | 稳定 | 已实现；先固定并补发到 `replayedThroughSeq`，再进入实时订阅 |
| `turn/start` | 稳定 | 创建 Turn 和用户 Item 后异步启动有界 runner；配置 provider 且协商 tool calls 时始终注册 `workspace.list/read/search`，并仅在 sandbox capability/host ceiling 允许时附加受保护的 `workspace.apply_patch`、`process.run`；持久化多轮模型流、ToolCall/ToolResult、Invocation 和 assistant Item；未配置时以 `provider_unconfigured` 失败；重启不重调 provider/tool，而是终结遗留 Turn 为 `failed` |
| `turn/steer` | 稳定 | 持久化 steering 事件；runner 在每次后续模型请求前重载 ledger，使消息在下一安全点进入当前 loop |
| `turn/interrupt` | 稳定 | 已连接 runner 取消标志并持久化 `cancelling -> cancelled`（未跨越未决执行边界时）；取消后的 provider delta 不再落账；已越过执行边界但无法证明结果的工具会记录 `Unknown`/诊断，Turn 以 reconciliation-required 失败收束，禁止伪报成功、clean `Cancelled` 或静默重试 |
| `invocation/reconcile` | 稳定 | 仅收束父 Turn 已终态且 invocation 为 `Unknown` 的调用；当前只接受 `operator_review` 有界证据，可验证 `artifact://` 引用；以幂等 ledger 事务写入 `tool/reconciled` 与 ToolResult，绝不重试原 provider/tool |
| `model/list` | 稳定 | 读取 provider descriptor，不自动发现模型 |
| `skill/list` | 稳定 | 读取 descriptor，不加载或执行 Skill |
| `mcp/status` | 稳定 | 读取 descriptor，不连接 MCP server |
| `plugin/list` | 稳定 | 读取 descriptor，不启动插件 |
| `job/create|list|pause|resume` | 稳定 | 管理规格和投影状态，不调度执行 |
| `job/run` | 稳定声明 | 未实现，返回 feature unavailable |
| `approval/request` | 服务端请求 | live connection 上由副作用 runner 发出；pending 请求、断线和 malformed response 均 deny-default；daemon 不接受客户端自带的 ApprovalBinding |
| `user/input` | 服务端请求 | 类型和 TS handler 已定义，daemon 尚不发出 |

## 8. 服务端请求

需要人类决策时，目标协议由 daemon 发起带 JSON-RPC `id` 的请求，而不是事件通知：

- `approval/request`：携带完整 PreparedInvocation、effect digest 和解释；响应为拒绝或精确 ApprovalBinding。连接级 daemon 会为需要审批的副作用调用发出该请求；断线、格式错误或未知响应 ID 均按 deny-default 处理。
- `user/input`：携带 Thread、Turn、问题和 metadata；响应为 content blocks。

无头 JSONL 客户端默认不能交互审批。后台 Job 没有预授权时必须进入 `waiting_for_approval`，不能将“无客户端响应”解释为允许。

## 9. Replay 语义

“Replay”只表示：

```text
persisted EventEnvelope[] -> validate -> deterministic Projection
```

Replay 不表示重新发送 prompt，不重新执行 tool，不重新访问网络，也不尝试让外部世界回到历史状态。`thread/fork` 引用父历史点并创建新的局部事件序列；它同样不会重演父 Thread 的副作用。

调用处于 `unknown` 且非幂等时，只能由 reconciliation 收集并记录外部状态证据后请求用户决策。当前 `invocation/reconcile` 是 operator-review/evidence-only 路径：它验证可选 artifact URI、写入 `tool/reconciled`，不读取或重试原工具；未来 machine receipt authority 仍需单独设计。任何恢复路径都不得自动将它重新排队。

## 10. 错误

除 JSON-RPC 标准错误外，当前 daemon 使用：

| code | 含义 |
|---:|---|
| `-32000` | 尚未 initialize |
| `-32001` | 协议主版本不兼容 |
| `-32002` | `command_id` 冲突或重复实体冲突 |
| `-32004` | 实体不存在 |
| `-32005` | 当前状态不允许该命令 |
| `-32006` | 协议已声明但功能尚未实现 |

错误 `data` 可以包含用于诊断的结构化 detail，但不得包含凭据、未删改工具输出或敏感环境变量。

## 11. 演进规则

- 新的兼容字段使用可选字段和安全默认值。
- 破坏性变更提升 major，并在初始化时明确拒绝旧客户端。
- 新实验方法进入 `experimental/`，成熟后以新的稳定方法发布，不悄悄改变原方法语义。
- Rust schema、生成的 TypeScript 类型、黄金 wire fixtures 和 daemon contract test 必须在同一变更中更新。
- TUI 和无头客户端必须对同一事件 trace 构建相同投影。
