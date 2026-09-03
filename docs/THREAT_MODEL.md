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
- Malformed, duplicated or oversized tool-call fragments, and resource
  exhaustion through deep, large or link-heavy workspace trees.
- Descriptor-relative workspace mutation, including replacement of an
  intermediate directory, symlink, hardlink or final target while a patch is
  being prepared or published.
- Provider/tool invocations that cross an execution boundary and become
  durable `Unknown`, including operator reconciliation and idempotency.

## Out of scope for v1

- An attacker with root or equivalent control of the host.
- A malicious configured model provider, which necessarily receives the
  prompts sent to it.
- Serving mutually distrusting users from one daemon, remote execution and
  enterprise policy distribution. The per-user Unix socket still preserves
  the ordinary host UID boundary.
- Encryption at rest beyond host filesystem permissions. A production
  deployment may supply an operating-system keychain or enterprise secret
  store through `CredentialBroker`; the standalone CLI currently supplies a
  no-op broker and does not claim keychain protection.

## Mandatory invariants

1. Every side effect passes the same prepare, policy, approval and execution
   pipeline.
2. Child agents can only inherit or reduce capabilities and budgets.
3. Untrusted project content cannot grant trust to itself.
4. Unknown non-idempotent invocations are reconciled, never retried silently.
   `invocation/reconcile` accepts only a terminal parent Turn, a matching
   invocation in `Unknown`, and bounded evidence whose source is
   `operator_review`; it records the decision idempotently and never invokes
   the provider or tool again.
5. Replay reads persisted events only and performs no provider, tool or network
   calls.
6. Failure to establish the requested sandbox fails closed. Backend detection
   runs an isolation probe before advertising a capability, and every process
   spawn performs a bounded launcher handshake. Structured workspace mutation
   and arbitrary process execution have separate capability gates: missing
   process isolation does not disable a safe descriptor-bound file mutation,
   while an unproven process boundary is not advertised.
7. Unix sockets live below a private current-UID directory; clients reject
   wrong owner, mode or type and reject parent/socket device-inode changes
   across connect.
8. Untrusted text presented by the TUI is sanitized at its human-terminal
   sinks; JSONL retains the original protocol payload for faithful replay and
   automation. Direct plugin-host/daemon stderr remains a separate integration
   boundary until those processes become supported interactive entry points.
9. Provider error bodies, SSE bytes/events, emitted output, tool-call
   arguments, workspace traversal and tool results have hard resource
   ceilings.
10. Process target variables never enter the Seatbelt or bubblewrap launcher
    environment before isolation is active. Launchers receive only a fixed
    minimal environment; target variables are injected after the sandbox is
    established, and inherited sensitive variables are rejected.
11. The Agent loop always advertises the built-in
    `workspace.list`, `workspace.read` and `workspace.search` tools. It may
    advertise `workspace.apply_patch` and `process.run` only after the daemon
    confirms the required OS sandbox and a non-`observe` host ceiling; those
    calls still require the unified policy/approval/permit path. Their resolved
    effects remain inside the opened workspace unless an explicit higher mode
    is granted; unknown or unnegotiated tools never fall through to a shell,
    plugin or other executor.
12. Concurrent read-only calls may finish in any order (with one search slot per
    canonical workspace identity), but their persisted results and the next
    model request follow the model's original call order.

13. Workspace mutation is bound to an opened canonical root descriptor. On
    Unix, every component is opened with `O_NOFOLLOW`, temporary publication is
    dirfd-relative (`O_EXCL` + `renameat`), and the parent directory identity is
    rechecked immediately before publish. This closes path traversal,
    intermediate-directory replacement and symlink redirection. POSIX does not
    provide a conditional “rename only if this final name still names inode X
    or hash H” primitive: a hostile writer can replace the final name after
    the last check, or move the opened parent directory. The guarantee is
    therefore object/descriptor binding, not a complete namespace CAS; an
    unproven outcome must remain visible and be reconciled.

14. Credential material is represented by an opaque handle and a short-lived
    broker lease. Secrets are resolved only at the provider HTTP boundary and
    are excluded from model content, ledger payloads, diagnostics and ordinary
    child-process environments. A no-op or unavailable broker fails closed;
    no standalone CLI claim is made about keychain/enterprise storage.

15. Sandbox network capability is not an endpoint policy. The current runtime
    can isolate a namespace/profile, but does not yet provide a complete
    domain/IP proxy with private-network, cloud-metadata, DNS-rebinding and
    proxy-bypass defenses. Those controls remain release blockers.

## Known residuals and release blockers

- The final-name POSIX mutation race described in invariant 13 cannot be
  removed without a cooperating filesystem primitive or an external compare-
  and-swap/lock protocol. Callers must use the revision snapshot and
  reconciliation evidence rather than infer success from a path check.
- Linux process isolation currently relies on the probed bubblewrap PID
  namespace/`--die-with-parent` path. macOS Seatbelt deliberately reports no
  arbitrary descendant isolation, so `process.run` is unavailable there.
  Cross-platform supervisor/cgroup evidence and crash-injection coverage are
  not complete.
- Unix process cleanup observes leader exit with `waitid(WNOWAIT)`, keeping the
  numeric PID/PGID pinned until descendants have been signalled. After the
  leader is reaped, cleanup never signals that number again; remaining group
  evidence is treated as `Unknown` to avoid PID-reuse collateral damage.
- An execution error is terminal `Failed` only when it is proven to precede
  the side effect. Process-group/output uncertainty, mutation worker loss, and
  directory-sync failure after rename are persisted as `Unknown` and require
  reconciliation before any retry.
- Artifact storage has content-addressed primitives, but end-to-end tool-output
  spill, ledger references, retention/GC and reconciliation evidence linkage
  are still pending.
- Native provider adapters, an OS/enterprise credential backend, endpoint
  network proxying, scheduler/MCP/Skills execution and release signing are
  outside the current tested baseline.
