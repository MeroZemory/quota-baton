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
HTTP routes now forward to one linked account per provider. Unknown routes return
404; a family with no active account returns a native 400 error.

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

The upstream defaults are `https://api.anthropic.com` and
`https://chatgpt.com/backend-api/codex`. The normal build accepts only those exact
bases; the explicit `testkit` feature in debug builds also accepts
`http://127.0.0.1:<port>` for fake upstreams. Redirects and automatic decompression are disabled. Request and response
bytes pass through unchanged. The M2 interim upload path buffers the whole body;
only bodies within the retention cap and shared budget can be retried after a
linked-token reload. There is no account failover yet.

The core acquires its credential lease and loads accounts and passive quota state
before opening listeners. It runs at above-normal process priority. Passive quota
headers persist under `state/quota.json`; request metadata and linked-reload events
persist under `ledger/`. No bodies or credentials are logged. Request retention is
released at the bounded commit decision (first data/error, 64 KiB, or 10 seconds).

Phase 0 account pins use the authenticated admin API:
`POST /admin/v1/debug/pin` with `{account_id, session_key?}` and
`DELETE /admin/v1/debug/pin`. Session pins override the global pin; pins clear on
restart. These controls deliberately override active-account selection for tests.

Shape capture needs an explicit debug feature build and an existing directory whose
canonical path contains a `.sandbox` component:

```powershell
cargo build -p quota-baton --features shape-capture
target/debug/quota-baton.exe serve --capture-dir .sandbox/captures --capture-limit 50
```

It is off unless requested, expires after the request limit, and records only header
names, approved header values, and JSON key names. The release build refuses the
`shape-capture` feature. Capture files are independent of `QB_HOME`.
Key scanning is bounded to 64 MiB of decoded input, depth 3, and 4096 keys;
capture records explicitly mark unavailable or truncated key observations.

The synthetic HTTP harness can be started with
`cargo run -p qb-testkit --example http_fakes`. Full fake-upstream integration tests
run with `cargo test --workspace`; isolated core integration runs need
`cargo test -p qb-core --features testkit`.

OAuth login/refresh, WebSocket relay, failover, live polling, and tray UI remain later
milestones. Live upstream validation remains gated by the design-session G1 checks.

Implementation milestones and pending protocol gates are recorded in
[the architecture](docs/architecture.md) and [the decision log](docs/decisions.md).
