# ADR 0001: Rust authority and TypeScript surface

Status: accepted

The Rust `yeuxd` process exclusively owns the event ledger, providers, tools,
policies, sandboxes and jobs. The TypeScript `yeux` client only speaks the
versioned JSON-RPC protocol and renders events. It never opens the database or
executes tools directly.

Interactive clients connect to the per-user Unix socket when the daemon is
enabled; otherwise they spawn `yeuxd --stdio`. A single-writer lock prevents a
stdio child and the daemon from owning the same ledger concurrently.

