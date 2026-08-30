# YeuX Harness 审计后执行计划

日期：2026-08-30  
范围：当前 M0/M1 工程基线，以及进入 M2 前不可绕过的安全边界

## 本轮完成状态

G0 已于 2026-08-30 完成：2 个 confirmed findings 与 2 个 release-hardening gaps 均已处理并有回归测试，基线文档与实现一致。最终验证结果为 Rust **共 100 项**、TypeScript 36 项全部通过；Rust 总数中包含 schema 定向 2 项和 golden trace 1 项。format、Clippy `-D warnings`、TypeScript typecheck/build 全绿。三个依赖 localhost bind 的 provider mock 用例在允许本地监听的测试环境中通过。

- [x] 私有每用户 Unix socket 与连接前后身份检查。
- [x] TUI 的人类终端 sink 控制字符清理，JSONL 保留原始协议值。
- [x] sandbox launcher 与目标环境分离。
- [x] provider 错误体、流、事件、输出和 tool-call 状态硬上限。

## 目标与优先级

### G0：关闭本次审计确认的问题与发布加固缺口

退出条件：2 个 confirmed findings 和 2 个 release-hardening gaps 均有回归测试；Rust 与 TypeScript 全量检查通过；报告、路线图和实现状态一致。

1. 将默认 Unix socket 放入当前 UID 独占的 `0700` 目录，daemon 和客户端同时校验目录/套接字类型、owner 与 mode。
2. TUI 交互终端中的模型、诊断、审批，以及经 TUI 呈现的 daemon 子进程错误输出必须经过控制字符清理；JSONL 保持机器可读原值。
3. `ProcessExecutor` 的调用方环境只能在 OS 沙箱建立后进入目标进程，不能影响 `sandbox-exec` 或 `bwrap` launcher。
4. Provider 对成功流总字节、事件数、累计输出和错误响应体设置硬上限，超限以明确的非重试错误终止 Turn。

### G1：完成 M0 契约门槛

退出条件：协议只有一个真源；相同固定输入产生字节级一致的 trace；每个崩溃窗口都有预期终态。

1. 从 Rust schema 自动生成完整 TypeScript 类型，删除手工镜像，并在 CI 中执行跨语言 drift 检查。
2. 将 faux clock、确定性 UUIDv7 生成器和 faux provider/tool 注入 daemon golden trace，固定事件 ID、时间、顺序和响应。
3. 增加平台能力矩阵：macOS Seatbelt、Linux bubblewrap、Landlock/seccomp 可用性、降级原因与失败关闭测试。
4. 为 `accepted`、`prepared`、`started`、副作用完成、ledger commit、artifact publish 等崩溃窗口建立表驱动测试。
5. 增加依赖、许可证和供应链门禁：RustSec、npm audit、license allowlist、锁文件漂移和生成物来源检查。

### G2：完成 M1 只读纵向闭环

退出条件：一条真实任务能完成 `yeux -> yeuxd -> provider -> read/list/search -> provider -> answer`，且 replay 不产生任何外部调用。

1. 定义并注册三个内置只读工具：`workspace.list`、`workspace.read`、`workspace.search`；参数、文件数、单文件字节、总扫描字节、深度和结果大小全部有界。
2. 将 Turn runner 改为有界 Agent loop：聚合碎片化 tool call、校验 JSON、按模型调用顺序持久化 proposed/result、并发仅限结构化只读调用。
3. 工具结果以 `ToolResult` 回灌 provider；设置最大 loop 次数、总工具调用数、总上下文字节和 token/cost 预算。
4. 接入 `CredentialBroker` 句柄，provider token 只在请求构建时短期解析，不进入事件、错误、普通环境或 Debug 输出。
5. `turn/steer` 只在明确安全点注入下一次模型请求；取消后 provider/tool 残余输出不再落账。
6. 对同一 golden trace 比较交互模式和 `--jsonl` 投影，验证补发、背压、断线恢复与第二 active Turn 拒绝。
7. 增加最小 FTS5 投影和容量上限；索引可重建，不能成为事实源。

### G3：进入 M2 前的安全集成门

退出条件：任何写文件、启动进程或联网的路径都无法绕过统一 pipeline。

1. 将 `validate -> prepare effects -> policy -> approval -> sandbox -> execute -> redact -> persist` 实现为唯一公开执行入口。
2. `PreparedInvocation` 使用不可伪造、短期、单次 token；审批重新校验 workspace identity、参数、effect、工具版本、agent、mode 和过期时间。
3. `workspace.apply_patch` 转为 dirfd-relative/no-follow 发布，补齐中间目录 TOCTOU、文件大小和 inode 复核。
4. 进程监督覆盖主动 `setsid`/`setpgid` 脱组；输出、文件描述符、子进程树和取消都有 soak test。
5. 工具网络统一走代理，阻断 loopback/private/link-local/metadata、代理环境绕过和 DNS rebinding；模型供应商网络单独治理。
6. plugin host 在 daemon 接线前完成 OS 沙箱、完整包摘要、hash-to-exec 原子性、进程树清理和 effect enforcement。

## 执行顺序

1. 先完成 G0 并冻结审计证据。
2. G1 的协议生成、确定性 trace 和崩溃矩阵并行推进；任何失败会阻止 M1 新功能合入。
3. G2 先接只读工具，再做多轮 loop、凭据和 steer；不提前接写工具或 shell。
4. G2 退出门槛全部通过后才进入 G3；M2 中所有副作用能力默认关闭，逐项接线并逐项证明不可绕过。

## 每次合入的统一门禁

- Rust：format、clippy `-D warnings`、workspace tests、schema drift、golden trace。
- TypeScript：typecheck、unit tests、build、JSONL/interactive trace parity。
- 安全：路径/链接、终端控制字符、环境注入、资源上限、取消、重启和 command dedup 回归。
- 文档：README、ARCHITECTURE、PROTOCOL、ROADMAP 与实际 daemon 行为必须同步。
- 交付：没有测试证据的 checkbox 不得标记完成；尚未接入 daemon 的原语只能标记为“已实现组件”，不能标记为“能力已交付”。
