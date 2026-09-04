# Changelog

YeuX Harness follows Semantic Versioning. Versions below `1.0.0` may change
protocol and product behavior between previews; protocol compatibility remains
governed by the explicit JSON-RPC protocol version.

## [0.1.0-alpha.1] - 2026-09-04

First source-only GitHub Developer Preview.

### Added

- Rust-authoritative local daemon with append-only SQLite event ledger,
  deterministic replay, workspace/thread/turn projections and bounded agent
  loops.
- Structured workspace read tools and protected `workspace.apply_patch` and
  `process.run` adapters behind EffectSet, capability intersection,
  invocation-bound approval, sandbox checks and one-shot execution permits.
- Minimal Git repository fixture covering read, public plan, bad patch, failed
  check, revision-bound repair, passing check and final diff, plus a real
  JSON-RPC workspace-trust and wire-approval fixture.
- Evidence-only reconciliation for Unknown invocations, restart recovery and
  durable terminal evidence without automatic side-effect replay.
- Line-oriented TUI control plane with model/doctor/context/plan/thread/mode,
  steer, interrupt and reconciliation commands, deny-by-default approval,
  capability diagnostics, clean EOF handling and grapheme-aware framing.

### Changed

- Kept the sandbox launcher handshake fail-closed and bounded while increasing
  its deadline from two to five seconds, preventing false capability loss when
  a macOS host is briefly saturated during parallel release tests.

### Preview limitations

- This release contains source archives only. It does not provide signed
  binaries, an installer, Homebrew formula, SBOM or upgrade/rollback tooling.
- The full process fixture requires Linux strict sandbox capability; arbitrary
  process execution remains disabled on macOS until descendant supervision can
  be proven.
- Dedicated Git checkpoint/worktree tools, artifact spill and GC, multi-repo
  task gates, full crash injection, context compaction, full-screen TUI and
  provider onboarding remain release work.

[0.1.0-alpha.1]: https://github.com/zzzfu411/YeuXharness/releases/tag/v0.1.0-alpha.1
