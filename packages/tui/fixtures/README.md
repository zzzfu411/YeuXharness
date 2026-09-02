# Presenter fixtures

These JSONL files are inert event streams for the four paper presenters. They
exercise the approval gate and the durable `unknown`-then-`failed` ordering
without invoking a daemon, provider, filesystem, process, or network
operation. Fixture tool IDs are intentionally unregistered.
