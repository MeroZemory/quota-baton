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

M1a imports CPA accounts in linked mode. Use a synthetic fixture directory during
development:

```powershell
cargo run -p quota-baton -- accounts import cpa --dir .sandbox/fixtures/cpa --dry-run
cargo run -p quota-baton -- accounts import cpa --dir .sandbox/fixtures/cpa --activate codex:synthetic@example.invalid
cargo run -p quota-baton -- accounts list
```

Imports start disabled unless explicitly activated. A running core owns the
credential lease, so the CLI sends imports through its authenticated admin API.
Otherwise the CLI acquires the lease for the write. Dry runs do not create account
or lock files. Tokens are protected using CurrentUser DPAPI and application entropy.
Linked accounts read rotated tokens from CPA by identity; they make no token-endpoint
calls. Older tokens are skipped, including on re-import; explicit activation still
applies. `--adopt` is reserved for M8 and currently refuses.

OAuth login, refresh, forwarding, live polling, and tray UI remain later milestones.

Implementation milestones and pending protocol gates are recorded in
[the architecture](docs/architecture.md) and [the decision log](docs/decisions.md).
