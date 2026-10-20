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
  - It is a cutover step. The mechanism is D17.

Evidence behind the rule (2026-10-13):
- CPA refreshes **disabled** accounts as well. Its disabled Claude files were refreshed at 09:14. Disabling an account in CPA therefore does not hand over ownership.
- Rotated refresh tokens are single-use. Reusing one can revoke the whole token family (R1 §2, R2 §2), as in the 10-10 incident.

**Never imported:**
- `~/.codex`, `~/.claude`, Orca and the orchestrator homes. Their CLIs refresh the tokens themselves. Exception since 2026-10-19: an explicit, warned **takeover** from such a home under D18. It is never a silent import and never linked.

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
   - **Free:** after at least 1 h idle, when the cache is cold on every account, but only if the current account fails the gate (D15 adds: or is at its spread threshold). V2 confirmed Codex's objection: a move keeps the conversation working but drops its earlier hidden reasoning, so a move that is not needed is avoided.
   - **Overload (added by D15):** a bounded rebalance at a request boundary when an account is concentrated and another has room.
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

<a id="d16"></a>
## D16. Subscription OAuth only; API keys out of scope — Accepted (design owner, 2026-10-17)

**Trigger.** The previous proxy mixed four Claude OAuth credentials with four billable Claude API keys in one pool. When the OAuth credentials were rate-limited, session affinity pinned a large session to an API key, which then failed with `Credit balance is too low`. A diagnostic replay of that session cost USD 64.68.

**Decision.**
1. quota-baton serves only the user's **subscription OAuth accounts** (D5: its own login, or a CPA OAuth import). It never imports or uses API keys, so a billable credential can never enter the pool, and no silent OAuth-to-API fallback can exist.
2. If API keys are ever wanted, that is a new user decision. It must then follow the cost-safety policy in the incident note:
   - fallback is off by default
   - API keys live in a separate opt-in route or profile
   - existing OAuth sessions are never migrated to an API key
   - a billing error quarantines the credential without retries across other keys
   - the class and cost are visible before a request is sent
   - tests cover the safe default
3. Points from the note that already hold today:
   - provider-native errors pass through unchanged (C6 rows 2–7, C6.2)
   - bindings are reselected after quota, auth, permission or payment failures (C5, C7)
   - tests and diagnostics use testkit fakes and never replay a real session (AGENTS.md, C13)
4. **Candidate, not yet scheduled:** an admin action and tray item to view one session's binding and drop it (redacted session key). The note's item 6 asks for this; it is useful for operators. Today the binding is dropped only on forced moves, at cold points or by the D15 rebalance.

<a id="d17"></a>
## D17. Retire CPA by adopting its accounts; owned accounts always refresh — Accepted (user direction, 2026-10-18)

**User direction (quoted).** "cpa 계정인수기능도 만들고요(더럽지않은 깔끔한방법으로 가능하면), 애초에 qb에 직접등록한계정도 자동 갱신되도록 해야합니다."

The design was independently reviewed (`.sandbox/reviews/D17-review.md`); all six must-fix items are folded in below.

- **Adoption is per account and runs in the lease-holding core** (`POST /accounts/{id}/adopt`, C11).
  - A linked account is adopted from CPA's file when the file exists and its identity matches.
  - When CPA no longer has the file, it is adopted from a newer CPA backup copy the user points at, or else from quota-baton's stored tokens. D5 already allows this.
  - `id`, bindings, quota history and ledger keys never change, and `cooling(link_stale)` becomes `ready`.
- **`retire-cpa` is a journaled, resumable, reversible sequence** (architecture §6). Its steps, in order:
  1. Disable CPA's tasks.
  2. Stop the CPA process, identified by pid file, path, listener and start time. This happens only outside CPA's own refresh window: `expired - now > lead + 5 min` (Claude 4 h, Codex 24 h), and `last_refresh` older than 60 s.
  3. Do the final read under the lease.
  4. Adopt each account.
  5. Make CPA's credential files unloadable: rename them to `*.json.retired` and add a marker file.
  6. Optionally run a verify-refresh.
  7. Report.

  A torn (unparsable) file after the stop is adopted from the store and is never restored to CPA. `retire-cpa` requires the clients to be wired to quota-baton first, so CPA has no traffic and therefore no 401-triggered refresh.
- **`restore-cpa` reverses it.**
  - It stops quota-baton's refresh for the adopted accounts.
  - It is refused while any of them has `refresh_uncertain=true` or a refresh still active.
  - It writes quota-baton's current token pair into each retired file. This is a read-modify-write that changes only the token and time keys and keeps CPA's timestamp shape.
  - It converts the accounts back to linked, un-retires the files, restores the tasks, and starts CPA through its task. Port 10100 must be free.
  - Accounts adopted without a CPA file stay owned.
- **Retired copies and CPA backups must never be loaded into CPA again** while quota-baton owns the accounts. A rotated refresh token presented twice is a reuse that can revoke the family (R1 §2, R2 §2, the 10-10 incident).
- **Owned and adopted accounts refresh ahead regardless of `user_state`**, disabled and paused included. Only `drain` and `needs_relogin` stop refresh. CPA kept disabled accounts alive the same way (D5 evidence).
- **A manual refresh** (`POST /accounts/{id}/refresh`, `quota-baton accounts refresh <id>`) makes refresh observable. It uses the same gate as refresh-ahead.
- **Applying `retire-cpa` changes CPA's auth directory and stops CPA**, so it is a user decision under D9, taken once per `--apply`.

**Evidence (2026-10-18).**
- The two active plus-plan Codex accounts (cx-413c3b, cx-763c46) were removed from CPA on 10-16 21:11 (CPA log `auth file changed (REMOVE)`).
- Nobody refreshes them, and their stored refresh tokens equal CPA's last state.
- They expire on 10-23 and 10-24, so they must be adopted from the store before 10-22, whenever the full retire ships.

<a id="d18"></a>
## D18. Take over an account from another app's home, with a warning — Accepted (user direction, 2026-10-19)

**User direction (quoted).** "경고하고(원래쪽 로그인된다는) 토큰 가져와서 관리주체 바꾸는 기능도 필요합니다." The first case is `acct-delta@mail.test` (Codex, Pro) in an Orca-managed Codex home.

- **What it does.** quota-baton reads the token pair from another app's home and becomes the account's **sole refresh owner** (`source.kind="takeover"`, owned like `oauth`). Sources:
  - a Codex home with a ChatGPT `auth.json`: `~/.codex`, `~/.codex-acct/*`, Orca `codex-accounts/*/home`;
  - a Claude config dir with `.credentials.json`.

  quota-baton has no Orca-specific code. The user points at a directory.
- **The warning is the point.** After quota-baton's first refresh, the source's stored refresh token is consumed. The source app will fail and must log in again. If it refreshes with the old token first, the reuse can revoke the whole token family (D5 evidence, the 10-10 incident). The command and the tray show the warning and the identity before anything is written:
  - email, plan, provider account id;
  - the source path and the source's `last_refresh` and file mtime;
  - whether a process that owns the home looks alive.

  The user confirms each takeover (`--yes` on the CLI, a confirm dialog in the tray).
- **Guards.**
  - Refuse while the source owner looks alive. Examples: an Orca process is running for an Orca home; a `codex` process runs with that `CODEX_HOME`, best effort by command line; the file changed in the last 120 s.
  - Re-read the file just before commit. Refuse if its bytes changed after the warning was shown.
  - Refuse an identity that is already in the store, unless `--replace` (same rule as `--replace-linked`). The existing `id`, bindings and history are kept.
  - Refuse API-key auth (D16).
- **Retire the source by default.** After a successful takeover, rename the source file to `<name>.taken-over-by-quota-baton-<date>` so the source app cannot present the consumed token again. It then shows "not logged in" rather than revoking the family. `--keep-source` skips the rename and gets a second, stronger warning. Renaming another app's file is a user decision under D9, made per takeover by the confirmation above.
- **Then verify.** Run one manual refresh (D17 gate) right after the takeover and report it, so a dead refresh token is found at once and not at expiry.
- **Not covered.** Reverse hand-back to the source app. To use the source app again, the user logs in there afresh.

**Evidence (2026-10-19).**
- Orca has no service, scheduled task, Run key or startup entry, and was not running.
- Its homes' `auth.json` were last refreshed when Orca was last used: 09-18, 09-30, 10-11 and 10-12.
- Orca refreshes only while it (or a Codex CLI in that home) runs. A takeover while Orca is closed is therefore safe, provided Orca is not started on that home afterwards. The source rename guarantees this.

## D19. Codex credit continuation: permission is not proof of funds — Accepted (design owner ratification, 2026-10-20)

**Context.** During the design pause (10-19) the implementer rewrote the C5 and C7 credit rules. No brief or decision covered it, but it followed the user's R4 and tray v2 direction: "유료 크레딧 사용 허용" means a Codex account keeps serving from credits once its subscription window is spent. A Sol max independent review covered it (`.sandbox/reviews/credits-logic-20261019/sol-max-final.md`). The design owner reviewed it on 10-20 (`.sandbox/reviews/pause-merges-review-2026-10-20.md`, MUST-FIX 2) and ratifies it as written in C5 ("Codex credit entitlement and subscription observations") and C7 ("Unusable stays narrow").

- **Permission and funds are separate.** `allow_credits` is the user's permission. Available credits need evidence: unlimited, `has_credits=true` or a positive balance, with no explicit absence. Alternatively, a successful paid response proves entitlement for the life of the process.
- **No subscription-only cooling with permitted credits.** A permitted Codex account with credits available is not cooled only because a subscription window reached 100 %. An explicit upstream quota rejection, `limit_reached`, and auth or payment holds still apply.
- **Real rejections are not undone by settings.** A hold caused by an actual upstream quota rejection is non-releasable (`quota_credit_releasable=false`). Turning credits on releases only holds that came from observations. This keeps a credit toggle from re-sending traffic into a real rejection.
- **Display needs proof.** "유료 크레딧 사용 중" is shown only after a successful paid response, never from a poll or a capability flag. Proof is not restored after a restart.
- **The 99 % preventive hold** applies only to credit-capable accounts with permission off.

**Follow-ups.** Record `quota_credit_releasable` in AccountView and the account file format (C11, C12).

## D20. Routing policies kept, default rebalance; no herding onto accounts with weak usage evidence — Accepted (user direction, 2026-10-20)

**User direction (quoted).** "기본값은 분산허용입니다 (사용량예측기능이 이미 있다면 분산허용이 올바릅니다). 다만 사용한지 얼마안된계정, 막 사용한계정의 사용량예측이 제대로안될 때 사용량이 적다는이유로 한계정에 여러세션이 몰리는문제를 회피하는 분명한 방법을 적용해주세요."

**1. Routing policies.** Keep the `impl/cache-routing` surface: `strict` / `recover` / `rebalance` per project and per root session, plus the admin API and the CLI. This supersedes the D11 §4 rule that drain is the only manual placement setting. The default is `rebalance`. A user who sets no policy MUST get exactly the D15/C7.1 behaviour, including the review fixes below.

**2. Anti-herding.** The problem: usage observations lag behind placements. An account that was just added, just reset or just observed low looks emptiest, so a burst of new sessions all land on it before any observation shows their cost.
- **Placement.** A placement is one of two things:
  - a new session group bound by C7 steps 3–6;
  - a group moved onto the account by C7.1 or by the cold-point rule.

  Steps 1–2, restored bindings and in-place retries are not placements.
- **As-of time.** An observation's as-of time depends on its source:
  - for response headers and WebSocket rate-limit events, the send time of the request that produced it;
  - for a poll, the time the poll started.

  An observation is newer than a placement when its as-of time is after the placement in the account's monotonic sequence. The same instant is not newer.
- **Placement charge.** Each placement adds `placement_allowance_pct` (default 2 points) to that account's projected usage on every gating window. It adds on top of the in-flight allowance.
  - The charge for a window stays until an observation of that window newer than the placement arrives. A response to a request sent before the placement never clears it.
  - The charge is used only for placement, like the in-flight allowance (C7). It never cools an account and never moves a bound session. The cold-point gate check (C7) uses projected usage without the charge.
- **Confidence cap.** An account's usage evidence is **weak** when any gating window has:
  - no observation yet;
  - an observation older than the account's latest placement (an account that has just been used);
  - or a passed `reset_at` with no new observation.

  An account is **capped** when two things hold:
  - its evidence is weak;
  - `placed_since_obs` is at least `weak_evidence_new_sessions` (default 1). `placed_since_obs` counts the placements newer than the oldest observation among the account's gating windows. While a window has no observation, every placement counts.

  Where the cap acts:
  - It applies in C7 step 4 (both branches), in step 5 and in the C7.1 move.
  - It applies after the room partition and the pacing skip. Room (C7.1) is decided before the cap.
  - Uncapped candidates come first, in the C7 order. When every candidate in the set is capped, order them by the fewest `placed_since_obs`, then by the C7 order.
  - A capped account is never a refusal.
- **Fresh evidence soon.** 120 s after any placement, check whether every gating window of the account has an observation newer than that placement. If not, run one usage poll for that account.
  - This poll has its own budget: one per account per 5 min, independent of `poll_min_interval_secs`.
  - It respects the poll backoff and the C9 exclusions.
  - Polls stay read-only GETs with honest headers (C9, C10).
- **Ledger.** `placement` records carry `evidence: fresh|weak`, `capped`, `placed_since_obs` and the charge applied. The triggered poll logs `poll{reason: placement_evidence}`. Together they measure herding and the poll cost.
- **Config keys** (C12): `placement_allowance_pct = 2`, `weak_evidence_new_sessions = 1`, `placement_poll_after_secs = 120`, `placement_poll_min_interval_secs = 300`. The counters, `last_placement_at` and the poll budget live in memory, like the C7.1 marks, and are not exported on takeover.
- **Limits** (from the review `.sandbox/reviews/D20-anti-herding-check-fable.md`):
  - The charge reorders only accounts that are tied on both resets. It mainly matters at the gate.
  - A busy account that is first in the C7 order refreshes its evidence on every response, so it keeps taking sessions up to `spread_threshold`. That is D11 plus D15, and D20 does not change it.
  - The cap bounds a burst: within one observation cycle no account is more than one placement ahead of another.

**3. Review fixes the design owner decided (no user input needed).** References are to `.sandbox/reviews/cache-routing-review-fable-2026-10-20.md` §2.
- **M5, D15 cure for idle targets.** Drop the requirement of a committed success within 300 s. A rebalance target needs eligibility, the gate, room, and C7 non-stale observations. A target with weak evidence takes at most one moved group (§2) until it is observed. If every target with room is capped, the group stays and keeps its mark.
- **M6, rate-hold expiry.** Keep D14 and C6.3: when the 60 s rate hold expires, send once. A locally synthesized 429 is not adopted (C7: projections never synthesize a 429). Local hold expiries do not count toward the C7.1 overload trigger.
- **Churn guards.** The dwell, revisit, group, provider and cost guards are kept as defaults for **discretionary** moves only (rebalance and cold point). Forced moves (C5 ineligibility) are never blocked or budgeted (M3).
- **Availability.** Policy-file failures deny dispatch only to `strict` sessions (M1, M2).
