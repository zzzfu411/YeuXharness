# Baseline evidence

These files preserve the relevant source as it existed when the 2026-08-30
audit began. They are not build inputs. The audit remediated two confirmed
findings and two release-hardening gaps in place, so the final production files
show the fixes rather than the original implementations.

- `transport.pre-fix.ts`, `renderer.pre-fix.ts`, and `prompter.pre-fix.ts` were
  copied before the TypeScript remediation.
- `process.pre-fix.rs` and `provider.pre-fix.rs` were copied before the Rust
  remediation.
- `server.pre-fix-excerpt.rs` records the exact relevant functions from
  `crates/yeuxd/src/server.rs` before the private-parent validation patch.

SHA-256 digests for these evidence files are recorded in `SHA256SUMS`.
