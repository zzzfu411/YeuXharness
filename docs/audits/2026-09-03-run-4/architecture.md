# YeuX Harness security-audit architecture (Run 4)

Audit target: the current working tree of `/Users/zfu/Documents/develop/YeuX/Harness`.
Audit date: 2026-09-04 (Asia/Shanghai). The M2 security/recovery implementation
and its audit record are finalized in the local repository; remote state is checked
separately in `STATUS_AND_PLAN.md`.

## Application and deployment

YeuX Harness is a local, single-user coding-agent harness, not a web service or
multi-tenant API. A TypeScript `yeux` TUI/JSONL client talks to the Rust `yeuxd`
authority over newline-delimited JSON-RPC on stdio or a private per-UID Unix
socket. The daemon owns the SQLite WAL event ledger, artifact directory, model
provider, tool registry, policy/approval pipeline and runtime adapters. The
supported host model treats another process with the same UID as an operator,
and explicitly excludes root/equivalent users, mutually distrustful users on
one daemon, remote execution and malicious provider configuration from v1.

The implementation is Rust 1.98, edition 2021, Tokio/Serde/Reqwest/rustix/
rusqlite/BLAKE3/UUIDv7, with TypeScript 5.9, Node 22 and pnpm 9.15.9 clients.
Key entry points are:

- `crates/yeuxd/src/server.rs` — daemon initialization, framing, socket/stdio,
  command gate and subscriptions.
- `crates/yeuxd/src/commands.rs` — JSON-RPC dispatch, workspace/thread/turn,
  approval and `invocation/reconcile` commands.
- `crates/yeuxd/src/runner.rs` — bounded provider/tool loop, context assembly,
  cancellation settlement and durable invocation results.
- `crates/yeuxd/src/pipeline.rs` and `crates/yeuxd/src/tools.rs` — sealed
  registry, effect planning, capability intersection, approval binding,
  revalidation and execution permits.
- `crates/yeux-runtime/src/ledger.rs` — append-only facts, receipts and pure
  projection/replay; `workspace.rs`, `workspace_tools.rs`, `process.rs`,
  `sandbox.rs`, `provider.rs`, `credentials.rs` and `artifact.rs` implement
  runtime boundaries.
- `packages/tui/src/transport.ts`, `app.ts`, `renderer.ts` and `terminal.ts` —
  client transport/control plane and human-terminal projection.
- `packages/plugin-host/src/manifest.ts` and `plugin-host.ts` — experimental
  out-of-process plugin host, deliberately not advertised by the daemon.

Comparable products are local coding agents such as Grok Build, OpenAI Codex,
DeepSeek Harness and Pi. They provide broader edit/shell/provider/MCP/long-task
and release surfaces. YeuX intentionally keeps a smaller provider-neutral
authority kernel, append-only replay and fail-closed capability gates; it does
not claim parity with their mature installers, cross-platform supervisors,
artifact handling, compaction or ecosystem.

## Trust boundaries and capabilities

Untrusted inputs include CLI values and environment-selected paths, JSON-RPC
params and content blocks, workspace names/files/symlinks, model-generated
tool-call JSON, provider HTTP/SSE bytes and experimental plugin manifests and
outputs. The model is not an authority: only daemon-registered and negotiated
tools enter `InvocationPipeline`. The pipeline normalizes arguments, computes
effects, intersects host/user/project/turn grants, asks the daemon approval
handler for side effects, revalidates identity/revision and consumes a one-use
permit. Unknown or unadvertised tools never fall through to shell execution.

The built-in read tools are always bounded and read-only. `workspace.apply_patch`
uses a retained canonical root descriptor, component-wise `openat(O_NOFOLLOW)`,
exclusive temporary files, `renameat` and parent/revision checks. This is a
descriptor/policy/approval boundary in the daemon; it is not an OS process
sandbox. POSIX still lacks a conditional final-name inode/hash rename. The
arbitrary `process.run` adapter is separate and is advertised only when the
detected backend proves filesystem, network and process isolation. Linux uses
bubblewrap PID namespace/`--die-with-parent`/new session; macOS Seatbelt does
not claim strict process-tree isolation, so the process tool stays closed there.

Credentials are opaque handles resolved by a daemon-owned `CredentialBroker`
into a short-lived non-serializable lease only at the provider HTTP boundary.
The standalone CLI installs a no-op broker, so an unresolved handle fails
closed. Provider errors, debug values, event streams and child environments are
redacted or reject secret-like values. An OS keychain/enterprise backend is not
yet implemented.

## Input and sink inventory

| Input | Main path | Dangerous or security-relevant sink |
|---|---|---|
| JSON-RPC frames | `server.rs` framing/dispatch; `commands.rs` | ledger append, workspace paths, turn state, approval/reconciliation decisions |
| CLI/env options | `crates/yeuxd/src/main.rs`, `packages/tui/src/args.ts` | provider URL/handle, state/socket/daemon paths, workspace cwd, process launch |
| Provider HTTP/SSE | `provider.rs:stream_inner`, `parse_chunk`, `SseDecoder` | model context, tool-call arguments, terminal events, tool registry dispatch |
| Workspace files and metadata | `workspace.rs`, `workspace_tools.rs` | reads/search, patch publication, revision/identity checks |
| Model tool output | `runner.rs`, `tool_calls.rs`, `pipeline.rs` | process executable/argv, file mutation, network/effect authorization |
| Ledger/history | `ledger.rs`, `runner.rs` context loading | provider prompt reconstruction and state-machine transitions |
| Reconciliation evidence | `commands.rs:invocation_reconcile`, artifact store | durable terminal decision; intentionally no provider/tool re-execution |
| TUI/plugin text | `renderer.ts`, `terminal.ts`, `plugin-host.ts` | human terminal writes, plugin child process/RPC; plugin host is unsupported for untrusted code |

There is no in-repository HTTP server, browser DOM, SQL query assembled from
attacker identifiers, dynamic `eval`, or supported MCP/plugin execution path.
The provider is an outbound HTTP client; proxy/cache/auth-protocol findings
requiring deployment components are therefore not source-confirmed here.

## Prior audit coverage and gaps

Run 1 confirmed and fixed terminal control-sequence injection and cross-UID
fallback socket spoofing. Run 3 confirmed and then fixed the long-common-prefix
`workspace.search` CPU denial of service; its other candidates (interrupt race,
root/intermediate-directory replacement under the stated v1 actor model, forged
TurnStart role, provider decoder complexity) were rejected or retained as
release hardening. Run 4 focuses on credential redaction/lease boundaries,
descriptor-bound mutation, process lifecycle and PID reuse, Unknown/reconcile
semantics, artifact URI validation, and CI/schema/release drift.

The current hunt should concentrate on sad paths and cross-component state:
post-dispatch result budgeting, cancellation/timeout and reaping, final-name or
executable identity races, credential/error disclosure, reconciliation replay,
and model output reaching file/process/terminal sinks. Do not report a missing
keychain, SBOM, branch rule or cross-platform feature as an exploitable finding
unless a concrete attacker path and impact are demonstrated.
