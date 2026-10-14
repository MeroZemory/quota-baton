# quota-baton decision log

## Document status

| Field | Value |
|---|---|
| Role | Decision record: completed and pending decisions, and who made them |
| Lifecycle | Living document |
| Normativity | A decision marked Accepted is authoritative within its stated scope |
| Last verified | 2026-10-13 |

Status values: **Accepted** means the user decided or confirmed it. **Proposed** means it awaits the user.

<a id="d1"></a>
## D1. Product scope — Accepted (user, 2026-10-13)

quota-baton is a personal local proxy and a Windows tray app. It rotates the user's own subscription accounts for genuine Claude Code and Codex CLI. It replaces the user's earlier proxy setup.

The user rejected both tools as buggy and memory-heavy, and rejected patching them because that would mean maintaining a hand-maintained fork.

<a id="d2"></a>
## D2. Pass-through only; no protocol translation — Accepted (user, 2026-10-13)

quota-baton does not translate between protocols. This includes using Claude models from Codex, which will never be supported.

<a id="d3"></a>
## D3. No client impersonation — Accepted (design owner, 2026-10-13)

quota-baton does not implement any of the following:
- TLS or HTTP fingerprint imitation
- header disguise
- identity or device rewriting
- client "cloaking"

It serves only genuine Claude Code and Codex CLI requests and changes only credentials (contract C3/C4).

The user asked for client impersonation, and the design owner declined to build it. The user has been told this.

<a id="d4"></a>
## D4. The orchestrator boundary: option C — Accepted (user direction, 2026-10-13)

The orchestrator does not know quota-baton exists and only chooses the provider. quota-baton does all account selection and switching.

The orchestrator keeps working unchanged in one case: when every account is exhausted, quota-baton returns the native limit errors, and the orchestrator's existing limit handling still applies.

Required the orchestrator changes are separate work in the the orchestrator repo:
- revise ADR-0032 and ADR-0040
- reverse H-0136
- drop per-account homes
- make Claude launches use the default config deterministically

The latter also resolves the Claude adapter env leak at `packages/claude-code-adapter/src/index.ts:147`. quota-baton exposes no orchestrator-specific API.

<a id="d5"></a>
## D5. Where accounts come from — Accepted (user, 2026-10-13; revised the same day at the user's request)

Accounts enter quota-baton in two ways: through its own OAuth login, or through **import from CPA's auth files**.

An import runs in one of two modes, so that a single-use refresh token never has two refreshers:
- **Linked.**
  - CPA stays the refresh owner, and quota-baton never calls a token endpoint for the account.
  - It re-reads CPA's file for that account to follow each rotation.
  - This needs no change to CPA and is usable during the shadow run.
- **Adopted.**
  - quota-baton becomes the sole refresh owner.
  - This is allowed only after CPA can no longer refresh the account: CPA has stopped for good, or the user has removed the file from CPA's auth directory.
  - It is a cutover step (M8).

Evidence behind the rule (2026-10-13):
- CPA refreshes **disabled** accounts as well. Its disabled Claude files were refreshed at 09:14. Disabling an account in CPA therefore does not hand over ownership.
- Rotated refresh tokens are single-use. Reusing one can revoke the whole token family (R1 §2, R2 §2), as in the 10-10 incident.

**Never imported:**
- `~/.codex`, `~/.claude`, Orca and the orchestrator homes. Their CLIs refresh the tokens themselves.

<a id="d6"></a>
## D6. Implementation language: Rust — Accepted (user, 2026-10-13)

The core and the tray are both Rust. The reasons:
- the memory target
- long-running stream stability
- a single executable
- first-party Windows bindings (`windows` crate)

Hot patching is handled by architecture rather than language. Config and accounts reload live, the tray is a separate process, and the core upgrades by takeover-and-drain. Python and TypeScript were considered and not preferred.

<a id="d7"></a>
## D7. Two processes: core and tray — Accepted (user, 2026-10-13)

The UI can crash, restart or be rebuilt without cutting live Claude Code or Codex streams.

<a id="d8"></a>
## D8. Providers — Accepted (user, 2026-10-13)

The MVP covers Claude and Codex.

Later, Anthropic-compatible API-key providers will be used from Claude Code: DeepSeek, Z.ai (GLM-5.3) and Xiaomi MiMo (v2.6 Pro). Using them from Claude Code is an assumption stated to the user and not yet contradicted.

pi agent and other non-genuine clients are out of scope.

Orca-managed Codex homes are not wired at cutover; the user does not use Orca (2026-10-13). quota-baton has no Orca-specific code, so a Codex CLI in any home that is pointed at it still works.

<a id="d9"></a>
## D9. Hands-off live configuration; cutover is the user's decision — Accepted (user, 2026-10-13)

Without explicit approval, none of these are modified:
- the live CPA config and auth files
- `~/.codex/config.toml`
- `~/.claude/settings.json`
- `%APPDATA%\herdr\bin\codex.ps1`

The server binds only to 127.0.0.1, and the repo and docs contain no secrets. The previous proxy stays installed until quota-baton is stable.

The herdr shim requires `openai_base_url` to remain a top-section key in `~/.codex/config.toml`.

<a id="d10"></a>
## D10. Mid-session hand-off: relay as the genuine clients do — Accepted (user direction "the way Codex and Claude do it", 2026-10-13)

Codex and Claude Code replay their encrypted or signed conversation items verbatim on every turn and never strip them: Codex reasoning and compaction blobs, Claude thinking signatures. quota-baton does the same. It never strips, rewrites or reorders them, and it has no hand-off rewrite.

**Evidence: V2 HTTP cells, 2026-10-13.**
- Items minted by account A and sent to account B: 200. They are silently ignored, because the input-token count equals the stripped body. This holds inside a tool-use turn as well.
- Tampered items on the minting account: 400. The server validates what it minted and ignores what it did not.
- A moved session keeps working. It loses only its earlier hidden reasoning, and the first turn after the move is cache-cold.

**Withdrawn:** C4 exceptions E1, E2, E3 and E2r, and header exception H3.

**Residual risk.**
- Codex remote-compaction blobs may be rejected on another account. The previous proxy logs show 3 such 400s after it lost a session binding, with the minting account unconfirmed (R8 I-16).
- Stripping cannot help there, because the blob is the earlier history. Persistent, sticky bindings (C7) keep a session on its minting account instead.
- The ledger records upstream error codes, so the failure is visible if it happens.

Rejected alternative: keeping full transcripts in the proxy, as earlier proxies do. It is the measured cause of their memory use (R6, R7).

<a id="d11"></a>
## D11. Account-switching policy — Accepted (decided by a P2 blind panel at the user's request, 2026-10-13)

The user delegated this decision to independent reviewers. Three models answered a neutral sheet blind: Opus 5.5 max, Fable 5.1 xhigh and Codex gpt-6.1-sol. The sheet, the answers and the union of conditions are in `.sandbox/reviews/switch-policy-blind-panel.md`. The normative rules are in contract C7.

**Goal:** minimize tokens re-read after cache misses, keep perishable quota from expiring unused, and need no user attention.

1. **New sessions: soonest weekly reset first (unanimous).**
   - The choice is limited to accounts that pass a gate on projected usage (in-flight requests included): 5-hour below 90 %, weekly below 90 %, or below 98 % within about 5 h of its reset.
   - If none passes, the account with the most headroom is used.
2. **Live sessions: move only when forced or free ("staged 1B"; Opus and Fable chose 1B, Codex 1A).**
   - **Forced:** the account becomes unusable (a limit, auth still failing after a refresh, or a user pause). A limit that resets within 60 s holds the request instead.
   - **Free:** after at least 1 h idle, when the cache is cold on every account, but only if the current account fails the gate (D15 adds: or is at its spread threshold).
   - **Overload (added by D15):** a bounded rebalance at a request boundary when an account is concentrated and another has room. V2 confirmed Codex's objection: a move keeps the conversation working but drops its earlier hidden reasoning, so a move that is not needed is avoided.
3. **Rejected:** restoring immediately when a preferred account recovers (unanimous), and moving warm sessions early to use expiring weekly quota (unanimous). New-session placement serves that goal without a cache cost.
4. **No priority list.** The only manual placement setting is per-account **drain** (no new sessions; existing sessions stay). `allow_credits` stays off by default.
5. **Tuning from data:** the ledger records moves, the weekly quota left unused at each reset, and forced moves per gate crossing.
6. **Caution:** the 88 % post-switch Codex cache figure may reflect shared prefixes rather than true cross-account sharing. Codex rules are not relaxed before a clean test.

<a id="d12"></a>
## D12. WebSocket permessage-deflate — Accepted (user, 2026-10-13): off by default; decide after an M3 A/B measurement

**Loopback leg (Codex ↔ quota-baton): always declined.** Compression saves nothing on loopback.

**Upstream leg (quota-baton ↔ chatgpt.com): a switch, `upstream_deflate`, default off.**
- quota-baton is not a client imitating Codex. It sits inside the address layer that Codex is pointed at, so this upstream connection is its own. Compressing it is purely a bandwidth and latency choice.
- Deflate is negotiated per connection. If the server declines, the connection simply runs uncompressed, so support can be checked safely (V4).

**Expected gain (R7).** Large Codex inputs are large mostly because of screenshots, which are already compressed.

| Input | Size, raw → zstd | Saved upload at 20 Mbps | at 100 Mbps |
|---|---|---|---|
| p95 | 33 MB → about 22 MB (1.5×) | about 4 s | about 1 s |
| Text-only | 3–4 MB → about 1 MB | about 1 s | under 0.3 s |

**Costs:** zlib state per connection, CPU, and a deflate-capable WebSocket library.

**Decision procedure (M3).** An A/B test with genuine Codex through quota-baton:
- Use a synthetic large test conversation of tens of MB, never the user's real conversations.
- A test switch makes the proxy open a new upstream connection every turn, so every turn sends the full input.
- Alternate deflate on and off on each connection, about 10 times each, on a less important account.
- Measure upload time, time to the first response event (which includes server-side decompression), proxy CPU and memory, and the negotiated extension.
- Enable it only if the time saved on the user's own line is clear and A1 still holds.


**Outcome (M3, 2026-10-13):** tokio-tungstenite 0.28 has no permessage-deflate, so enabling it would need a git fork. The switch stays off, and `true` refuses to start. Expected gain is modest (about 1.5x at p95, mostly screenshots). Revisit only if slow full-input uploads become a measured problem.

<a id="d13"></a>
## D13. Phase 0 test accounts — Accepted (user, 2026-10-13)

Log in to quota-baton **one account at a time**, starting with the main accounts the user already uses:
- **Codex:** `acct-alpha@mail.test`, then `hello.sample-acct@mail.test`. Both are plus plans, and both can spend paid credits.
- **Claude:** `acct-alpha@mail.test`. `hello.sample-acct@mail.test` is added when a second account is needed, for example for the V2 pair.

Before the next login, check that the same account still works in CPA. This guards against an unknown effect of a second login (O10).

Before M5's credit hold exists, the Phase 0 credit guard applies (architecture §16.1): no tests at 80 % or more of any window, and stop at 95 %.

<a id="d14"></a>
## D14. Rate limits and role separation — Accepted (user direction "separate each side's role exactly", 2026-10-14)

**Background.** On the night of 2026-10-14, about 16 Codex mains and their sub-agents shared two ChatGPT Plus accounts through the previous proxy. Many sessions stopped with "exceeded retry limit, last status: 429" and stayed stopped for hours. The root cause is in `<audit>/codex-429-recovery-2026-10-14/원인보고서.md`:
- **Capacity.** Upstream request-rate limits started at 60–72 requests per minute.
- **Fail-fast proxy.** The previous proxy answered 429 within 73 ms during a 0–4 s cooldown.
- **Re-binding cascade.** Sessions moved to the other account and re-sent 150–350k uncached tokens each, which pushed that account over its limit too.
- **Retry burn.** Codex stopped and left `/goal` stalled with no automatic resume. Codex 0.162.1 retries a rate 429 only when the response carries retry advice. The previous proxy's synthesized errors carried a ~4 s advice, so the 5 retries went within seconds. Without advice, the same error ends the turn at once.

**Decision. Each layer owns one thing.**

| Layer | Owns | Never does |
|---|---|---|
| quota-baton (proxy) | Account problems: failover for quota, auth and permission; absorbing short rate limits on the same account (rate hold and pacing, contract C6.3); precise native errors with `retry-after`; spreading sessions across accounts (D15) | Moving a session because of a rate limit **alone** (D15 moves a bounded number only when an account is concentrated and another has room); deciding how many sessions run in total |
| Client (Codex CLI, Claude Code) | Its own in-turn retries, unchanged | — |
| Orchestrator (an agent orchestrator, or `/goal` supervision) | How many sessions run in total (quota-baton spreads them across accounts, D15), and when a stopped session resumes: back off with jitter, resume in the same session (a fork loses the cache), never resume everything at once | Choosing accounts (D4) |

**Assignment of the 429 report's proposals:**
- **A (soft-pin on rate limits)** and **F (`retry-after`):** quota-baton, in C6.3. A rate limit never moves a session and never causes failover, whatever its `Retry-After`.
- **B (admission control):** quota-baton, in its simple form, which is per-account pacing in C6.3. Token buckets on uncached input are not adopted, because the upstream limits are not published and a guessed bucket would either throttle needlessly or not protect at all.
- **C (active-session cap with rotation):** spreading across accounts belongs to quota-baton (D15). The total and the rotation of stopped sessions belong to the orchestrator.
- **D (stall watcher with same-session resume):** the orchestrator.
- **E (account health signals):** quota-baton already covers this with the tray badge, the ledger of every health and user-state change, and its own usage polling (C9). The 401s from the quota-watch script belong to the old setup.

**Limit of absorption.** Under a sustained limit, a Codex HTTP turn survives about 5–6 minutes (C6.3 "Ceiling"). The orchestrator's caps and resume rules must take this into account.

**Review.** Fable independent review, `.sandbox/reviews/D14-rate-hold-review.md`. All five must-fix and six should-fix items were adopted into C6.3 on the same day.

<a id="d15"></a>
## D15. Session spreading and bounded overload rebalance — Accepted (user direction, 2026-10-15)

**User direction (quoted).** "a계정 빈도찼다고 무조건 b계정 연결해주면 캐시문제로 매우 불편함. 다만 이미 a계정에 너무 많은 세션이 몰려있었으면 분산을 위해 다른 계정으로 이관하는 것까지 허용해야 된다고 생각함. 이때 캐시 적중 안 하는 건 손해보다 이득이 크다고 생각. 그러나 이미 잘 분산되어 있을 때는 429 떴다고 다른 계정으로 바꿔주면 안 될 거라고 생각함."

This gives three rules:
- A 429 alone never moves a session.
- When sessions are concentrated on one account, moving some away is allowed despite the cache loss.
- When sessions are already well spread, a 429 moves nothing.

**Background.** D11 packs sessions: every new session goes to the account with the soonest weekly reset until its 5 h window projects 90 %, so with two idle accounts all sessions land on one. D14 made rate limits never move a session and never fail over. Together they left no way to relieve a concentrated account except through the orchestrator.

**Evidence.** The 2026-10-14 data (`원인보고서.md`, `proxy-rate-10min.tsv`, `tokload-10min.tsv`) does not yield a per-account capacity:
- On 10-13 (16:20 and 20:00), one Plus account served 61–70 requests per minute with 37–67 rollouts and no rate 429.
- On 10-14 at 03:10, the same account was limited at 13 requests per minute with 7 rollouts.
- In the two-account phase (01:20–02:40), the load was already split and both accounts were limited.

The upstream limit behaves dynamically. The spread threshold is therefore a heuristic for where D11's packing should stop, tuned from the ledger. It is not a capacity.

**Decision** (contract C7.1):
1. **Prevention.** New sessions go first to accounts with fewer than `spread_threshold` (default 5) active session groups, in D11 order within that set. When every account is full, they go to the lowest `active / spread_threshold`. A full account never refuses.
2. **A bounded cure.** An account counts as overloaded only when all of these hold:
   - rate 429s that reached clients hit 3 distinct groups within 10 min
   - the account is above its threshold
   - another eligible account has room, with an imbalance of at least 2

   Then at most 2 groups per 10 min are marked, cheapest (smallest last request) first. Each marked group moves only at its own next request boundary, and only to an account with room. Marks expire, in-flight work is never touched, and the request that got the 429 is never failed over.
3. **Well spread.** When no account has room, nothing is marked. A 429 then only paces (C6.3).
4. **Cold point.** A cold session on an account at or above its threshold is re-placed. The move is free.
5. **Roles (amends D14).** quota-baton spreads sessions across accounts (prevention and the bounded cure) and publishes `active_sessions` per account. The orchestrator decides the total number of sessions and when stopped sessions resume. quota-baton never queues or refuses a session.
6. **Amends D11.** Item 2 gains a third, bounded move cause: the overload rebalance at a request boundary. Item 3 stands, because D15 never moves sessions for quota.
7. **Errors that are not about accounts** never count. Model capacity ("Selected model is at capacity"), 5xx and 529 are the same on every account.

**Tuning from the ledger.** The inputs are:
- `overload_detected`, including the "well spread, no move" case
- `binding_move{overload}` with the next turn's cache result
- `active_sessions` over time

Later candidates: a per-plan default table, once Codex `plan` shows plan-dependent limits, and a lower `active_window_secs` if the count overstates load.

**Review.** Fable independent review, `.sandbox/reviews/D15-review.md`. All six must-fix and seven should-fix items were adopted the same day.
