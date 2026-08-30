# ADR 0002: Append-only event ledger

Status: accepted

SQLite WAL is the canonical store. Events are append-only and include a UUIDv7
identifier, per-thread monotonic sequence, schema version and causation links.
Queryable thread, turn, item and job state is a projection that can be rebuilt
from those events. JSONL is an interchange format, not a second source of
truth.

Replay means deterministic projection reconstruction. It does not re-execute
models or tools. External side effects use a durable invocation state machine;
an uncertain non-idempotent completion becomes `unknown` and requires explicit
reconciliation.

