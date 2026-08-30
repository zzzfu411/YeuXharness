# YeuX Harness — MEDIUM+ Findings Detail

审计日期：2026-08-30  
本文件只展开最终确认的 MEDIUM 及以上问题。LOW/加固项见 `REPORT.md`。

## YX-2026-001：交互 TUI 终端控制序列注入

### 完整数据流

1. **不可信 provider 输入**  
   `docs/audits/2026-08-30-run-1/evidence/provider.pre-fix.rs:394-429` 的 `parse_chunk` 从 SSE JSON 的 `/choices/0/delta/content` 读取任意字符串，创建 `ModelEvent::TextDelta`。这里不应清理协议内容；该值需要原样进入 ledger/JSONL。
2. **持久化与广播**  
   `crates/yeuxd/src/runner.rs:682-720` 的 `PersistingModelSink::emit` 把 event 包装成 `Event::ModelStreamEvent`，`crates/yeuxd/src/runner.rs:459-484` 的 `persist_locked` 在写入 ledger 后广播同一 envelope。
3. **发送给订阅客户端**  
   `crates/yeuxd/src/server.rs:405-433` 的 `serve_connection` 将匹配 Thread subscription 的 envelope 写成 `event` notification。
4. **TUI 投影**  
   `packages/tui/src/app.ts:109-126` 注册 event handler，并把 event 交给 `EventRenderer.render`。
5. **基线危险 sink**  
   `docs/audits/2026-08-30-run-1/evidence/renderer.pre-fix.ts:28-37` 取出 model delta 后执行 `this.#write(text)`，没有 neutralize ESC、CSI、OSC、C0/C1 或 bidi controls。`docs/audits/2026-08-30-run-1/evidence/renderer.pre-fix.ts:50-71` 和 `docs/audits/2026-08-30-run-1/evidence/prompter.pre-fix.ts:29-45` 对 diagnostic、approval explanation、effects、user prompt 也存在同类 raw terminal sink。

### 触发 payload

OpenAI-compatible SSE 响应中的一个最小恶意 delta：

```text
data: {"choices":[{"delta":{"content":"\u001b[2J\u001b]0;trusted-looking title\u0007fake approval: allow?"}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

```

触发步骤：

1. 受害者的 YeuX daemon 配置到攻击者可影响的 endpoint；一种具体条件是远程 `http://` provider 流量可被在途修改。
2. 受害者在交互模式发起 Turn。
3. endpoint 返回上面的 SSE delta。
4. 基线 daemon 将字符串作为正常 `model/event` 持久化并广播。
5. 基线 TUI 把 ESC/OSC 字节直接写入真实终端。

### 攻击者获得什么

攻击者可以修改终端显示状态、清除或隐藏已有输出、改变终端标题，并伪造看似来自 YeuX 的提示。没有证据表明该路径单独产生任意代码执行；某些终端的额外 OSC 能力取决于本地配置，不作为本 finding 的必要影响。

### 同类基线

本地编码智能体通常必须在“协议/ledger 原值”和“人类终端渲染值”之间区分边界：前者保真，后者 neutralize 控制序列。当前修复采用这一模式，在 terminal sink 清理、在 JSONL 保留原始值。

## YX-2026-002：可预测 Unix socket 的跨 UID endpoint 冒充

### 完整数据流

1. **可预测共享路径**  
   `docs/audits/2026-08-30-run-1/evidence/transport.pre-fix.ts:23-31` 在没有 `YEUX_SOCKET`/`XDG_RUNTIME_DIR` 时返回 `/tmp/yeux-<uid>.sock`。
2. **存在即连接**  
   `docs/audits/2026-08-30-run-1/evidence/transport.pre-fix.ts:33-47` 看到该路径存在后优先调用 `connectSocket`，而不是启动私有 stdio daemon。
3. **未认证 endpoint**  
   `docs/audits/2026-08-30-run-1/evidence/transport.pre-fix.ts:50-65` 直接 `createConnection(path)` 并构造 `JsonRpcClient`；没有 parent/socket 的 owner、mode、type、symlink、device/inode 或 peer credential 检查。
4. **发送 cwd**  
   `packages/tui/src/app.ts:167-195` 的 `openThread` 调用 `workspace/open`，把 `options.cwd` 发送给当前连接对端。
5. **发送 prompt**  
   `packages/tui/src/app.ts:221-247` 的 `runTurn` 调用 `turn/start`，把用户 prompt 放入 `content` 并发送给同一对端。
6. **接受伪造交互请求**  
   `packages/tui/src/app.ts:136-147` 为连接对端注册 `approval/request` 和 `user/input` handler；`packages/protocol/src/json-rpc-client.ts:275-294` 按 method/id 分派服务端请求。

### 触发 payload / 操作

攻击者的关键输入是 Unix socket 路径：

```text
/tmp/yeux-<victim-uid>.sock
```

线性触发步骤：

1. 攻击者是共享 Unix 主机上的另一普通账号，确认受害者 UID；受害者的 `YEUX_SOCKET`/`XDG_RUNTIME_DIR` 为空，且 `os.tmpdir()` 是共享可写目录，或受害者显式选择了等价的不安全路径。
2. 在受害者启动 TUI 前，攻击者在上述路径 bind/listen，并设置允许受害者连接的 socket 权限。
3. TUI 发现路径存在并连接 rogue server。
4. rogue server 按请求 id 返回形状正确的 `initialize`、`workspace/open`、`thread/start` 和 `thread/subscribe` 响应，使客户端继续。
5. 当受害者新建 Thread 并提交 prompt 时，rogue server 记录 `workspace/open.params.path` 与 `turn/start.params.content`；`--thread` 恢复路径不会发送 cwd。
6. 如需展示交互欺骗，rogue server 在非 JSONL 模式发送带 id 和完整合法 `params` 的请求，例如：`{"jsonrpc":"2.0","id":"evil-input","method":"user/input","params":{"threadId":"00000000-0000-4000-8000-000000000001","turnId":"00000000-0000-4000-8000-000000000002","prompt":"Re-enter a secret","metadata":{}}}`。`approval/request` 应使用与协议 PreparedInvocation 一致的 snake_case 字段，包括合法 UUID、agent/workspace/thread/turn 标识、参数和 effect 摘要、prepared token、RFC3339 时间戳，以及含 `idempotency`/`reversibility` 的完整 `effects`。只有用户实际作答时，rogue peer 才获得回答。

### 攻击者获得什么

- 自动获得受害者当前 workspace 路径和提交给 YeuX 的 prompt。
- 可以伪造 daemon event、诊断、审批说明和用户输入问题。
- 可以获取用户对伪造 `user/input`/`approval/request` 的回答。
- 不能仅凭该 UI 回答让合法 daemon 执行一个已批准操作；rogue server 与合法 daemon 是不同进程。

### 完整条件

- Unix-like 共享主机和另一 non-root 本机账号。
- 默认路径场景中 `YEUX_SOCKET`/`XDG_RUNTIME_DIR` 为空且 `os.tmpdir()` 共享可写，或用户显式选择共享目录中的不安全 socket。
- 攻击者在合法 daemon/client 之前占用路径。
- 攻击者创建的 socket 允许受害者连接。
- 同时获取 cwd 和 prompt 要求受害者新建 Thread 并提交 prompt；获取人工回答要求非 JSONL 模式和用户实际作答。
- root 与同 UID 对手属于不同信任边界，不是本 finding 必需条件。

### 同类基线

本地单用户 daemon 不需要实现多租户身份系统，但应依赖 OS 的 UID 隔离：socket 位于当前 UID 独占的 runtime 目录，节点为 owner-only，并在连接时验证文件系统身份或 peer credentials。当前修复使用私有父目录、owner/mode/type 和 pre/post device-inode 检查；长期可再叠加 peer credential 或会话令牌。
