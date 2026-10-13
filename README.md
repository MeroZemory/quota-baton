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
bytes pass through unchanged. Uploads buffer at most the per-request retention cap;
larger uploads stream the buffered prefix followed by the remaining client body.
Only complete bodies within the cap and shared budget can be retried after a
linked-token reload. Expiry reloads run at most once per account per minute;
401-triggered reloads bypass that throttle. There is no account failover yet.

The core acquires its credential lease and loads accounts and passive quota state
before opening listeners. It runs at above-normal process priority. Passive quota
headers persist under `state/quota.json`; request metadata and linked-reload events
persist under `ledger/`. No bodies or credentials are logged. Request retention is
released at the bounded commit decision (first data/error, 64 KiB, or 10 seconds).
SSE classification uses an incremental side decoder for gzip, deflate, Brotli and
zstd. Unknown or stacked encodings commit immediately; relayed bytes stay raw.
The accepted C3 limitation remains: reqwest inserts `Accept: */*` when a client
omits `Accept`; genuine clients supply that header themselves.
Observability uses a bounded non-blocking queue and batches disk synchronization
once per second. Queue overflow drops events and reports counts once per minute.
Corrupt quota caches are moved aside and ledger pruning failures are warnings.
Successful streams are recorded when their terminal event is delivered, even if
the client closes before upstream EOF. Failed terminal events record an error.
A bounded observer recognizes Claude and Codex terminal SSE events, including
compressed streams; disconnects before a terminal event remain `client_drop`.
Logs default to `info` for file and stderr. `QB_LOG` overrides `RUST_LOG` when set.

Codex WebSocket upgrades dial upstream before answering the client. Each socket
pair stays on its selected account, shares the HTTP linked-token gates and uses
the existing TLS client. Handshake headers are generated independently per hop.
The loopback leg declines compression. `[websocket] upstream_deflate=true` fails
startup because tungstenite 0.28.0 has no deflate support; adopting a deflate fork
and the live D12 A/B test remain design decisions.

Both legs allow 192 MiB messages and frames by default. Text/binary payloads and
control messages pass through verbatim; large outgoing messages are fragmented
into 64 KiB frames so pongs can run between writes. The shared in-flight budget
pauses client reads and counts complete messages until writes finish. A small
local patch to crates.io tungstenite releases large socket buffers after use;
its source, licenses and regression are in `vendor/tungstenite`.

Only upstream text messages of at most 1 MiB are inspected, for response tracking,
quota and error identifiers. No transcript or previous-response state is retained.
The ledger includes response and connection records plus bounded
`upstream_error_code` identifiers on HTTP and WebSocket responses. Socket pairs
close normally after `idle_close_secs` (default 1800); upstream reads time out
after 600 seconds, reset by upstream pings. Debug `phase0` builds add
`serve --phase0-close-after-response` to close normally after each completion.

For a synthetic manual check, run `cargo run -p qb-testkit --example ws_fakes`;
its `--client ws://127.0.0.1:<port>/v1/responses` mode prints only a sanitized
transcript. Use the matching synthetic fixture key specified in the example,
an isolated `QB_HOME`, ephemeral ports and a `testkit` build for the local base.

Phase 0 account pins use the authenticated admin API:
`POST /admin/v1/debug/pin` with `{account_id, session_key?}` and
`DELETE /admin/v1/debug/pin`. The account determines the provider family;
`session_key` uses the internal prefixed form (`codex:<id>` or `claude:<id>`).
Session pins override that family's global pin; pins clear on
restart. Pins prefer an eligible account; they never bypass paused/disabled state,
health or credential gates.

M4a persists account health and user state under the credential lease. HTTP retries
use the retained, unchanged body only before commit. A request tries at most four
accounts, with one extra token or network retry per account. Selection currently
prefers the provider pin, then eligible account IDs in order; M4b supplies bindings
and the full selection policy. A matching pre-commit WebSocket error is suppressed
after its health transition is saved, followed by close 1012. An idle connection
whose account becomes ineligible closes with 1000. Post-commit errors stay verbatim.

Passive headers and Codex rate-limit events update quota health, including the
credits hold and early reset. Exhausted HTTP accounts replay a bounded cached limit
response with its original encoding; WebSocket upgrades return native terminal 429.
The admin API adds `/state`, `/events`, account pause/resume/disable/enable and
settings PATCH. `quota-baton status` renders a Korean account table; account actions
use the running core, or acquire the lease directly while it is stopped. Settings
PATCH preserves TOML comments and other fields and persists account metadata.

`cargo run -p qb-testkit --example m4a_fakes` starts local synthetic HTTP and
WebSocket fault servers. Its `--client ws://127.0.0.1:<port>/v1/responses` mode checks
1012, reconnect on another account and normal completion, without printing bodies
or credentials. Use an isolated synthetic `QB_HOME` and a debug `testkit` core.

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
For a captured request, a separate response record includes limit header names
and values (`anthropic-ratelimit-*`, `x-codex-*`, `retry-after`, `x-request-id`).

The `phase0` debug feature adds body capture and replay for synthetic portability
checks. These captures contain conversation content; use synthetic conversations
and delete the files after verification. A release build enabling `phase0` fails.

```powershell
cargo build -p quota-baton --features phase0
target/debug/quota-baton.exe serve --capture-bodies-dir .sandbox/body-captures --capture-bodies-limit 20
target/debug/quota-baton.exe phase0 replay --capture .sandbox/body-captures/body-example.json --account cx-example --strip reasoning
```

The capture directory must already exist inside a canonical `.sandbox` path.
Only complete `/v1/messages` and `/v1/responses` bodies within the upload cap are
captured. Authorization, API key, cookie and host headers are excluded. Captures
store raw base64 bytes and decoded JSON for zstd. Bodies and decoded input are
bounded to 64 MiB; capture files are bounded to 192 MiB including base64 overhead.

Replay requires a running core and its admin token. The authenticated debug route
`POST /admin/v1/phase0/replay` accepts `capture_path`, `account_id`,
`strip_encrypted` (`none`, `reasoning`, `all`; default `none`) and
`tamper_encrypted` (default `false`). It calls the same relay, credential gates and
linked-reload path as product traffic, without changing account pins.
`--strip reasoning` preserves the narrower legacy diagnostic mode; `--strip all` is a diagnostic
control removing every encrypted Codex input item or every Claude thinking block.
`--tamper-encrypted` changes the first encrypted value or signature across types.
The CLI prints presence counts before sending and one summary line afterward:
status, error fields, usage, serving account, applied mode, removal counts and
tampered type. It prints no body or credentials. Unchanged replay preserves raw
bytes; transformations retain the original supported content encoding.

The synthetic HTTP harness can be started with
`cargo run -p qb-testkit --example http_fakes`. Full fake-upstream integration tests
run with `cargo test --workspace`; isolated core integration runs need
`cargo test -p qb-core --features testkit`.

OAuth login, active polling, full selection/bindings and tray UI remain later
milestones. Live upstream validation remains gated by the design-session G1 checks.

Implementation milestones and pending protocol gates are recorded in
[the architecture](docs/architecture.md) and [the decision log](docs/decisions.md).
