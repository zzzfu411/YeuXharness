# Presenter fixtures

These JSONL files are inert event streams for the four paper presenters. They
exercise the approval gate and the durable `unknown`-then-`failed` ordering
without invoking a daemon, provider, filesystem, process, or network
operation. Fixture tool IDs are intentionally unregistered.

Replay the approval fixture, including its rendered 朱印 gate, with:

`pnpm --filter @yeux/tui build && pnpm --filter @yeux/tui start -- replay packages/tui/fixtures/paper-approval-gate.jsonl`

`paper-m2-cannot-bypass.jsonl` records the M2 fail-closed boundary: a write
proposal waits for approval and cannot reach execution when the required OS
sandbox is unavailable.
