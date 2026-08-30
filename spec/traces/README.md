# Golden traces

Golden traces are JSONL protocol/event scenarios. Fixtures use fixed client
command IDs and compare server-generated identities across replay. Runners may
inject or capture nondeterministic fields, but event order and durable identity
must remain explicit. Agent-loop replay tests must additionally assert that the
provider, network and tool invocation counters remain zero.

`thread-lifecycle-v1.jsonl` is executed by
`crates/yeuxd/tests/golden_trace.rs`. It covers initialization, workspace and
thread creation, catch-up subscription, turn creation and interruption, event
replay, in-process command deduplication, daemon restart, and durable command
deduplication after restart.

Each line is one operation:

- `command` sends the supplied JSON-RPC envelope, captures response values,
  checks JSON Pointer expectations, and consumes the listed event notices.
- `restart` closes the connection, drops the daemon, and opens the same SQLite
  state directory with a new daemon instance.

Strings of the form `${name}` refer to a value captured by an earlier step or
provided by the runner. This fixture drives the real `yeuxd --stdio` binary,
captures its event IDs, then checks those same IDs after restart so changes in
ordering or persistence are visible as trace failures.
