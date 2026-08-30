# Stable protocol schemas

These JSON Schema files are generated from the Rust types in
`crates/yeux-protocol`. Do not edit `*.schema.json` by hand.

Regenerate them from the repository root:

```bash
cargo run -p yeux-protocol --example export_schemas
```

Check committed files without changing them:

```bash
cargo run -p yeux-protocol --example export_schemas -- --check
```

The `yeux-protocol` wire test performs the same byte-for-byte drift check.
