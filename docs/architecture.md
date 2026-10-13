# quota-baton architecture

## Document status

| Field | Value |
|---|---|
| Role | Architecture (how the system is built and why) |
| Lifecycle | Draft — awaiting user approval |
| Normativity | Non-normative until Accepted. Behavioural rules live in [contract.md](contract.md). |
| Scope | v0 (MVP) and the roadmap to cutover |
| Last verified | 2026-10-13. Revised after the P1 design pre-check (Fable xhigh, Opus max); see `.sandbox/reviews/design-precheck-consolidated.md`. |
| Supersedes | Nothing. Inherits the draft handed over by session qa-x-db on 2026-10-13. |

## 1. Purpose and scope

quota-baton is a personal, local proxy. It lets the user's genuine Claude Code and Codex CLI sessions run across several of the user's own subscription accounts per provider. When one account runs out of quota, the next one takes the baton. A Windows tray flyout shows usage and account health.

In scope (MVP):
- Clients: genuine Claude Code (Anthropic Messages API) and genuine Codex CLI (OpenAI Responses API over HTTP SSE and WebSocket). This includes Codex CLI homes managed by Orca.
- Upstreams: `api.anthropic.com` with Claude OAuth accounts, and `chatgpt.com/backend-api/codex` with ChatGPT OAuth accounts.
- N accounts per provider, session affinity, quota-aware selection, refresh and re-login handling, and a tray flyout.

Later (designed for, not built in MVP):
- Anthropic-compatible API-key providers used from Claude Code: DeepSeek, Z.ai (GLM), Xiaomi MiMo ([D8](decisions.md#d8)).

Out of scope, permanently:
- Protocol translation, for example Claude models inside Codex ([D2](decisions.md#d2)).
- Clients other than genuine Claude Code and Codex CLI. A generic multi-provider gateway.
- Client impersonation of any kind: TLS fingerprint imitation, header or identity rewriting, cloaking ([D3](decisions.md#d3)).
- Remote or multi-user access. Auto-update. An orchestrator-specific API ([D4](decisions.md#d4)).

## 2. Design goals

The design goals are stated in the sections they govern: session affinity and failover before the first byte (§5, §7), single-flight refresh and a generation-guarded store (§6), quota-aware selection (§8), the tray and admin API (§10, §11), the journaled cutover tool (§12) and the metadata-only ledger (§13).

### 2.1 What quota-baton adds

- **Truthful health.** An account in the tray is green only if it can actually serve, and it shows why when it cannot.
- **Bounded memory by construction.** Nothing holds conversation history.
  - Each in-flight HTTP request keeps at most one retained copy of its body.
  - Each in-flight WebSocket message is held only until it is written.
  - Both count against one shared in-flight budget, enforced by backpressure.
- **Zero-drop UI and upgrades.** The tray can crash or restart freely. A core upgrade hands over the listener and drains live streams.
- **Native "all exhausted" errors.** The client receives the provider's own limit error with the earliest real reset time, so Claude Code, Codex CLI and the orchestrator keep their existing limit handling.
- **Right-click usage flyout** instead of a context menu.

### 2.2 Is a proxy's memory use irreducible? (R6, from Codex CLI 0.162.1 source)

No. Neither the provider's caching nor its conversation model requires a proxy to hold conversation state.

- **Prompt caching lives upstream.**
  - The cache matches on the rendered prefix and on `prompt_cache_key`.
  - The Codex `session-id` header carries the root session id, which subagents share.
  - A proxy keeps the cache warm by forwarding bytes unchanged and keeping a session on one account. It stores nothing.
- **The Codex client owns the conversation.**
  - Over HTTP it sends the full input on every turn with `store:false`. It never sends `previous_response_id` over HTTP.
  - Over WebSocket it sends incremental input (`previous_response_id`) only on the same socket. That continuation state lives in the upstream connection's memory.
  - On any close, drop, `previous_response_not_found` or connection-age limit, the client reconnects and **resends the full input** by itself.
- **Irreducible proxy state:**
  - per open socket: the glue to one upstream socket and its pinned account, plus small stream buffers that do not grow with conversation length
  - a session → account map, tens of bytes per session
- **Optional state, which we choose:** one retained request body per in-flight HTTP request, so that a limit or auth error can fail over before the first response byte without the client noticing (§5).
- **Not needed by genuine clients, so dropped:**
  - per-socket transcripts (the previous proxy)
  - synthetic prewarm
  - body rewriting
  - multiple body copies

**Measured on this machine (R7, 126 h of the previous proxy logs plus local transcripts).**

| Measure | Value |
|---|---|
| Concurrent model HTTP requests | peak 32, p95 13 |
| Open client WebSockets | peak 776, p50 309. Idle sockets pile up because Codex never closes them and the previous proxy never ages them out. |
| Codex request bodies, raw | p50 1.4 MB, p95 33 MB, p99 51 MB |
| Codex request bodies, zstd | p95 22 MB. Screenshots compress poorly. |
| Claude Code request bodies | p50 ~0.8 MB, p95 ~6 MB; API ceiling 32 MB; always uncompressed |

What a design needs follows from those numbers:
- One compressed body per in-flight request needs about 33 MB typically and about 290 MB at p95 load. Bodies are released at the first response byte, which makes the real figure lower still.
- Adding one decompressed transcript per open WebSocket, which is the previous proxy's behaviour, needs 0.7–3.9 GB at the current socket count. That reproduces the previous proxy's observed 3.6–4.2 GB.

**The real constraint is correctness, not memory.**
- Codex replays these items verbatim on every turn and never strips them: encrypted reasoning (`reasoning.encrypted_content`), remote-compaction blobs and file ids.
- If another ChatGPT account rejects them, every later turn of a session that changed account fails.
- The evidence is contradictory.
- For Claude, thinking-block signatures carry the same question. The previous proxy never stripped them across accounts and saw almost no 400s, which is weak evidence that they are portable.
- **New evidence (R8 I-16):** three Codex compaction blobs were rejected with `thinking_signature_invalid` after the previous proxy lost a session's binding and served it from another account. Compaction at least appears to be account-bound.
- **V2 result, HTTP cells, day 1 (2026-10-13; `.sandbox/g1/G1-log.md`).**
  - Codex encrypted reasoning items and Claude signed thinking blocks from account A are **silently ignored** by account B: 200, and the input-token count equals the stripped body. This holds inside a tool-use turn as well.
  - The negative controls (tampered blob or signature on the minting account) are rejected with 400, so validation exists.
  - The first request after a switch is cache-cold.
  - **Not portable, but harmless:** a moved session keeps working and loses only earlier hidden reasoning. Remote-compaction blobs were not tested; R8 I-16 suggests they are rejected.
  - The WebSocket cells and the second-day repeat are still pending.
- **Decision ([D10](decisions.md#d10)): relay as the genuine clients do.** Both CLIs replay these items verbatim and never strip them. The servers verify the items they minted and ignore the ones they did not. quota-baton therefore changes nothing, and has no hand-off rewrite.

## 3. Principles

- **P1 Pass-through.** Request and response bytes are forwarded unchanged. The only exceptions are the credential headers listed in contract C3 and the registered body exceptions in C4. Each exception must be justified by Phase 0 evidence.
- **P2 Bounded memory.**
  - The core holds at most one retained copy of each in-flight HTTP request body.
  - It holds each in-flight WebSocket message only until the message is written.
  - Both count against one in-flight budget (§13).
  - Response bodies are never buffered beyond the bounded commit hold (§5).
  - Conversation state is never stored.
- **P3 Stable core, volatile edges.**
  - The data plane rarely restarts.
  - The tray, config and accounts change without touching live streams.
  - Upgrades use takeover-and-drain.
- **P4 Truthful account health.** State changes are driven only by explicit upstream signals (contract C5/C6). Token expiry alone never proves health.
- **P5 Quota-aware and cache-aware.**
  - Sessions stick to their account.
  - Usage thresholds steer only new sessions.
  - A forced move happens only when the bound account becomes ineligible.
- **P6 Single owner of credentials.**
  - Accounts come from quota-baton's own OAuth flow, or from CPA in linked or adopted mode (D5).
  - Exactly one refresher exists per token family: quota-baton for owned and adopted accounts, CPA for linked ones.
  - There is one writer per account file.
  - Rotated tokens are persisted atomically before use.
- **P7 Small surface.** Two upstream families now, plus one later class: Anthropic-compatible API-key providers. No translation.
- **P8 Local only.**
  - Bound to 127.0.0.1.
  - Client-key authentication and a Host-header check.
  - The admin plane is separate from the data plane.

## 4. Processes and components

```
 Claude Code ─┐                       ┌─► api.anthropic.com        (Claude OAuth accounts)
 Codex CLI ───┼─► quota-baton core ───┼─► chatgpt.com/backend-api/codex (ChatGPT OAuth accounts)
 (Orca homes)─┘   127.0.0.1:<data>    └─► [later] Anthropic-compatible API-key providers
                       ▲  127.0.0.1:<admin> (token)
                       │
                 quota-baton tray  (flyout UI, client of the admin API)
                 quota-baton CLI   (status, login, pause, takeover, wiring)
```

- **`quota-baton` binary (core + CLI).**
  - `quota-baton serve` runs the core: data plane, admin API, account manager, quota tracker and ledger. It runs as a windowless background process.
  - The other subcommands are thin admin-API clients: `status`, `login`, `pause`, `resume`, `takeover` and, later, `wire`/`unwire`.
- **`quota-baton-tray` binary.**
  - A native Win32 tray icon and flyout. It holds no business logic and talks to the core only through the admin API.
  - At startup it ensures the core is running and spawns it detached if needed.
  - Quitting the tray never stops the core unless the user chooses "stop proxy" explicitly.
- **Inside the core:**
  - `http` — data-plane listener, routing and authentication
  - `relay` — HTTP/SSE and WebSocket relays
  - `providers::{claude, codex}` — upstream URLs, credential headers, limit-signal parsing, OAuth parameters and the usage endpoint
  - `accounts` — store, state machine and refresh
  - `select` — session bindings and the selection policy
  - `quota` — usage snapshots and polling
  - `admin` — admin API and event stream
  - `ledger`
  - `guard` — in-flight budget and memory guard

The provider interface (`trait Provider`) covers:
- login parameters and token exchange
- refresh
- building the upstream request (URL and credential headers)
- classifying upstream responses and stream events
- parsing usage headers and events
- polling usage

The later API-key providers implement the same trait without OAuth.

## 5. Data-plane request lifecycle (HTTP)

1. **Accept and authenticate** (contract C2).
   - Reject non-loopback peers.
   - Reject a `Host` other than `127.0.0.1:<port>` or `localhost:<port>`; this blocks DNS rebinding.
   - Reject any request that carries an `Origin` header.
   - Require the client key.
2. **Route** by method and path (contract C1). Unknown paths get 404 and are never forwarded.
3. **Admission and body retention.**
   - The body is forwarded exactly as received; `Content-Encoding` is never decoded for forwarding.
   - A **retained copy** is kept so that a limit or auth error can be retried on another account before commit.
     - Codex in API-key mode sends uncompressed bodies of 1–45 MB. Only clients using ChatGPT auth with a provider named "OpenAI" use zstd (R6).
     - So when a body larger than 1 MiB is not already `Content-Encoding: zstd`, its retained copy is zstd-compressed in memory (level 1). A body that is already zstd is retained as is.
   - **Target shape (M7b):** the body streams to upstream while its retained copy is compressed alongside ("tee"), so no full raw copy is ever held.
     - If upstream answers before the client body has been fully read, keep draining the body into the retained copy before failing over. If that copy would exceed its cap, forward the response without failover.
   - **Interim shape (M2):** read the whole body, then send it, retaining it raw. The raw bytes count against the budget.
   - Retained bytes count against the shared in-flight budget (§13). A body over `retain_max_per_request_mib`, or arriving while the budget is exhausted, is still forwarded. It just has no retained copy, so it cannot fail over. It is not rejected.
4. **Classify the session** (contract C8). The key comes from headers. The bounded body peeks that C8 allows apply to Claude bodies only. Codex bodies are never decoded on this path.
5. **Select an account** (contract C7). If its access token is near expiry, refresh it first (single-flight; §6).
6. **Build the upstream request.**
   - Copy the client headers, minus those that C3 removes.
   - Add the credential headers from C3.
   - Send the body unchanged (C4).
7. **Commit point.** Commit is the first upstream event that is not an error: the first SSE `data` event, or a non-SSE 2xx body.
   - Response headers and leading events wait in a bounded hold, at most 64 KiB and 10 s. Then they are released together.
   - Before commit, a failover-eligible outcome (C6) may retry on another account from the retained copy. That includes an error status or an error event that arrives first.
   - After commit, nothing is retried, and errors are relayed in-stream unchanged.
8. **Stream the response** chunk by chunk.
   - A line scanner watches SSE events for limit and error signals (C9). It skips lines longer than 1 MiB.
   - If the response has a `Content-Encoding`, the scanner reads a bounded, streaming-decoded side copy. The relayed bytes are never changed.
   - Error bodies read before commit are decoded the same bounded way, at most 64 KiB, for classification.
   - Upstream response headers are forwarded, so the clients' own limit displays keep working.
9. **Release and record.**
   - The retained copy is released at commit, or at the end for non-stream responses.
   - The ledger, quota and binding `last_used` are updated.

## 6. Accounts, credentials, refresh

**Store.** One file per account under `%LOCALAPPDATA%\quota-baton\accounts\`.
- Non-secret metadata (provider, label, email, external IDs, plan, user state, health, generation and token timestamps) is stored in clear JSON.
- Tokens are stored in a DPAPI (CurrentUser) encrypted blob inside the same file.
- Writes go to a temp file, then `FlushFileBuffers`, then `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`.
- Each write increments `generation`. A writer that started from an older generation fails instead of overwriting.
- DPAPI is called with `CRYPTPROTECT_UI_FORBIDDEN` and an application entropy value. This protects tokens at rest from other users, but not from malware running as the same user.

**Credential lease, the single owner across processes.**
- A core must hold an exclusive lease before it loads accounts. The lease is a named mutex `Local\quota-baton-credentials` plus a locked `accounts\.lock`.
- Only the lease holder may refresh tokens or write account and state files. A second `serve` without the lease refuses to start.
- On takeover the old core hands the lease over during drain (§14). That way the two cores never refresh the same single-use token.

**Login.**
- OAuth 2.0 with PKCE, using each provider's public CLI client registration (contract C10).
- The callback server binds the loopback addresses only, `127.0.0.1` and `::1`. The redirect URIs say `localhost`, and some browsers try IPv6 first.
- The OAuth `code` and the callback query string are redacted from all logs.
- The login is started from the tray or the CLI. The core returns an auth URL, and the tray opens the browser.
- A re-login for an existing identity (same `account_uuid`, or same `chatgpt_account_id` plus user) replaces that account's tokens and clears `needs_relogin`.
- If the Codex callback port 1455 is busy (e.g. a native `codex login`), the login fails with a clear message. There is no fallback port, because the redirect URI is fixed.

**Refresh.**
- **When it runs.**
  - Refresh-ahead: when less than 20 % of the token lifetime remains, with jitter.
  - On a 401: a refresh happens **only if the failed request used the account's current access token** (the failed-token guard). Otherwise another request has already refreshed, and the request is retried once with the current token.
- **Single-flight.** Refreshes are single-flight per account, keyed by the token that failed. They run detached from the requesting client, with a 30 s timeout.
- **Write-ahead ordering.**
  1. Persist `refresh_uncertain=true`.
  2. Send the refresh request.
  3. Persist the new tokens, then clear the flag.
  4. Only then use the new tokens.
- **Persist failure.** Keep the new tokens in memory, keep retrying the write, and never reload an older generation. The account goes to `cooling(store_error)` and the tray shows a critical warning.
- **Write-ahead save failure** (the `refresh_uncertain=true` write fails, M1b review SF2): do **not** send the refresh. Keep serving with the current access token while it is valid, and show the attention badge. Requests are never failed because of this. The account goes to `cooling(store_error)` only when the current token can no longer serve.
- **A 401 while a rotated token is pending persistence** carries a token that is no longer current. It changes nothing (SF4).
- **Errors are typed by the JSON error code.**
  - **Terminal:** `refresh_token_invalidated`, `refresh_token_reused`, `refresh_token_expired`, `refresh_token_revoked`, `invalid_grant`, or any 401 from the token endpoint. The account becomes `needs_relogin` at once, with no retry.
  - **Uncertain:** a transport failure after the request was sent, or a crash while `refresh_uncertain` is set. The refresh token may already be consumed.
    - Keep serving with the current access token.
    - **Retry only when the current access token can no longer serve** (M1b review MF1/SF1). That means it is within 10 min of expiry, or a 401 arrives for the current token. There is no fixed-interval retry, and no immediate retry after a restart, even when the original refresh-ahead point has already passed. The possibly consumed refresh token is thereby used as late as possible.
    - A terminal code on that retry means `needs_relogin`.
  - **5xx or 429:** back off and retry while the access token stays valid and is not rejected.
  - **Unknown 4xx:** back off like 5xx. After 3 in a row, the tray shows an attention badge. These never trigger an automatic `needs_relogin`.
- **A 401 after a successful refresh** is not proof of death. The account goes to `cooling(auth)` for 15 min, doubling up to 4 h, and a probe request runs when the cooldown expires. Only the terminal conditions above, and revocation 401s (C6 row 4), demand a browser re-login.

**Import from CPA** ([D5](decisions.md#d5)).
- `quota-baton accounts import cpa` reads `~/.cli-proxy-api/auths/*.json`. It maps the fields as follows:
  - Claude: `access_token`, `refresh_token`, `expired`, `email`, `account_uuid`, `organization_uuid`
  - Codex: the same tokens plus `id_token`, `account_id` and `plan_type`
- It writes one quota-baton account per identity: DPAPI-encrypted, `user_state=disabled` unless listed in `--activate`.
- **No token rollback.** A re-import compares the file's `last_refresh` (tie-break `expired`) with the stored token.
  - Newer → update.
  - Equal → may update.
  - Older → skip the token write and report why.

  An explicit `--activate` still applies its `user_state`. No path, linked reload included, ever replaces a token with an older one.
- It prints metadata only, never token values.
- **Linked mode** (default).
  - The account never refreshes. Before the access token is within 10 min of expiry, and once on any 401, the core re-reads the CPA file with the same identity.
  - It matches by `account_uuid`/`account_id` + email, not by filename, because CPA renames files on re-login.
  - CPA writes files non-atomically, so a partial or unparsable read is retried up to 3 times, 200 ms apart.
  - If the file holds no newer token by expiry, the account goes to `cooling(link_stale)`. The tray then shows "CPA is not refreshing this account".
- **Adopt** (`--adopt`, M8 only).
  - The command refuses unless CPA can no longer refresh the account: the CPA process is not running, or the account's file is gone from CPA's directory.
  - From then on the account is owned like an OAuth login.

## 7. Selection and session affinity

**Session key.**
- Derived from client headers (contract C8).
- **Claude Code:** session and agent ids. A subagent's key carries its parent, and an unbound child inherits the parent's account.
- **Codex:** the `session-id` header. It carries the root session id for subagents too, which is also the prompt-cache key, so one key covers the whole session family.

**Binding table.**
- Maps a session key to its account and `last_used` time. The LRU cap is 20 000.
- Every HTTP request and every relayed WebSocket `response.create` updates `last_used`.
- A binding that an open pinned socket references is never expired or evicted.
- Lifetime follows the cold-point rule in contract C7. The idle time is measured from the end of the last response.
- Persisted write-behind (at most 60 s late) to `state\bindings.json`, . Flushed on drain and exit, and exported to the successor on takeover.
- A WebSocket connection is additionally pinned to one account for its whole life.
- **Unbound requests** (no key derivable, C8) stay on their family's last-used account until it becomes ineligible. They never move per request.

**Policy.** The full algorithm is contract C7.
1. If the bound account is eligible for this request, use it.
2. Otherwise, if the parent's account is eligible, use it and bind.
3. Otherwise choose among eligible accounts:
   - Placement is serialized. It applies the gate to projected usage: the last observed usage plus an allowance for each request in flight.
     - 5-hour usage must stay below 90 %.
     - Weekly usage must stay below 90 %, or below 98 % within about 5 h of that account's weekly reset.
   - Among accounts that pass, the **soonest weekly reset** comes first ("use it before it resets"; the same ordering as the orchestrator's ADR-0040). Resets within 3 h of each other count as tied and are broken by the soonest 5-hour reset.
   - If none passes, take the account with the most headroom.
   - Bind the session to the chosen account.
4. If no account is eligible, return the all-exhausted response (C6).

No user-maintained priority order exists. The only manual placement setting is **drain**, which takes no new sessions while existing ones stay.

The gate never moves a bound session. A bound session moves only in two cases:
- **Forced:** its account enters `cooling`, `needs_relogin`, `paused` or `disabled`. A quota limit that resets within 60 s holds the request instead of moving it.
- **Free:** at a cold point, i.e. after one hour idle, when the provider cache is already cold.
  - Until V2 shows that conversation items are portable, a cold session moves only if its current account fails the gate.
  - After that, it is placed like a new session ([D11](decisions.md#d11)).

Why not switch a session back as soon as the preferred account recovers? Each switch makes one turn re-read the whole context without cache.
- Claude caches are isolated per workspace, so a switch is always a full miss.
- On Claude Opus 5.5 a cache read costs 0.05× the input price, while a write costs 1.25× (5-minute TTL) or 2× (1-hour TTL). So one switched turn costs about 25–40 warm turns of input.
- Codex switches measured cheaper on this machine: 88 % cache hits after a switch versus 95 % before (R6 §3.2).

Switching only when forced or free is therefore the token-optimal policy. Routing new sessions to the quota that expires soonest captures most of the benefit of restoring.

Tuning items, revisited with shadow-run data rather than designed up front. The ledger logs every move, the weekly % left unused at each reset, and the forced moves per gate crossing (C7).
- A 5-hour gate projected from the burn rate, and headroom measured in absolute tokens (learned from Δ% per token), as the panel suggested.
- The herd of cold turns when a busy account exhausts.
- Whether Claude Code on a custom base URL uses only the 5-minute TTL. If so, the cold point could be shorter than 1 h. Keeping 1 h is conservative and only delays a free rebalance.

## 8. Quota tracking

Each account keeps a usage snapshot: one entry per window (`5h`, `7d`, and named buckets such as Claude `7d_oi` or Codex metered limits). Each entry holds `used_pct`, `reset_at` and `status`, plus `plan`, `credits`, `source` and `observed_at`.

**Passive sources.** These are free and arrive with traffic:
- Claude `anthropic-ratelimit-unified-*` response headers
- Codex `x-codex-*` response headers
- Codex WebSocket `codex.rate_limits` events

**Active polling.** Codex uses `wham/usage`; Claude uses the OAuth usage endpoint, pending verification V5.
- **When to poll.**
  - When the snapshot is older than 10 minutes and the account is a selection candidate or the tray is open.
  - Every `cooling(quota)` account **at least hourly**, to catch early resets. One really happened on 10-10 (R3 Q2).
  - Immediately before answering C6 row 14 or row 15, for every candidate account.
- **Limits.** The minimum interval per account is 5 minutes, with jitter and backoff.
- **Exclusions.** Never poll accounts in `needs_relogin` or `disabled`.
- **No usage endpoint.** If V5 finds no usable Claude usage endpoint, a Claude `cooling(quota)` simply ends at `until` and is confirmed by the next real request.

**Proactive exhaustion.**
- Any window at or above 100 %, or `limit_reached`, moves the account to `cooling(quota, until=reset_at)` even before upstream rejects anything.
- **Credits hold:** a Codex account that would keep answering from paid credits is held unless `allow_credits` is set for it. This replaces `Watch-CodexQuota.py`.
- **Early reset:** if a poll shows a different `reset_at` and the usage is below 100 %, the account returns to `ready`. This applies the 10-10 watcher lesson.

## 9. WebSocket relay (Codex `GET /v1/responses`)

Facts from Codex 0.162.1 source (R6):
- One socket per thread, reused across turns and kept open while idle indefinitely. Subagents use their own sockets.
- One in-flight response per socket.
- Every message is `response.create`. The only other client message is `response.interrupt`.
- The server ends connections at 60 minutes.

Codex's retry budget matters for every rule below:
- A handshake failure or a close during a response counts as one retry.
- After 5 retries in one request, the session switches to HTTP for the rest of its life (`responses_retry.rs:120-139`).
- A handshake answered with 429 `usage_limit_reached` or with 400 is **terminal**: no retry and no fallback (`responses_websocket.rs:559-571`, `api_bridge.rs:175-206`).

Relay design:
- **Upgrade: dial first, then answer.**
  1. Authenticate the client upgrade as in §5.
  2. Select an account from the upgrade request's session headers.
  3. Dial `wss://chatgpt.com/backend-api/codex/responses` *before* answering the client.
  4. Classify the upstream handshake response with C6 rows 2–7. Update that account's health, and fail over to the next eligible account.
  5. Only then answer the client:
     - 101 when an upstream socket is open.
     - A native 429 `usage_limit_reached` body with the earliest reset (row 14) when every account is exhausted. This is terminal.
     - 400 (row 15) when no account is usable. Also terminal.
     - 502 for a pure network failure. Codex retries it.
  - quota-baton never answers 426, because a 426 sends that Codex session to HTTP permanently.
- **Per-hop handshake.**
  - `Sec-WebSocket-*`, `Connection` and `Upgrade` belong to each leg; they are never copied (C3). Each leg negotiates its own extensions.
  - Codex offers permessage-deflate (`responses_websocket.rs:538-541`).
  - Per [D12](decisions.md#d12):
    - **Loopback leg:** deflate is always declined.
    - **Upstream leg:** a switch, `upstream_deflate`, default **off**. It is decided by an A/B measurement in M3. The expected gain is modest, because large inputs are mostly screenshots (about 1.5×). Enabling it needs a deflate-capable client, such as the tungstenite fork Codex itself uses.
- **Pin.** One client socket maps to exactly one upstream socket, pinned to one account for its life.
- **No WebSocket-over-HTTP emulation mode exists**, so there is no setting to forget. The previous proxy's default emulation mode is what made it keep a transcript per socket (R6 §5). A WebSocket client is always served by a WebSocket upstream, and an HTTP client by HTTP.
- **Relay messages in both directions with unchanged payloads.**
  - This covers text, binary, ping, pong and close, including `response.interrupt`. Frames themselves are per-hop: masking and deflate differ on each leg.
  - The relay parses upstream text messages of 1 MiB or less to read:
    - `type`
    - `status` and `error` on `error` events
    - the `codex.rate_limits` fields (C9)
    - `response.created` / `response.completed`, which mark the commit point
  - **No transcript, no `previous_response_id` state.**
- **Message memory.**
  - Codex sends full-input messages uncompressed: p95 33 MB, p99 51 MB (R7).
  - Every in-flight message counts against the shared in-flight budget (§13) until it has been written.
  - The budget is checked before the relay reads the next message from a client socket. While it is exhausted, the relay stops reading, so TCP backpressure holds the bytes instead of the heap.
  - A message already being read is completed, so one message of at most 192 MiB can overshoot the budget. The memory guard is the hard backstop.
  - Both `max_message_size` **and `max_frame_size`** are set explicitly to 192 MiB on both legs, above the largest measured body (132 MB).
    - Codex sends a full input as a single frame, and tungstenite's default `max_frame_size` is 16 MiB.
    - So both limits are needed; otherwise oversized input loops through a close, the client's retries, and a sticky HTTP switch.
  - Per-socket buffers shrink back after a large message.
- **Commit point for a WebSocket response.** The response is committed by the first upstream message for the current `response.create` that is not an error, normally `response.created`.
  - Messages that arrive before commit, such as `codex.rate_limits` or a metadata event, may be relayed. Codex discards a failed stream.
- **Failover before commit.**
  - If an `error` from rows 2 or 4–7 arrives before commit, the frame is not relayed.
  - The account's health is updated **first**, and then the client socket is closed (1012).
  - Codex retries on a new connection with full input, and the upgrade path above selects another account.
  - Each such close costs one retry. With the user's 4 Codex accounts, k exhausted accounts cost k retries and about 0.2·(2^k − 1) s of backoff. That stays within the budget because the all-exhausted case is answered terminally at the upgrade.
- **After commit,** error frames are relayed as is. A synthesized error frame MUST include `status`, because Codex ignores error frames without one.
- **The pinned account becomes ineligible while idle:** close the client socket. Codex reconnects silently on its next request.
- **Timeouts.** The upstream idle read timeout is 10 min.
  - Upstream pings reset it; Codex itself only answers pings.
  - Pings are answered promptly and never wait behind large writes.
- **Idle pairs.**
  - A socket pair idle for more than `[websocket] idle_close_secs` (default 1800) is closed. The server's 60-minute limit also applies. This value must stay below `rebalance_idle_secs`.
  - Codex reconnects silently on its next request and resends full input. That costs upload bandwidth only, because the server-side prompt cache is keyed by session and prefix, not by socket.
  - This keeps the socket count near "sessions active in the last 30 minutes" instead of the 776 that the previous proxy accumulated.
- **Session moves to HTTP:** a proxy outage longer than about 6 s during an active request switches that Codex session to HTTP for the rest of its life.
  - So HTTP SSE support is mandatory.
  - Takeover gaps must stay in the millisecond range. Whether Windows allows that is spiked in M3; see §14.

## 10. Tray

- **Icon.** A Win32 notify icon. Its badge shows normal, attention (an account needs re-login or all accounts are exhausted) or error (core unreachable). The tooltip carries a one-line status.
- **Flyout.** Opened by left **or right** click.
  - It is a borderless, DPI-aware (Per-Monitor V2) popup anchored to the icon rectangle, with Windows 11 rounded corners and the system light/dark theme.
  - It closes on focus loss or Esc.
  - It is keyboard navigable.
  - UI text is Korean.
- **Content:**
  - Header: core status and version.
  - One section per provider.
  - Per account:
    - label or email and plan
    - a state chip: in use, ready, limit until HH:MM, needs re-login, paused, disabled
    - 5-hour and weekly usage bars with reset times
    - row actions: log in / re-login, pause / resume, disable / enable
    - row menu toggles: "take no new conversations" (`drain`) and, for Codex, "allow paid credits" (`allow_credits`)
    - These two toggles cover every per-account setting, so the user never edits files for normal use.
  - Footer:
    - add account
    - settings (opens the config folder)
    - open logs
    - quit, which offers "tray only" or "tray and proxy"
- **Updates.** The flyout subscribes to the admin event stream while open and refreshes on a slow timer otherwise.
- **Toasts** appear only for actionable events:
  - an account needs re-login
  - an account hits its limit
  - all accounts of a provider are exhausted
  - **a burst**: one client (`originator`/UA) opens more than `burst_new_sessions` (default 30) new sessions within 10 minutes. This is the signature of a runaway loop draining the pool (R8 I-40).
  - a linked account goes stale
  - the core restarted unexpectedly
- **Supervision.**
  - The tray polls the core's health every 10 s.
  - If the core is unreachable for 30 s and its process is gone, the tray restarts it with backoff (5 s doubling to 5 min) and shows a toast. The previous proxy died silently three times with no supervisor (R8 I-03).
  - The tray never kills a live core.
- **Autostart.** The tray starts at logon through `HKCU\...\Run`, but only after cutover approval.

## 11. Admin API

The admin API listens on `127.0.0.1:<admin>`, a separate port, and applies the same Host and Origin checks as the data port. It requires a bearer admin token, stored in a user-ACL'd file.

It carries:
- health and state
- the event stream (SSE)
- the login flow
- account actions
- usage refresh
- lifecycle: drain, takeover and shutdown, including the credential-lease handover
- a debug-only account pin, used by Phase 0

Details are in contract C11.

## 12. Persistence layout (`%LOCALAPPDATA%\quota-baton\`)

| Path | Content |
|---|---|
| `config.toml` | Small user settings: ports, gates, and the per-account `drain` / `allow_credits` (C12) |
| `accounts\*.json` | Account metadata plus the DPAPI token blob, one file per account |
| `accounts\.lock` | Credential lease (§6) |
| `state\quota.json` | Last usage snapshots, so the tray is not blank after a restart |
| `state\bindings.json` | Session bindings. Written behind, at most 60 s late (§7). |
| `ledger\YYYY-MM-DD.jsonl` | Per-request metadata, no content (§13) |
| `logs\` | Rolling logs with redaction |
| `admin.token`, `client.key` | Local secrets, user-ACL'd |
| `cache\codex-models.json` | Last good Codex model catalog |
| `journal\wiring.json` | Original values for reversible client wiring (cutover tool, M8) |

## 13. Observability and self-protection

- **Logs.**
  - Structured logs with a redaction layer. Never logged: Authorization values, tokens, keys, OAuth codes and callback queries, and bodies.
  - Rotation is daily, keeping 7 days and at most 50 MB.
- **Ledger** (from M2).
  - One JSON line per request with: time, request id, family, route, account id, session hash, model (when known without decoding), `originator` and `User-Agent` product token, status, upstream status, time to first byte, duration, bytes in and out, attempts, outcome.
  - It identifies who sends what during the shadow run (O11).
  - It also records one event per token refresh attempt and per linked reload: account, trigger, outcome code, and new expiry. No token values. Three Codex token families died within a week for unknown reasons (R8 I-06), and these events make the next one diagnosable.
  - Retention: 30 days or 500 MB, whichever comes first.
- **Shared in-flight budget**, `inflight_budget_mib`, default 384. It covers two things:
  - retained HTTP bodies
  - in-flight WebSocket messages

  The budget is an admission and backpressure check, not a hard heap cap; the memory guard below is the hard backstop.

  Retained copies are capped at 64 MiB per request. When the budget is exhausted:
  - HTTP is forwarded without retention, so it has no failover.
  - WebSocket reading pauses, which applies backpressure.
- **Memory guard.**
  - Private bytes are sampled every 10 s.
  - Above the hard limit (default 1 GiB), until memory recovers:
    - new requests get 503 with `Retry-After`
    - reading pauses on every client WebSocket
    - the tray shows a warning
  - Existing streams are never killed.
- **Timeouts.** Connect 10 s; response headers 5 min; idle read 10 min.
- **Shape capture** (Phase 0 only).
  - Compiled in only with the `shape-capture` cargo feature **and** `debug_assertions`. A release build with the feature fails to compile.
  - Enabled per run with `serve --capture-dir <dir> [--capture-limit N]` (default 50 requests, then off).
  - `<dir>` must already exist and its path must contain a `.sandbox` component. Otherwise `serve` refuses to start. Capture output is never tied to `QB_HOME`.
  - It records header names; header values from an allowlist only (beta lists, versions, `originator`, the UA product token); and body key names to a bounded depth. It never records session, installation, account or turn-metadata values.
  - Output goes under `.sandbox\` and is included in the A5 secret grep.
  - It is a declared exception to "Codex bodies are never decoded".

## 14. Lifecycle and upgrades

- **Startup order.**
  1. Acquire the lease.
  2. Load accounts and state.
  3. Then bind the listeners.

  A request never reaches a half-initialised core; the previous proxy returned startup 400s (R8 I-04).
- **Priority.** The core runs at `ABOVE_NORMAL_PRIORITY_CLASS`, so that a saturated machine does not starve relaying or health checks.
- **Config** reloads on file change. Account changes apply immediately through the admin API.
- **Takeover.**
  1. `quota-baton serve --takeover` asks the running core, over the admin API, to drain.
  2. The old core does the following:
     - stops refresh-ahead and 401-triggered refresh; a 401 during drain is relayed without refreshing and without changing health
     - stops writing account and state files
     - flushes bindings, exports them, closes its data listener
     - **releases the credential lease**
  3. The new core:
     - acquires the lease
     - loads the accounts
     - imports the bindings
     - binds the port, retrying for up to 2 s
  4. The old core finishes its in-flight HTTP streams. WebSocket connections are closed with 1001 after their current response.
  5. The old core exits when idle, or after the drain timeout (default 30 min).
- **Gapless handover.**
  - Spiked in M3 (2026-10-13): the new core binds the port in under 1 ms while the old core still holds accepted sockets, with or without `SO_EXCLUSIVEADDRUSE`. Socket duplication is not needed.
  - If it does not, hand the listening socket over with `WSADuplicateSocketW` instead.
  - A WebSocket gap longer than about 6 s moves active Codex sessions to HTTP permanently (§9).
- **Configuration** stays deliberately small. Behaviour fixes go into code, with tests, never into config knobs.

## 15. Technology

Rust, with one workspace and two binaries ([D6](decisions.md#d6), pending confirmation).

| Concern | Choice |
|---|---|
| Runtime | tokio, multi-threaded |
| Data-plane and admin servers | axum / hyper |
| Upstream HTTP client | reqwest with rustls, streaming bodies, no automatic decompression; standard configuration (no fingerprint shaping) |
| WebSocket | tokio-tungstenite. If D12 enables upstream deflate after the M3 measurement: a permessage-deflate-capable tungstenite, e.g. the openai-oss-forks fork that Codex uses |
| Windows APIs | the `windows` crate: DPAPI, Shell_NotifyIcon, Win32 windows, Direct2D/DirectWrite |
| Config | serde + toml |
| Logs | tracing with a redaction layer |
| Secrets | zeroize / secrecy |

Crates:
- `qb-core` (library)
- `quota-baton` (binary: serve and CLI)
- `qb-tray` (binary)
- `qb-proto` (admin API types)
- `qb-testkit` (fake upstreams and fault injection)

## 16. Verification

### 16.1 Phase 0: the protocol facts that gate the design

Phase 0 runs against the real upstreams on a **shadow port (10200)**:
- One interactive session is pointed at quota-baton through its environment only. Global client config is not touched.
- It uses accounts the user logs into through quota-baton, following [D13](decisions.md#d13).
  - Log in **one account at a time**, starting with the main accounts already in use: Claude `acct-alpha`, Codex `acct-alpha`, then Codex `hello.sample-acct`. Claude `hello.sample-acct` is added only when a pair is needed.
  - After each login, confirm that the same account still works in CPA before adding the next one (O10).
- **Credit guard.** The Codex plus accounts can keep serving by spending paid credits, and the credit hold only arrives in M5. Until then:
  - Before each test run, check the account's usage with quota-baton's own usage poll (V5, which therefore runs first in G1). The CLI's `/status` shows nothing in API-key mode (R8 I-37).
  - Do not run tests on an account at or above 80 % of any window.
  - Stop if usage reaches 95 %.
- Wire shapes are recorded with the `shape-capture` build (§13).
- The decision rule for each test is written down before it runs.

| Id | Question | Outcome → decision |
|---|---|---|
| V1 | **Claude pass-through.** Does Claude Code's request, unchanged apart from the C3 credential headers, succeed on an account token? Record whether Claude Code sends the following to a custom base URL: `X-Claude-Code-Session-Id`, `-Agent-Id`, `-Parent-Agent-Id` (main, subagent and count_tokens requests), `metadata.user_id`, `diagnostics`, a billing `cch` value, and `Accept-Encoding`. | **Pass:** the Claude half proceeds. **Upstream rejects the identity or signature:** if the fix would require writing the serving account's real `account_uuid` and no client signature covers the body, it becomes candidate E4. If it would require computing a client signature, it is **infeasible under D3** and escalated to the user. **Header absent:** the C8 fallbacks apply. |
| V2 | **Account portability.** Procedure below. | Done for HTTP on 2026-10-13: foreign items are silently ignored, so [D10](decisions.md#d10) is pass-through. The WebSocket cells and a second-day repeat are confirmation only. |
| V3 | **Codex pass-through, default home** (`~/.codex`: built-in OpenAI provider, API-key auth; the user does not use Orca homes, 2026-10-13). Does the request, unchanged apart from C3, succeed? Is a routing hint needed? | **Pass:** proceed. **Fail:** the user chooses between candidate header exception H3 and rewiring the home to a passing shape at cutover. |
| V4 | **Codex WebSocket relay.** Check on the wire what R6 found in source: a close during a response leads to a reconnect and a full resend, and an idle close leads to a silent reconnect. Also check that a 429 `usage_limit_reached` or a 400 on the upgrade is terminal, and the deflate negotiation on each leg. | **Pass:** proceed. **Fail:** redesign §9 before M4. |
| V5 | **Usage endpoints** (`wham/usage`, Claude OAuth usage): exact shape, required headers, an honest `User-Agent`. | The parsed snapshot matches the CLI's own `/status`. If there is no Claude endpoint, use the fallback in §8. |
| V6 | **OAuth login and refresh** for both providers, with an honest `User-Agent`. | Tokens obtained and the refresh token rotates |
| V7 | **Pass-through of** `/v1/alpha/search`, `/v1/images/*` and `/v1/models?client_version=`. `/v1/responses/compact` is dormant in Codex 0.162.1, so it stays an untested pass-through. | 200 from the real client |

**V2 procedure.**

It is run with the debug pin (C11). The HTTP cells run right after M2, and the WebSocket cells after M3 as confirmation only.

1. **Matrix.**
   - Run each Codex cell over HTTP, and repeat a subset over WebSocket.
   - Pairs, run in both directions:
     - Codex: `acct-alpha` ↔ `hello.sample-acct` (plus ↔ plus).
     - Claude: `acct-alpha` ↔ `hello.sample-acct`.
     - A plus ↔ free cell is added only if the user later adds a free account.
   - Use one model that every plan in the pair can run, with the same effort and headers, so that "model not supported" cannot cause a false negative.
   - Switch both at a turn boundary and in the middle of a tool loop.
2. **Codex cells.**
   1. Build a session of at least 3 turns with reasoning on account A, and capture the full body of the next request.
   2. **Presence check:** the captured body contains `encrypted_content` minted on A: reasoning items, compaction items, and any encrypted message parts, each counted separately. With zero such items, the cell is invalid; it does not count as a pass.
   3. Send the captured body four ways:
      - **Positive control:** to A unchanged. It must succeed.
      - **Test:** to B, changing only the C3 headers.
      - **Stripped control:** to B with every `encrypted_content` item removed. This separates an account-level rejection from a blob rejection.
      - **Negative control:** to A with one byte of one blob flipped. This shows what a rejection looks like. If the server accepts the flipped blob, the cell proves nothing.
   4. **Silent-drop check:** compare `usage.input_tokens` between A and B for the identical body.
   5. **Compaction cell:** plant a canary fact, trigger a remote compaction (the provider must be named "OpenAI"), then ask for the canary on B.
3. **Claude cells.** Run a tool-use loop with thinking on A, so that a signature is actually validated.
   - Replay the request to A, to B, and to B with the thinking blocks stripped.
   - Tamper with a signature on A, once in an earlier turn and once in the current turn, to learn which blocks the API validates.
   - Record `context_management`.
4. **Repeat** the whole matrix on a second day.
5. **Decision rule.** A family is **portable** only if every valid cell passes and its negative control fails. Any explicit rejection or silent drop marks that family **not portable**.
6. **Watching for change.** The ledger records upstream error codes (C6 row 12b), so a future server change that starts rejecting foreign items becomes visible.
7. **Data handling.** Body captures for V2 contain conversation content and are therefore made only with the separate Phase 0 tool. That tool writes only under `.sandbox\` and deletes its output after the decision is recorded.

### 16.2 Fault injection (`qb-testkit` fake upstreams)

Each row of contract C5 and C6 has a test. In particular:
- 429 with a reset
- a `token_revoked` / invalidated-token 401
- a stale-token 401 racing a refresh: no health change
- a 401 after a successful refresh: `cooling(auth)`, not `needs_relogin`
- refresh `refresh_token_invalidated`
- a crash while `refresh_uncertain` is set
- 403 with an unknown model, on two accounts in a row
- 404 thread not found
- 5xx, 529 and 507
- connect failure
- an error event before commit, and a drop after commit
- a WebSocket handshake answered with 429 / 400 / 502
- a WebSocket pre-commit error that leads to health being recorded, then a 1012 close, then a reconnect on another account
- all accounts exhausted, on HTTP and on the WebSocket upgrade
- an early reset seen by an hourly poll of a cooling account
- a refresh falling due during a takeover drain: exactly one refresh happens
- a binding kept alive by an open pinned socket past `rebalance_idle_secs`
- a single unfragmented WebSocket frame larger than 16 MiB (e.g. 40 MiB) on both legs: relayed, not rejected (`max_frame_size`)
- body and message-payload invariance, byte for byte, for plain and zstd bodies and for WebSocket messages

### 16.3 Acceptance criteria

| Id | Criterion |
|---|---|
| A1 | **Memory**, as core private bytes. The harness is a seeded load generator that samples request sizes from the R7 distributions (HTTP bodies and WebSocket full-input messages: p95 33 MB, p99 51 MB raw) and records time to first byte and upload rate.<br>• Idle: ≤ 40 MiB.<br>• 300 idle WebSocket pairs: ≤ 128 MiB.<br>• 2 h soak (32 concurrent HTTP streams, 50 active WebSocket pairs, plus reconnect-driven full-input resends): peak ≤ 512 MiB. The in-flight budget is 384 MiB.<br>• Back to ≤ 128 MiB within 5 min of idle.<br>• 24 h soak: no upward trend.<br>• Tray: ≤ 30 MiB.<br>• **CPU:** idle ≤ 1 % of one core on average; during the soak ≤ 25 % of one core on average. The previous proxy averages about 0.7 of a core (R8 I-54).<br>These are initial targets, set at M7c. The allocator and buffer sizes may be tuned to meet them; the targets themselves are not relaxed silently. |
| A2 | Added time to first byte ≤ 5 ms p50 locally, excluding refresh and the commit hold. Measured from M7b (tee); the M2 interim shape is exempt. |
| A3 | Every fault-injection test in §16.2 passes. |
| A4 | Forwarded HTTP bodies and WebSocket message payloads are identical to what was received, except for registered exceptions. |
| A5 | No secret appears in logs, ledger or captures. An automated grep test enforces this. |

## 17. Milestones

Each milestone is independently verifiable and sized for an implementer working at medium reasoning. Codex implements; this design session reviews each milestone before the next one starts.

| M | Content | Verification |
|---|---|---|
| M0 | Workspace and crates; config; redacting logs; data listener with client key, Host and Origin checks; admin health; `qb-testkit` skeleton | cargo test, curl |
| M1 | **M1a:** account store (DPAPI with entropy, atomic writes, generation, credential lease) and **CPA import in linked mode** (D5). **M1b:** OAuth login for both providers through the CLI (callback on 127.0.0.1 and ::1); single-flight refresh with the failed-token guard, typed errors and `refresh_uncertain` | Unit tests; one real login per provider by the user (V6). The user chooses the accounts (O10). |
| M2 | Single-account HTTP pass-through for every C1 HTTP route; commit hold; in-flight budget and retention cap; ledger with `originator` / UA; debug pin (C11); `shape-capture` build; body-invariance tests | Testkit; then **gate G1** |
| **G1** | **Phase 0 gate:** V1, V3 (both home types), V5, V6, V7, and the HTTP cells of V2 | Outcomes recorded. D10 decided for HTTP. Any D3 conflict escalated to the user before M3. |
| M3 | Codex WebSocket relay: dial-first upgrade, per-hop handshake, the `upstream_deflate` switch (default off), message budget and backpressure, commit point, idle close; spike of the Windows port rebind | Testkit; V4; WebSocket cells of V2; the D12 deflate A/B measurement |
| M4a | State machine and failover: C5 and C6 for HTTP and WebSocket; passive quota from headers and events | The fault-injection rows for C5/C6 |
| M4b | Selection and bindings: C7 and C8, persistence write-behind, all-exhausted and no-usable responses | Testkit selection cases |
| M5 | Usage polling (cooling accounts hourly, pre-row-14/15 polls), placement gate with in-flight allowance and near-reset weekly gate, reset hold, credits hold, early reset, move and unused-quota logging | Testkit; a live comparison with the CLIs' own status |
| M6a | Tray: read-only flyout | Manual UI check; tray memory |
| M6b | Tray: account actions, row-menu toggles, login and re-login | Manual UI check |
| M7a | Takeover: credential-lease handover, drain, and socket handover if the M3 spike requires it | Testkit takeover cases |
| M7b | Tee plus compressed body retention, and the memory guard | A2; budget tests |
| M7c | Soak | A1 |
| M8 | Cutover tool: journaled `wire` / `unwire` with a dry-run diff. Checklist: the Claude settings env; the Codex default home (keep `openai_base_url` in the top section for the herdr shim); Orca homes are **not** wired (the user does not use Orca), but herdr's `session-homes.json` may still resume old sessions in Orca homes, which keep pointing at the previous proxy until those sessions are retired; `model_catalog_json` without the 14 `anthropic/*` entries (D2), never pointed at a frozen file (R8 I-19); autostart; what happens to other clients on 10100, such as pi and omp; stop the previous proxy's scheduled tasks (`cliproxyapi-proxy`, `cliproxyapi-quota-watch`); offer to archive or delete plaintext credential copies in the previous proxy backups (R8 I-33). Wiring rules (R8 I-34–I-36): wired homes send the client key, never their own ChatGPT token; never change the provider id of existing Codex threads; write config files atomically and retry on a locked file | Dry run reviewed by the user. **Running it is the user's decision** ([D9](decisions.md#d9)). |
| Later | Anthropic-compatible API-key providers (DeepSeek, Z.ai, MiMo); token accounting in the ledger | — |

## 18. Open questions

- **O1–O7** correspond to Phase 0 items V1–V7. O2 (V2, account portability) was answered on 2026-10-13: pass-through ([D10](decisions.md#d10)). The M3 WebSocket cells are confirmation only.
- **O8.** Resolved. Codex posts `{base}/images/generations` and `{base}/images/edits` (`codex-api/src/endpoint/images.rs:62-83`), so the upstream is `https://chatgpt.com/backend-api/codex/images/{generations,edits}`. V7 confirms acceptance.
- **O9.** Resolved (2026-10-13, V7). `GET /v1/models?client_version=` passes through to `backend-api/codex/models` and returns the full `{"models":[...]}` catalog that Codex expects (10 models live). The Claude `/v1/models` also passes through (13 models).
- **O10.** Do quota-baton's own OAuth logins coexist with the CPA logins of the same accounts during the shadow period? Separate logins should be separate token families. The user picks the accounts. Watch for revocations.
- **O11.** Who sends the 99.6k HTTP `POST /v1/responses` requests seen at the previous proxy?
  - Codex is WebSocket-first (R6), and the default-home `codex-tui.log` has no "falling back to HTTP" warning. So sticky fallback is not shown to be the cause.
  - The M2 ledger records `originator` and the `User-Agent` product token so the shadow run can identify the senders.
- **O13. Per-account model entitlements: catalog and routing.** Owner decision, not yet decided (raised 2026-10-13).
  - **Problem.** Plans and accounts differ in which models they can serve. On 2026-10-13, Codex `gpt-daybreak-blue-latest` was listed for acct-alpha but not in the catalog fetched through hello.sample-acct (V7: 10 models, no daybreak). CPA hides such models entirely, because it serves a public catalog, and rejects them with `model_not_found`.
  - **Today's design.**
    - `/v1/models` passes through with **one** account, so the visible catalog depends on which account answered.
    - Selection does not look at entitlements in advance. A wrong account answers with a "model not supported" 400, and C6 row 6a then fails over and cools only that model on that account. This is reactive, and the first request on the wrong account pays one extra round trip.
    - Codex HTTP bodies are not decoded, so the model is unknown on HTTP. The WebSocket path reads it from `response.create`.
  - **Options** (they can be combined):
    1. Show the union of the catalogs of all active accounts. Fetch per account on start and refresh periodically.
    2. Model-aware routing: skip accounts whose catalog lacks the requested model, falling back in placement order. This needs the model name, either from a bounded head peek of identity-encoded Codex bodies or from the WebSocket message.
    3. A manual allow or hide list per model in the tray.
    4. Rely on row 6a cooling only (the current design).
  - The choice affects C1 (`/v1/models`), C7 (eligibility gets a model filter) and C8 (peek scope). It does not change D2: the request body still never changes.
- **O12.** `POST /v1/memories/trace_summarize` is used by Codex's `memories` feature, which is off by default (`core/src/client.rs:179`, `features/src/lib.rs:1231-1235`). It returns 404 (unsupported) until the user asks for it.
