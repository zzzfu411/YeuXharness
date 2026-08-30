# Threat model

YeuX treats model output, repository content, tool output, MCP servers and
third-party plugins as untrusted. Its security boundary is the intersection of
host limits, user policy, project trust and turn-specific capability grants,
enforced by the operating system rather than by prompts.

## In scope

- Model mistakes and prompt injection that attempt filesystem, process,
  network, secret or external side effects.
- Malicious repository configuration, skills, hooks and executable content.
- Plugins or MCP tools requesting undeclared capabilities.
- Symlink/path traversal, stale writes, command cancellation and crash windows.
- Accidental replay of non-idempotent work after a crash.
- Secret leakage through prompts, inherited environments, logs and artifacts.
- Local socket path substitution or daemon impersonation by another non-root
  account on the same host.
- TUI terminal control-sequence injection and unexpectedly large or fragmented
  provider responses.

## Out of scope for v1

- An attacker with root or equivalent control of the host.
- A malicious configured model provider, which necessarily receives the
  prompts sent to it.
- Serving mutually distrusting users from one daemon, remote execution and
  enterprise policy distribution. The per-user Unix socket still preserves
  the ordinary host UID boundary.
- Encryption at rest beyond host filesystem permissions; credentials still
  live in the operating-system keychain.

## Mandatory invariants

1. Every side effect passes the same prepare, policy, approval and execution
   pipeline.
2. Child agents can only inherit or reduce capabilities and budgets.
3. Untrusted project content cannot grant trust to itself.
4. Unknown non-idempotent invocations are reconciled, never retried silently.
5. Replay reads persisted events only and performs no provider, tool or network
   calls.
6. Failure to establish the requested sandbox fails closed.
7. Unix sockets live below a private current-UID directory; clients reject
   wrong owner, mode or type and reject parent/socket device-inode changes
   across connect.
8. Untrusted text presented by the TUI is sanitized at its human-terminal
   sinks; JSONL retains the original protocol payload for faithful replay and
   automation. Direct plugin-host/daemon stderr remains a separate integration
   boundary until those processes become supported interactive entry points.
9. Provider error bodies, SSE bytes/events, emitted output and tracked tool
   calls have hard resource ceilings.
10. Process target variables never enter the Seatbelt or bubblewrap launcher
    environment before isolation is active.
