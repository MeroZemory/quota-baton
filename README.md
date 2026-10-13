# quota-baton

A local Windows proxy for subscription accounts used by Claude Code and Codex CLI.
The implementation follows [the behavioural contract](docs/contract.md).

Build and verify with Rust 1.93:

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Use an isolated data directory for development:

```powershell
$env:QB_HOME = "$PWD/.sandbox/dev-home"
cargo run -p quota-baton -- serve
cargo run -p quota-baton -- status
```

The default data and admin ports are 10200 and 10201. `QB_HOME` overrides
`%LOCALAPPDATA%/quota-baton`. Configuration is read from `config.toml` in that directory.
The data and admin listeners require separate keys from `client.key` and `admin.token`.
M0 authenticates local requests and serves admin health; data requests return 404.

Implementation milestones and pending protocol gates are recorded in
[the architecture](docs/architecture.md) and [the decision log](docs/decisions.md).
