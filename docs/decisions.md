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
- Tampered items on the minting account: 400. Along with the token-count comparison, this supports validation and dropping in the tested cases; it is not a universal rule about foreign items.
- The tested moved sessions completed, with the first turn cache-cold and earlier hidden reasoning apparently unused. This observation does not guarantee that every later model/account/path or compaction item can resume without loss or rejection.

**Evidence scope clarified, 2026-10-20 (no relay behaviour change).** Local transcript resumption, cache reuse and preserved reasoning are distinct. Current official Claude API documentation describes separate model, account and prefix checks. Those statements do not establish subscription OAuth or Codex behaviour. The [routing design supplement](account-routing-design.md) records sources and routing rationale. Routing controls and churn defaults are accepted under D20; forecasting remains observation-only. D10 continues to require verbatim relay, not stripping or rewriting opaque history to force a hand-off.

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
   - **Lower-affinity heuristic:** after at least 1 h idle (this does not establish cache expiry on every account), but only if the current account fails the gate (D15 adds: or is at its spread threshold). V2 confirmed Codex's objection: the tested move kept that conversation working but did not reuse its earlier hidden reasoning, so a move that is not needed is avoided.
   - **Overload (added by D15):** a bounded rebalance at a request boundary when an account is concentrated and another has room.
3. **Rejected:** restoring immediately when a preferred account recovers (unanimous), and moving warm sessions early to use expiring weekly quota (unanimous). New-session placement serves that goal without a cache cost.
4. **No priority list.** Per-account **drain** rejects new sessions while existing sessions stay. D20 supersedes the old drain-only restriction with project/root `strict`, `recover` and default `rebalance` policies. `allow_credits` remains separate and off by default.
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
1. quota-baton serves only the user's **subscription OAuth accounts** (D5: its own login, or a CPA OAuth import). It never imports or uses API keys, so no silent OAuth-to-API-key fallback can exist. This does not make OAuth usage inherently free: a Codex OAuth account can have separately purchased credits. C5's explicit credit permission and observations apply; the local 99% preventive hold is not an upstream-enforced zero-spend guarantee.
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
- **No subscription-only cooling with permitted credits.** A permitted Codex account with credits available is not cooled only because a subscription window reached 100 % or a usage snapshot reports `limit_reached=true`. The design owner corrected this snapshot-only rule after the 2026-10-21 outage (R12.9 Update 2, hotfix `77ba7f8`): the flag alone must not create a hard hold. An actual upstream request quota rejection, rejected status, and auth or payment holds still apply.
- **Real rejections are not undone by settings.** A hold caused by an actual upstream quota rejection is non-releasable (`quota_credit_releasable=false`). Turning credits on releases only holds that came from observations. This keeps a credit toggle from re-sending traffic into a real rejection.
- **Display needs proof.** "유료 크레딧 사용 중" is shown only after a successful paid response, never from a poll or a capability flag. Proof is not restored after a restart.
- **The 99 % preventive hold** applies only to credit-capable accounts with permission off.

**Follow-ups.** Record `quota_credit_releasable` in AccountView and the account file format (C11, C12).

<a id="d20"></a>
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

### D20 §4. Merge-review rulings (design owner, 2026-10-20)

These come from the R8 B/C merge review (`.sandbox/reviews/R8-BC-merge-review-2026-10-20.md`).
- **F1, rate-hold expiry everywhere.** "Send once" (D14, C6.3) applies to every hold that ends a request's wait: the initial admission, HTTP retries and WebSocket resends or redials. Restore the dev tests by name and body. C6.3 keeps its dev wording.
- **F2, the destination-quiet guard is removed.** The D20 §2 cap does the same job better: a weak target takes one group until it is observed. A veto by any remaining discretionary guard keeps the overload mark and never converts it to a forced move.
- **F4, deadlines ratified.**
  - **The 300 s pre-commit deadline (504) is ratified.** It stays under Claude Code's 600 s client timeout, and it applies only before response headers.
  - **The 10 s WebSocket envelope is ratified.** It includes refresh and dial, and stays under Codex's 15 s connect timeout.
- **F6, narrowings ratified.**
  - **Cold-point targets.** A cold-point move needs a target that passes the gate, has room and has known quota. The move is discretionary; when no such target exists, the session stays.
  - **Room.** Pending mark reservations count against room.
- **Forced moves are placements.** A group moved off an ineligible account (C5) is a placement on its new account. It takes the charge, and target choice applies the cap. The cap is never a refusal, so a forced move always proceeds when any eligible account exists.
- **Polls.** When the placement-poll budget refuses a poll, the regular poll schedule still runs.
- **Config bounds.** A value outside its bounds logs a `core_warning` and is clamped. It never refuses startup. Unknown keys are ignored with a warning, in every table.

## D21. codex-qb keeps a separate Codex home; existing sessions are brought in by hardlink import, automatically on resume — Accepted (user direction, 2026-10-21)

**User direction (quoted).** "기존폴더에서 찾아 자동으로 가져온뒤 여는 기능을 넣죠. 복사도 안하고요". Before that the user had asked whether codex-qb can share conversations with the normal `codex` at all.

**Problem.** `codex-qb` runs Codex with `CODEX_HOME=~/.codex-acct/qb` (R3). That home holds the qb base URL and an API-key auth.json carrying the qb client key. Codex keeps settings, login, rollouts, thread state and goals in one home, so sessions started with `codex`, `codex-mz` and the like are invisible to `codex-qb`. `claude-qb` has no such problem: Claude Code accepts `--settings` per process, so it shares `~/.claude` (R3).

**Options studied** (`.sandbox/research/codex-shared-home-qb.md`, a Codex 0.163 source read with file:line citations):

1. **Shared home with a custom provider id (`qb`).**
   - **How it would work.** The process uses `-c model_provider=qb` with `model_providers.qb.env_key` holding the client key. Auth works per process, and the home's auth.json stays untouched.
   - **Rejected.** Codex records the active provider on a thread when it resumes it.
     - **Provider flip.** A thread resumed once in codex-qb becomes provider `qb`. The normal `codex` then hides it from its picker and fails to resume it by UUID ("Model provider `qb` not found").
     - **Split picker.** codex-qb's picker shows only threads whose provider is `qb`.
   - **Other costs.** The approach also depends on Codex internals: the `name=="OpenAI"` behaviour switches, request compression and the picker filter. Those can change in any Codex release.
2. **Shared home keeping the provider id `openai`.**
   - **Rejected.** The TUI has no per-process API-key override (`CODEX_API_KEY` is honoured by `exec` only), and built-in provider ids cannot be redefined.
   - **What it would cost.** It would mean sending the home's ChatGPT token to qb, routing ChatGPT-only request paths through qb, and changing qb's client-auth contract (C3, C11).
3. **Separate home plus import (chosen).**
   - **Why it works.** The qb home already uses the built-in `openai` provider, with `openai_base_url` and API-key auth. An imported thread therefore keeps provider `openai`.
   - **Hardlinks, no copies.** The rollout files are hardlinked, so the import copies nothing and costs no disk space. Appends made in either home are visible in both.
   - **Two-way.** The session can go back to `codex-mz resume <id>`.
   - **Import contents.** Only the thread metadata rows and the goal row are copied into the qb home's databases (R9).

**Decision.**
- **No copies.** `codex-qb` keeps its separate home. Rollouts are never copied; they are hardlinked. A cross-volume source is refused rather than copied.
- **Auto-import.** `codex-qb resume <uuid>` imports a thread that is missing from the qb home before it starts Codex. It searches `~/.codex`, `~/.codex-acct/*` and Orca homes, prints one Korean line naming the source home, and refuses a live session with the reason.
- **Manual and bulk import.** `quota-baton sessions import` supports both, with a dry run by default, a journal and `unimport`.
- **Provider check.** A thread whose provider is not `openai` is refused.

**Concurrency findings** (`.sandbox/research/codex-session-concurrency.md`, read from the Codex 0.162.1 source; the 0.163 binary contains the same lock names and error string):
- **The lock is a file inside the home.** Codex refuses a second writer only within one home: it holds an OS lock on `<home>/thread-writer-locks/<id>.lock`. A second resume in the same home gets "already has an active writer" and drops to a read-only view. The rollout file itself is never locked; it is opened with full sharing.
- **No guard across homes.** Two processes in different homes that share a hardlinked rollout both append silently. Their line counters collide, and Codex drops the losing writer's turns from history without any warning. **One writer at a time is therefore a hard rule.**
- **Some paths split the hardlink.** Each writes a new file or renames one in one home only, so the homes diverge:
  - revert/backtrack (on by default);
  - cold compression to `.zst`;
  - `migrate-rollouts`;
  - archive and delete.

**Rules that follow (R9).**
- **Before every import and every `codex-qb resume`, check that no process is writing the session.**
  - **Rollout probe:** open the rollout for read with write sharing denied. A sharing violation means a live writer. This works through hardlinks.
  - **Lock probe:** try Codex's thread lock in every home where the thread exists.
  - Command lines and recent writes are hints only.
- **Before reusing an imported session, run a divergence check.** Both names must have the same file id, both homes' `rollout_path` filenames must match, and no `.zst` file may exist beside the rollout. On divergence, refuse and explain; never merge files.
- **What to link and what not to.** Link every file of a paginated thread. Never hardlink sqlite files or `session_index`. Do not copy `thread_history_1` rows; the target rebuilds them.
- **Remaining gap.** Opening a qb session from an old command while qb is running it is not caught by qb. That is the old command's own launch, which qb does not wrap. Possible fix: point every home's `thread-writer-locks` at one shared directory, so Codex's own guard covers all homes. That changes the user's existing homes, so it needs a synthetic test first and then the user's approval.

**Do not revisit without new evidence.** The shared-home options above were rejected on source evidence (Codex 0.163). Re-open them only if a Codex release lets resume keep a thread's provider, or adds a per-process auth override for the TUI.

### D21 amendment: move, not share (user direction, 2026-10-21)

**User direction.** The user asked: "codex-qb로 지정한 세션(codex꺼든 orca꺼든 codex-mz꺼든 codex-ax꺼든... 범용적으로) 재개하기 (재개하는 과정에서 처리 필요한 문제들 e.g. 마이그레이션 후 검사 후 문제없으면 기존것 삭제 등. 일괄 처리)". The day's manual migrations removed the source copy once the session opened in its new home, and the user confirmed that is the wanted behaviour.

**Change.** Import is a **move** by default:
1. **Import.** Hardlink the rollout files and copy the rows, exactly as in D21.
2. **Verify.** Run all of these checks:
   - the target and source names share the same file id;
   - the target thread row points at the target path;
   - the goal row matches;
   - the divergence check passes;
   - no live writer exists.
3. **Remove the source.** On success, remove the session's own rollout name or names from the source home, its `threads` row and its `thread_goals` row. Do not remove fork ancestors: other threads in the source home may still fork from them, and the extra links cost no space. Do not edit `session_index`, which is append-only.

**Why.** After the move only one home can open the session. This closes the cross-home double-writer gap described in the concurrency findings, without changing the user's existing homes. The journal still allows `unimport`, which re-links the source name and restores its rows while the file exists.

**Two-way sharing** remains available with `--keep-source`, together with the warnings above.

### D21 panel: move is the default, confirmed (independent panel, 2026-10-21)

**Mandate.** The user asked the design session to decide between move and share through independent review by Opus max and Astra max (gpt-6-astra, max). The user then challenged the argument that sharing relies on a promise never to open a session twice: Codex's own lock could enforce that if every home shared one lock directory.

**Evidence.** All files are in `.sandbox/reviews/d21-move-vs-share/`:
- `sheet.md` is the blind sheet.
- `answer-opus.md` and `answer-astra.md` are the round-1 answers. `answer-opus.md` also contains the revision after the shared-lock input. `answer-astra-r2.md` is Astra's round 2.
- `lock-experiment.md` is the real Codex 0.163 run with two temp homes and one hardlinked rollout:
  - With separate lock directories, **both homes opened the session**, giving two live writers.
  - With both homes' `thread-writer-locks` junctioned to one directory, the **second open was refused** by Codex itself ("This conversation is open in another app").
  - A first run was invalid, because the herdr codex shim overrides `CODEX_HOME` from its session map. Experiments must call the native binary.

**Outcome.** Both panelists chose **A, move**, in both rounds. Both accepted the user's point: a shared lock directory makes concurrent opens safe, and the "user promise" objection is withdrawn. The deciding reason changed to **sequential consistency**, which no lock provides:
- **Backtrack splits the hardlink.** A backtrack in one home replaces that home's rollout. A later launch from another home then resumes the obsolete file, and the lock is free by then.
- **Separate databases go stale.** Goal and thread rows live in per-home databases. Q cannot share the state database (provider id, D21), and qb never sees launches from the account homes.
- **No shared meaning for archive and delete.** These operations act on one home's names only.

Moving keeps a single authoritative home. The shared lock directory is useful only as **optional hardening during the move's commit window**. It is not adopted now: it changes the user's existing homes, and Codex's per-home managed app-server daemons hold the lock directory open.

**Guarantees R9 must meet.** These merge Opus G1–G11 and Astra's seven acceptance criteria:
1. **Single owner.** After any operation or recovery, exactly one home holds the session's rows. Any other name is a fork-ancestor link with no row.
2. **Inventory first.** Check every known home (the default home, `.codex-acct/*`, Orca) for the id. Refuse conflicting versions: different file ids with different content, or different goal state. List the homes in the refusal. Never pick by timestamp, and never merge.
3. **Never nameless.** Link before unlink. A journal rolls forward or back after a crash at any step. At every interruption point, at most one ordinarily writable location exists.
4. **Exclusion through commit.** Probe first (a deny-write open, then the lock probe in every home). Then hold Codex's thread lock in the source home and in Q until commit.
5. **Verify, then remove.** Before removing the source, all of these must hold: the same file id, the target row pointing at the target path, the goal row matching, no divergence and no writer. If any fails, abort with the source untouched.
6. **Ancestors kept.** A rollout other threads fork from keeps its name in the source home.
7. **Stale paths fail closed.** After a move, resuming in the old home modifies nothing.
8. **Return.** The return command moves Q's **current** rollout and goal state back to an account home under the same rules, and writes nothing from an old snapshot. It works with the proxy stopped.
9. **Bulk.** Each session moves atomically. Live and divergent sessions are skipped and listed. A re-run recognises sessions already committed and sessions still pending.
10. **Safe databases.** Use transactions with a busy timeout. Never replace a database file. Only append to `session_index`. Run `integrity_check` in tests.
11. **Auto-resume follows ownership.** The herdr session map (`%APPDATA%\herdr\session-homes.json`) is updated under the same journal and points at Q after a move. An interrupted move never resumes a fresh session.
12. **Disk.** Hardlinks only. A move that cannot hardlink is refused.

### D21 amendment: a move needs a typed confirmation (user direction, 2026-10-21)

User: "이동은 강제로 시키는 게 아니라 터미널로 사람이 실수 안 하게끔 입력받을거죠? (e.g. move라고 친다거나)".

- **The resume launcher asks before it moves anything.** When `codex-qb resume <id>` finds the session only in another home, it shows what would happen, in Korean:
  - the source home;
  - the rollout size;
  - the goal status;
  - the fork ancestors that stay in the source home;
  - that the session will be removed from the source home;
  - the return command.

  It then asks the user to **type `move`**. Any other input cancels and changes nothing. The launcher may also offer `share` (the `--keep-source` mode), with its warning.
- **No terminal, no move.** Without an interactive terminal it refuses and prints the manual command. It never moves silently.
- **Manual and bulk commands follow the same rule.** `quota-baton sessions import ... --apply` and the return command ask for the same typed word. A bulk run shows one table and one typed confirmation for the whole batch. `--yes` skips the prompt for scripted use only; the launcher never passes it.

### D21 panel 2: guarantees against raw native Codex runs (2026-10-21)

**Question.** The R9 feasibility study (`.sandbox/research/R9-panel-feasibility.md`) found two counterexamples with the real Codex 0.163 binary:
1. A moved root S whose source name is kept for a child T is resumed by raw `codex resume S` in the source home. Codex rebuilds S's row and appends to the shared file.
2. If the importer is killed while both homes have public names, its locks are released and raw resumes succeed in both homes.

**Panel.** Sheet and answers are in `.sandbox/reviews/d21-raw-scope/`. Opus max ran via claude-qb and Astra max via codex-qb. In round 1 they disagreed: Opus chose an amended C, Astra chose A with staging as defence. Per the user's rule, each was sent the other's answer in its own session. In round 2 they **converged**: Astra adopted refusal of moves with descendants, and Opus adopted Astra's precise contract and boundaries.

**Decision (consensus).**
- **Staging protocol.**
  - Link into a non-discoverable staging path in Q (Codex discovers only `sessions/` and `archived_sessions/`).
  - Verify. Then remove **every** discoverable alias of S in the source home: the names first, the rows second. Then publish S into Q. Ancestors are published before S, and the multiplexer map is updated at the commit point.
  - Before removing a name, open the file through it with sharing denied and delete it through that handle. A sharing violation means another process holds the file, so wait or refuse.
  - Never remove a file's last name. Refuse when the hardlink count exceeds the names found in the source home, Q and staging.
- **Descendants block the move by default.** Find descendants (forks and history-base children, transitively) by reading rollout headers, not sqlite rows, and refuse in Korean if any exist. Moving a whole family is deferred; it needs its own protocol and its own decision. Late forks are tested. If one cannot be excluded, it must fail closed and `qb doctor` must report it.
- **A Q-side ancestor alias that already exists is refused.** S may already have a discoverable name in Q because it is an ancestor of an earlier move. The move is then refused with an explanation, and no exception is made.
- **Contract wording.**
  - **G1** holds at successful completion and at completed recovery.
  - **G3** holds continuously, including the interval between a crash and recovery. While a journal is open, every managed entry point refuses its ids or runs recovery first, even after the OS locks are released.
  - Recovery after the commit point only rolls forward; rollback and undo go through staging.
  - If recovery finds conflicting rows, appends the journal did not record, or unknown names, it keeps the evidence, makes no destructive change and does not report success.
- **G7** is reworded to: a resume of a moved id in the old home leaves the moved rollout and rows unchanged. Auxiliary files created by native startup are allowed, and a plain raw startup need not fail.
- **Managed enforcement must cover every resume path:** explicit resume, `--last`, the session picker, automatic resume and the per-home daemon. If ancestor-only ids cannot be blocked on one of these paths, refuse the affected moves.
- **Raw-native claims are evidence-bound.** For moved threads, qb claims protection against raw native runs only for scenarios that pass the protection tests on the pinned Codex version (Opus R1–R3b and R10–R12 in `answer-opus-r2.md`). Counterexamples 1 and 2 must pass as **protection** tests. The raw resume of a Q-side ancestor, and the stale-path case if R3b fails, are documented boundaries reproduced as **boundary** tests.
- **Version gate.** Moves are disabled on a native Codex version that has not passed the suite.
- **`qb doctor`.** It detects rows in the wrong home, staging files with no journal, unreferenced Q ancestors and descendants that were missed. It recognises intentional ancestor aliases, so completed moves are not flagged.

## D22. The account is chosen at codex-qb launch, and the model catalog is that account's (resolves O13) — Accepted (user direction plus panel, 2026-10-21)

**User direction (quoted).**
- "1번으로하면 모델 변경시 계정변경으로 캐시가 깨질수있지 않나".
- "애초에 codex-qb 실행 시 계정이 결정되고 그 계정에따라 models이 보일순 없는건가".
- From the dashboard design tab: "세션 시작할 때 원하는 계정으로 시작가능하게 … 모델을 지정해서 시작하면 그 모델을 지원하는 최적계정으로 연결".

**Trigger.** `/daybreak` in codex-qb reported "not available for this account". Codex enables it only when its model catalog contains a daybreak model (`tui/src/chatwidget/slash_dispatch.rs:192-204`). qb passed `/models` through one account, acct-d, which lacks it.

**Panel.** Files are in `.sandbox/reviews/o13-launch-account/`. Opus max (via claude-qb) and Astra max (via codex-qb) each answered twice, after exchanging answers.
- Opus held A′: a union catalog with (session, model) bindings and affinity.
- Astra held B: a launch-scoped account.

Both agreed on these points:
- one authoritative binding store;
- `/models` never places or moves anything;
- entitlement maps;
- an explanation before an avoidable unsupported-model request;
- initial placement uses the **effective** startup model;
- concurrency tests.

The disagreement narrowed to two facts, which settle it:
1. **B's catalog isolation holds.** Codex's model cache is keyed by a provider/auth identity that **includes the catalog URL** (`model-provider/src/models_identity_tests.rs` `cache_identity_tracks_catalog_url`, `models-manager/src/cache.rs`). A per-launch base URL therefore never shows another launch's cached catalog. This was Opus's main objection to B, and Opus said B is acceptable if isolation holds.
2. **Cross-account hops are not free.** Evidence V2 (D10, 2026-10-13): encrypted reasoning items made on one account are silently ignored on another, so a move drops earlier hidden reasoning. Opus's own condition was G10, "no stripping, and portability holds". That fails, so his rule falls back to `consent`, which behaves like B. It also matches the user's account-first priority.

**Decision: B, with Opus's corrections.**
- **Launch token.** `codex-qb` obtains a launch token from qb and passes `-c openai_base_url=http://127.0.0.1:<port>/l/<token>/v1`. The token is a routing handle: local client-key authentication still applies, the token is unpredictable, it is redacted from logs, and it expires when idle. The launcher also passes a per-launch `model_catalog_url` if `openai_base_url` alone does not change the cache identity. The source test proves this for `model_catalog_url`, and `cache_identity_tracks_provider_routing…` suggests the base URL counts as well. Release gate: Astra's alternation test (launch on A, then B, then A again, with warm caches and concurrent launches) checks the picker contents and `/daybreak` with the real 0.163 binary.
- **Placement at launch.** Among usable accounts that support the effective startup model (`-m`, else the configured default), choose with the normal C7 and D20 rules. `--account <name>` is a preference inside that set. On `resume <id>`, the existing binding wins.
- **Catalog.** `/models` through a token returns the catalog of the account the session is bound to. qb keeps per-account entitlement maps, refreshed periodically and on "model not supported" responses.
- **Model switches.** Advertised models always route to the bound account. A manually entered model the account lacks gets a clear explanation without an upstream request. Account shopping is never triggered implicitly.
- **Forced moves** (account unusable). Move by current-model compatibility, preferring destinations that support the whole advertised catalog. A later pick that the new account lacks gets the same explanation. Never break a working session to keep the menu accurate.
- **Resume with an explicit `-m` the binding lacks.** It is not refused outright: qb explains, and `--account <name>` or `--move-for-model` is the user's explicit consent to move the session.
- **Unknown or expired tokens** fail explicitly only where the token is needed: `/models` and placing a new session. Requests that carry a bound session key route by the binding.
- **Gap logging.** Every placement and move made for a model choice is logged and shown in the panel.
- **Not chosen:** a union catalog, and routing each request by model.

## D23. Image generation through qb: a separate image home Qi with a custom provider, and qb relays the images routes — Accepted (user direction plus panel, 2026-10-21)

**User direction.** "codex-qb에는 이미지 도구가 없습니다 → 이건 우리가 빨리 지원하고 (모델 지정처럼 이미지젠 지정 시 지원하는 계정으로 잡아주도록)". Any imagegen model is fine (gpt-image-2).

**Facts.** The evidence is in `.sandbox/research/codex-image-tool-qb.md` plus a live tool listing. Codex exposes `image_gen__imagegen` only when one of two conditions holds:
- ChatGPT login together with `requires_openai_auth=true`;
- a provider with `requires_openai_auth=false` and a non-empty `x-openai-actor-authorization` header.

Built-in provider ids cannot be redefined. The tool POSTs to `<base>/images/generations|edits` with `gpt-image-2`, and qb relays neither route today.

**Panel.** Files are in `.sandbox/reviews/qb-imagegen/`. Opus max (via claude-qb) and Astra max (via codex-qb) **agreed in round 1**: option C. Both rejected synthetic ChatGPT auth (B), because it forges plan claims and risks refresh calls against the irreplaceable account.

**Decision.**
- **The Qi home.** `codex-qb --images [resume <id>]` runs in a separate home Qi. `wire` generates Qi's config from Q's config plus one provider block, so the two cannot drift. The block (`model_provider="qb"`) contains:
  - `base_url` set to qb;
  - `env_key` set to the client key;
  - `requires_openai_auth=false`;
  - `http_headers.x-openai-actor-authorization="qb-local"`, a local compatibility sentinel, honestly described as such;
  - `wire_api="responses"`, `supports_websockets`, `name="OpenAI"`, and remote compaction as in the shared-home study.
- **The default Q stays on the built-in `openai` provider.** Ordinary sessions are unaffected.
- **qb relays `/v1/images/generations` and `/v1/images/edits`** to the selected account's `…/backend-api/codex/images/*`.
  - Bodies are relayed byte for byte (multipart included).
  - `x-openai-actor-authorization` is stripped in every letter case and every duplicate, and the local client key never goes upstream. The sentinel never authorizes access to qb; the client key still does.
  - Upstream authentication is the selected account's real token.
- **Image account selection.**
  - Only non-Free accounts that are not marked image-exhausted are eligible. The session's bound account is preferred.
  - On an image 429 or quota error, that account is marked image-exhausted until its reset and each other eligible account is tried once.
  - Image traffic never changes the session's binding for responses.
  - Edits that reference a server-side image stay on the account that produced it.
  - A 401 or 403 on an image route only marks the account image-incapable. It **never** deletes, rotates or force-refreshes a token (acct-d has one irreplaceable login).
- **Provider-id round trip.**
  - Moving a thread into Qi sets the copied row's provider to `qb`; moving it out sets the destination's provider (`openai`). The R9 mover gains a destination-provider rule, and an unknown provider aborts the move with the id named.
  - Live test: verify on the real 0.163 binary whether resume reads the provider from rows only, or also from the rollout's session meta or turn context.
  - **Rows only:** keep hardlinks.
  - **Rollout too:** stop and return to the design session. Copying would reopen D21's "never copy", and that needs its own decision.
- **Failure messages** distinguish no eligible image account, bound account unavailable, auth rejected, quota exhausted, endpoint unsupported, and transport errors. Each says whether it was generation or editing, and they are OpenAI-shaped so the Codex tool result shows them.
- **Acceptance gates.**
  - One real generation and one real edit through qb. They cost a little image quota, which is acceptable.
  - The tool is listed in Qi.
  - The full round trip account → Qi → account → Q → Qi → account, with a turn at each stop.
  - Header-strip capture tests.
  - An upgrade canary: on a Codex version change, the launcher checks that Qi still lists `image_gen__imagegen` and points to `codex-mz` if it does not.

### D23 amendment: ship new Qi sessions first; probe the resume path (design owner, 2026-10-21)

**R12 hit the D23 stop** (`.sandbox/reports/R12.md`, checkpoint impl/r12 d58e630). Resuming an existing thread in Qi with native Codex 0.163 rewrote the thread's database provider from `qb` back to `openai`, taking it from the rollout. The next request then carried no actor header and offered no image tool. No rollout was copied or rewritten.

New Qi sessions passed a real built-in image generation and a local PNG edit through a spawned synthetic core.

**Rulings.**
1. **Ship new Qi sessions now.**
   - `codex-qb --images` starts a **new** session in Qi with the image tool.
   - `codex-qb --images resume <id>` and moving into Qi are **disabled**. They print, in Korean: "기존 대화는 이미지 모드로 옮길 수 없습니다. 새 이미지 세션을 여세요".
   - This needs the full gate and the real-profile `launch-client --check`, then a merge. The design session installs it and runs one live generation.
2. **Probe authorized**, bounded and synthetic. In temp homes, using the native binary against a fake upstream, test whether `codex -c model_provider=qb resume <id>` (and/or `-c model_providers.qb…` supplied on the command line) keeps provider `qb` for a resumed thread and exposes the image tool. Report the result.
   - **If it works:** re-enable resume and move into Qi through the launcher, with that override, under all D21 guarantees.
   - **If it does not:** existing sessions stay out of Qi.
3. **Copying stays prohibited** (D21).

## D24. Per-account burn rate and projected exhaustion, display only; the dashboard window uses the tray icon — Accepted (user direction, 2026-10-22)

**User direction.** "계정별 소진속도랑(시간기준을 어떻게할지가 중요) 예상소진까지 걸리는 시간, 예상소진시각 등을 알수있으면 좋겠네요" and "대시보드창의 아이콘도 트레이아이콘처럼 일관성있게".

**Facts.**
- Providers report usage per window (5h, 7d; Codex gives `used_percent` with `limit_window_seconds` and a reset time). Inside one window the value only rises, then falls at the reset.
- Codex reports whole percents, so a short span can show no change even under load. Polls run at most every 5 min per account (`poll_min_interval_secs = 300`); response headers add samples between polls.
- `routing/forecast.rs` is an opt-in shadow diagnostic with a 15-min horizon. It is never shown and never used for placement.
- The dashboard window class registers no icon, so Windows shows its default. The tray icon is drawn in code (`tray_icon/win_icon.rs`).

**Decision: the time basis.**
1. **Wall-clock time, not active time.** A window resets on the clock, so exhaustion has to be predicted in clock time. Idle minutes inside the measuring span correctly lower the rate.
2. **The recent rate** is the change in used percent over the **last 30 minutes** of wall-clock time, divided by 30 minutes. The start value is the sample at or interpolated just before now − 30 min.
   - 30 minutes is 10% of a 5h window and covers at least 6 polls.
   - Shorter spans are dominated by the whole-percent steps. Longer spans react too slowly when qb moves sessions between accounts.
   - Inside the first 30 minutes of a window, or after a restart, the span starts at the first sample instead. It needs at least 10 minutes and a change of at least 2 steps; until then the value is "측정 중".
3. **The window average** is used percent divided by the time elapsed since the window started (reset time − window length). It needs no history.
4. **Which basis gives the projection.**
   - **5h windows** project with the recent rate. The window average is a fallback only while the recent rate is unavailable.
   - **7d windows** project with the window average. Extrapolating 30 minutes of work over days ignores nights and idle days. The recent-rate projection is shown beside it as a second figure ("지금 속도면").
5. **Low confidence.** When the number of sessions bound to the account changed inside the measuring span, the estimate is marked as changing. That happens when qb placed sessions on the account or moved them away.

**Decision: outputs.** For each account and window:
- the rate in %/h;
- the time to exhaustion;
- the projected exhaustion clock time in local time (with the date when it is not today).

When the projection falls at or after the reset, it shows "초기화 전 소진 없음" with the reset time. An exhausted window shows "소진됨" with the reset time. Per account, the window that runs out first is named. For accounts serving from permitted credits, the line says that service continues on credits after exhaustion. If the credit balance is observed over time, the same method gives a credit burn rate and a projected credit exhaustion time.

**Boundaries.**
- **Display only.** Placement, health and D20 anti-herding are unchanged; feeding these numbers into routing would need its own decision.
- The numbers are estimates, labelled as such, never promises.
- They are computed once in qb-core and exposed through the existing status API, so the dashboard and CLI show the same values.

**Decision: icon.**
- The dashboard window uses the same generated icon as the tray for its title bar, taskbar and Alt-Tab (big and small sizes). It follows the theme and the current state badge, and is updated when the tray icon changes.
- Both executables embed an `.ico` resource made by the same pixel generator, so Explorer and shortcuts show the same glyph. A test checks that the embedded icon matches the generator.

**Order.** R13, right after R11 lands. The frozen dashboard-polish backlog stays frozen apart from these two user-directed items.

## D25. Hold instead of moving when the limit resets within the prompt-cache lifetime — Accepted (design owner, 2026-10-22; prompted by a user question)

**User question.** "클로드 5시간한도 도달했다고 다른계정으로 옮겨서처리하면 캐시미적중으로 비용엄청나오지 않을지? 긴급세션개념 도입해서 긴급세션아니면 기본적으로 5h한도같은건 기다리게하면 안되나요". Also: "codex, claude 둘다 세션관리방법이 다르다면 다르게 동작해야합니다", and the prompt cache is isolated per account and cannot be migrated.

**Facts.**
- The prompt cache belongs to the account (organization) on both providers and cannot be moved. Our V2 evidence: the first turn after a move is cache-cold.
- The cache does not survive a long wait either. Claude's cache lives 5 min (1 h with the extended-TTL marker), and Codex's cache retention is short and not published. A 5h limit usually resets tens of minutes to hours later. After such a wait, the first turn on the **same** account is cold as well. Moving and waiting then cost the same single cold turn, and waiting adds idle time.
- Waiting saves the cold turn only when the reset falls inside the cache lifetime. C7 holds a request only when the limit resets within 60 s.
- **Codex differs from Claude.** A cross-account hop also drops earlier hidden reasoning (D22, V2), so a Codex move costs some quality on top of the cold turn. A project or root session that should never move already has the D20 `strict` policy.

**Decision.**
1. Replace the 60 s hold with a **cache-lifetime hold**. When a live session's account hits a limit that resets within the hold bound, qb holds the request until the reset instead of moving.
   - **Claude:** the bound is 5 min, or 60 min when the request body uses 1-hour cache markers.
   - **Codex:** the bound stays 60 s until the ledger shows how long Codex caches survive. Measure `cached_tokens` on the first turn after idle gaps of 1, 5, 15, 30 and 60 min on the same account, then set the bound to the longest gap that still hits.
2. A hold must not break the client. Before raising any bound, test how long Claude Code and Codex wait for a held request (response headers or the first stream event). Cap every bound below the shortest client timeout found, with a margin. A hold that would exceed it moves instead.
3. **Long waits.** No change: a session moves when forced (C7). The cost of moving equals the cost of waiting, and qb spreads forced moves (D15/D20) so one exhausted account does not cascade onto another.
4. **The "urgent session" idea** does not pay off for the cache, for the reason in the facts. It does pay off for **money**: an account serving from paid credits. That choice is the user's and is asked separately; until answered, `allow_credits` stays per account as it is.

**D24 amendment (user, 2026-10-22): the active-session count is not shown as a fraction.** "활성세션이 왜 1/5 2/5 이런식으로 잇는지도 모르겠네요 5개 제한할이유도 없는데". The 5 is D15's `spread_threshold`: a soft point where new-session packing stops preferring that account, never a cap (a full account still accepts sessions). Showing "2/5" reads as a limit. The dashboard and tray show the plain count ("활성 2"). They mark the account as crowded ("몰림") only when the count reaches the threshold, with a tooltip explaining that it is a spreading guide, not a limit. Routing is unchanged.
