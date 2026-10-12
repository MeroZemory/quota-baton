# quota-baton behavioural contract

## Document status

| Field | Value |
|---|---|
| Role | Executable design contract: externally observable behaviour and invariants |
| Lifecycle | Draft. Awaiting user approval. Items marked **[V#]** are gated by a Phase 0 verification ([architecture §16.1](architecture.md#161-phase-0--protocol-facts-that-gate-the-body-invariance-rule)). |
| Normativity | Normative for implementation once Accepted. Tests are derived from it. |
| Scope | v0 (MVP) |
| Last verified | 2026-10-13. Protocol constants are the official clients' public values; each section notes how they were verified. Revised after the P1 design pre-check. |
| Supersedes | Nothing |

The key words MUST, MUST NOT and SHOULD carry their RFC 2119 meaning.

## C1. Data-plane routes

Listener: `127.0.0.1:<data_port>`. The default is 10200 during shadow running and 10100 after cutover.

| Client request | Family | Upstream | Notes |
|---|---|---|---|
| `POST /v1/messages[?query]` | claude | `https://api.anthropic.com/v1/messages[?same query]` | SSE or JSON |
| `POST /v1/messages/count_tokens[?query]` | claude | `https://api.anthropic.com/v1/messages/count_tokens[?same query]` | |
| `GET /v1/models` with an `anthropic-version` header | claude | `https://api.anthropic.com/v1/models` | **[V7]** |
| `POST /v1/responses` | codex | `https://chatgpt.com/backend-api/codex/responses` | SSE |
| `GET /v1/responses` with `Upgrade: websocket` | codex | `wss://chatgpt.com/backend-api/codex/responses` | C3, architecture §9 |
| `POST /v1/responses/compact` | codex | `https://chatgpt.com/backend-api/codex/responses/compact` | Untested pass-through: dormant in Codex 0.162.1 (R6 §4) |
| `POST /v1/alpha/search` | codex | `https://chatgpt.com/backend-api/codex/alpha/search` | Codex web search. Sends `x-codex-turn-metadata` but no `session-id` (C8 fallback) |
| `POST /v1/images/generations`, `POST /v1/images/edits` | codex | `https://chatgpt.com/backend-api/codex/images/{generations,edits}` | Codex `images.rs:62-83`. **[V7]** |
| `GET /v1/models` without `anthropic-version` | codex | `https://chatgpt.com/backend-api/codex/models?client_version=<query value>` | **[V7]**, open question O9 |
| `POST /v1/memories/trace_summarize` | codex | — | 404 (unsupported; the Codex `memories` feature is off by default). Open question O12. |
| anything else | — | — | 404 with an error in the native shape of the path's family (C6.2), or a plain JSON error. Never forwarded. |

Later work: the API-key providers listed in [D8](decisions.md#d8) are reached through `/v1/messages`. The route is chosen by a configured model-prefix map, which needs the `model` peek allowed by C8.

## C2. Client authentication and local safety

These rules apply to the data port and the admin port alike.
- The peer address MUST be loopback.
- The `Host` header MUST be `127.0.0.1:<port>` or `localhost:<port>`. Otherwise answer 421 and log it.
- A request that carries an `Origin` header MUST be rejected with 403. Genuine CLI clients never send one, while browsers always do on cross-site requests.
- **Data port.** The client key MUST arrive as `Authorization: Bearer <key>` or as `x-api-key: <key>`, and MUST be compared in constant time.
  - A missing or wrong key gets 401 in the family's native error shape (C6.2).
- **Admin port.** The admin API is a separate listener, `127.0.0.1:<admin_port>`, with its own bearer token read from `admin.token`.
  - The admin token is never accepted on the data port, and the client key is never accepted on the admin port.
- **OAuth callback listeners** bind `127.0.0.1` and `::1` only.

## C3. Header rules

### Request to upstream

1. Copy every client header, keeping the received order where the HTTP stack allows it, **except**:
   - hop-by-hop headers: `Connection`, `Keep-Alive`, `Proxy-*`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`
   - WebSocket handshake headers, which are **per hop** in both directions: `Sec-WebSocket-Key`, `-Version`, `-Extensions`, `-Protocol` on requests, and `-Accept`, `-Extensions` on 101 responses
     - Each leg's WebSocket library generates its own.
     - Extension negotiation per leg follows [D12](decisions.md#d12).
   - `Host`, which the client library sets
   - `Authorization` and `x-api-key`, which carry the local client key
   - `Origin` (rejected anyway, C2)
2. Add the credential headers for the selected account.

| Family | Credential headers |
|---|---|
| claude (OAuth) | `Authorization: Bearer <access_token>`. In addition, `anthropic-beta` MUST contain `oauth-2025-04-20`: append it to the client's comma-separated list if it is missing, or create the header. Upstream needs this beta to accept OAuth bearer tokens. |
| codex (OAuth) | `Authorization: Bearer <access_token>` and `ChatGPT-Account-ID: <chatgpt_account_id>`. The second replaces any value the client sent. This is the header that genuine Codex sends in ChatGPT auth mode (`model-provider/src/auth.rs:105-106`). |
| API-key providers (later) | That provider's key header only |

3. Nothing else may be added, removed or rewritten. In particular, the following are left as the client sent them: User-Agent, originator, version, session or installation identifiers, beta lists (apart from the single append above), header casing, and the TLS or HTTP client configuration.

   A candidate header exception joins this table only through the same register process as C4:
   - **H2.** For Claude, add `extended-cache-ttl-2025-04-11` when the body uses 1-hour cache markers. Pending **[V1]**.
   - **H3.** For Codex, add a routing-hint header. Pending **[V3]**.

### Response to client

- Forward the status code and every header except hop-by-hop headers. Keep `Content-Encoding` and do not decode.
- Add `x-quota-baton-account: <local account id>`. This is a short local id, never an email or an upstream identifier.

## C4. Body invariance

- The bytes of every forwarded HTTP request body MUST equal the bytes received from the client. This holds for compressed and uncompressed bodies alike.
- The payload of every relayed WebSocket message MUST equal the payload received. Frames themselves are per hop: masking, fragmentation and deflate may differ.
- Response bodies and WebSocket messages are relayed unchanged.
- Explicitly forbidden: rewriting the model name; rewriting identity or metadata (`metadata.user_id`, device or installation ids); adding or removing tools, flags or fields; forcing `stream` or `store`; computing or recomputing any client signature; injecting or merging conversation history.

**Exception register.** An entry needs evidence from Phase 0, a test, and the user's approval.

| Id | Status | Rule | Gate |
|---|---|---|---|
| E1 | Candidate | Claude: when the session's account has changed since the message that `diagnostics.previous_message_id` points to, set that field to `null`. | Adopt only if **[V1]** shows that Claude Code sends the field and **[V2]** shows that the upstream rejects a field from another account. If **[V1]** shows a client signature covering the body, E1 is rejected and the issue goes back to design. |
| E2 | Candidate | Codex hand-off rewrite. Applies to requests of a session whose account changed. At the first request after the change, record the ids of the input **reasoning** items carrying `encrypted_content` as **foreign**. On that request and every later one, drop the foreign items from `input`. Compaction items are never dropped; the same rule as in E2r applies. This requires decoding the body only for such sessions. The original `Content-Encoding` is re-applied. | Adopt only if **[V2]** shows that ChatGPT accounts reject another account's encrypted items. When adopted, the cutover tool also offers plaintext compaction (`remote_compaction = "unsupported"` on the client provider), because a foreign compaction blob cannot be recovered by stripping. |
| E3 | Candidate | Claude hand-off rewrite. For a session whose account changed, drop `thinking` / `redacted_thinking` blocks minted before the change from earlier assistant turns. The current tool-use turn is exempt; if that turn is foreign, the request fails over to the original account when eligible, or else is returned as is. | Adopt only if **[V2]** shows that thinking signatures are rejected across accounts. |

| E2r | Proposed (recommended for both D10 outcomes; needs the user's approval) | Reactive safety net. A request rejected **before commit** with a signal of foreign content triggers three steps:<br>• Signals: Codex `invalid_encrypted_content` or "Request blocked"; Claude "Invalid `signature` in `thinking` block".<br>• Step 1: strip from the retained body. For Codex, every `encrypted_content`-bearing **reasoning** item. For Claude, the `thinking` / `redacted_thinking` blocks outside the current tool-use turn.<br>• **Compaction items, and any other encrypted content that is not a reasoning item (e.g. encrypted message parts), are never stripped**, because stripping could silently discard history. If the rejected body carries a foreign compaction item, the request goes back to the account that minted it, when that account is known and eligible. The session binding is then moved back to that account. Otherwise the 400 is relayed and the tray shows an actionable notice: resume the session after the original account resets, or start a new one.<br>• Step 2: retry once on the **same** account.<br>• Step 3: record the session's foreign-id set, so later requests are stripped the same way (E2/E3 semantics).<br>On WebSocket the frame is not relayed. The socket closes with 1012, and the strip applies to the reconnect's full input. | Needed in both D10 branches. It recovers sessions whose binding memory was lost (LRU, crash, fork) and covers changes in server behaviour. Lossy: earlier reasoning continuity is lost. A body without a retained copy (C6 row 17) is returned as is. |

Hand-off rewrites keep bounded state. Per session that is the switch point and the foreign item ids, a few KiB, persisted write-behind in `state/bindings.json`. They never retain conversation content.

Decoding a body for a hand-off rewrite uses transient memory, counted against the in-flight budget. The original `Content-Encoding` is re-applied.

## C5. Account state

Each account has two independent fields:
- `user_state` ∈ {`active`, `paused`, `disabled`}. Only the user changes it.
- `health` ∈ {`ready`, `cooling{reason, until, scope}`, `needs_relogin{reason, since}`}. Only upstream signals change it, plus the expiry of `until` and a completed re-login.

`scope` is either `account` or `model:<model>`.

`reason` is one of:
- `quota` (usage window exhausted, observed or rejected)
- `rate` (short rate limit)
- `auth` (a 401 that persists after a successful refresh)
- `permission` (403 with a known model)
- `payment` (402)
- `store_error` (credential persist failure)

**An account is eligible for a request when all of these hold:**
- `user_state == active`
- `health == ready`, or `cooling` that is scoped to a model other than the request's model, where that model is known
- a credential is present
- the account is not in the request's tried set

| From | Event | To |
|---|---|---|
| any health | Terminal refresh error: `refresh_token_invalidated`, `refresh_token_reused`, `refresh_token_expired`, `refresh_token_revoked`, `invalid_grant`, or any 401 from the token endpoint | `needs_relogin` |
| any health | Upstream 401 carrying a revocation or invalidation signal (C6 row 4) | `needs_relogin` |
| `ready` | Upstream 401 with the **current** token, a refresh succeeds, then a 401 again with the **new** token | `cooling(auth, until=now+15m, account)`, doubling on each repeat up to 4 h. A probe request runs when it expires. |
| — | Upstream 401 with a token that is **no longer current** (another request already refreshed) | **no change**. The request retries once with the current token. |
| `needs_relogin` | Re-login completes for the same identity | `ready` |
| `ready` | Usage limit rejection (C6 row 2) | `cooling(quota, until=reset_at, scope)` |
| `ready` | Usage snapshot shows a window ≥ 100 % or `limit_reached`, or a Codex account is serving from credits and `allow_credits=false` | `cooling(quota, until=reset_at, account)` |
| `ready` | Rate limit with `Retry-After` > 10 s (C6 row 3) | `cooling(rate, until=now+retry_after, model or account)` |
| `ready` | 403 when the request's model is known (C6 row 6) | `cooling(permission, until=now+1h, model:<model>)` |
| — | 403 when the model is unknown (always the case for Codex) | **no account-wide change**. Handled per request (C6 row 6). |
| `ready` | 402 | `cooling(payment, until=now+6h, account)` |
| `ready` | Persisting rotated tokens fails | `cooling(store_error, until=now+5m, account)`, while the write keeps retrying. The tray shows a critical warning. |
| `cooling` | `until` passes | `ready` (`auth`: after a successful probe) |
| `cooling(quota)` | A poll shows a different `reset_at` and every window < 100 % (early reset). Cooling accounts are polled at least hourly (C9). | `ready` |
| — | 404, 400, 409, 413, 422, 5xx, 529, 507, network errors, anything during a takeover drain | **no change** |

`user_state` transitions come from admin actions: pause, resume, disable, enable. A `disabled` account is neither selected nor polled.

`health` and `user_state` are persisted in the account file and survive restarts. A `cooling` whose `until` has already passed when it is loaded becomes `ready`.

## C6. Upstream outcome mapping

**Terms.**
- **Commit** is the first upstream event that is not an error. For HTTP, response headers and leading events wait in a bounded hold until then (architecture §5 step 7). For WebSocket, see §9.
- **Failover** means re-running C7 without the accounts already tried, using the retained copy.
- **Attempt limits.** A request tries at most 4 distinct accounts. Each account gets at most one extra same-account retry, either the token retry of row 5 or the network retry of row 11. Those retries do not count as a new account.
- **Before acting on a decision** (a failover, or a WebSocket close), the health change is recorded first, so the next selection cannot pick the same account again.

| # | Upstream condition | Before commit | Client sees | Health change |
|---|---|---|---|---|
| 1 | 2xx | — | Relayed stream or body | Quota snapshot updated from headers (C9) |
| 2 | Usage limit: Claude 429 with unified `5h` or `7d` status `rejected` (sub-bucket only, e.g. `7d_oi`, means model scope); Codex 429 or error type `usage_limit_reached`, or `x-codex-limit-reached: true` | Failover | Next account's response; if none is eligible, row 14 | `cooling(quota, until=reset)` |
| 3 | Other 429 (rate limit without a window rejection) | Failover only if `Retry-After` > 10 s | Upstream response as is, or the next account's | Row 3 of C5 if > 10 s; otherwise none |
| 4 | 401 whose body signals revocation: Codex "invalidated oauth token" text or code `token_revoked`, or an Anthropic `authentication_error` saying the token was revoked | Failover | Next account's response; if none, row 15 | `needs_relogin` |
| 5 | Other 401 | If the failed token is not current, retry once with the current token. If it is current, do a single-flight refresh and retry once. A terminal refresh error, or a 401 with the new token, triggers failover. | Next account's response; if none, row 14 or row 15 | Per C5: `needs_relogin` on a terminal refresh error; `cooling(auth)` on a 401 with the new token |
| 6 | 403 | Failover. If a **second** account also returns 403 for the same request, stop and relay that response. | Next account's response, or the second 403 as is | Known model: `cooling(permission, model)`. Unknown model: none. |
| 7 | 402 | Failover | Next account's response; if none, upstream response as is | `cooling(payment)` |
| 8 | 404 | No failover | Upstream response as is | **None** |
| 9 | 400, 409, 413, 422 | No failover | Upstream response as is | None |
| 10 | 408, 5xx, 529, 507, `overloaded_error`, `server_is_overloaded` | No failover | Upstream response as is (clients run their own backoff) | None |
| 11 | Connect, TLS or DNS failure, header timeout, or the upstream closes before any byte | One retry on the same account after 250 ms, then give up | 502 in native shape (C6.2) | None |
| 12 | Error after commit (SSE `error` or `response.failed` event, connection drop) | **Never retried** | Relayed as is, then the stream ends | Signals inside the event apply (e.g. `usage_limit_reached` leads to `cooling(quota)`) |
| 12a | An error **event** before commit, e.g. a first SSE event that is `error` or `response.failed` | Treated like the matching status row (2–7, 10) | As that row | As that row |
| 12b | Pre-commit 400 signalling foreign content | Only if E2r (C4) is adopted: strip and retry once on the same account. Otherwise as row 9. | That retry's response, or the 400 as is | None (if E2r is adopted, the session's foreign-id set is recorded) |
| 13 | WebSocket upgrade | Dial upstream first. Classify the handshake response with rows 2–7 and 11, and fail over before answering the client. | 101 with an open upstream; otherwise row 14, 15, or 502 for a pure network failure | As in those rows |
| 13a | WebSocket: before commit, an `error` from rows 2 or 4–7 answers a `response.create` | Do **not** relay the frame. Record the health change, then close the client socket with 1012. Codex reconnects with full input, and row 13 selects another account. | No error; the turn continues on another account | As in rows 2–7 |
| 13b | WebSocket: an error frame after commit, or any other error | — | Frame relayed as is | As in rows 2–12 |
| 13c | WebSocket: the pinned account becomes ineligible while the socket is idle | — | Client socket closed (1000); Codex reconnects silently on its next request | — |
| 14 | No eligible account, and every candidate is in `cooling(quota, rate or auth)`. A poll of every candidate runs first (C9). | — | **HTTP:** 429. Replay the cached genuine limit response of the account whose reset comes soonest (status, body and limit headers, at most 64 KiB, with the original `Content-Encoding` kept), with `retry-after` set to the remaining seconds. Without a cache, use the synthesized shape (C6.2). **WebSocket upgrade:** native 429 with a `usage_limit_reached` body, which is terminal for Codex. | — |
| 15 | No usable account at all (`needs_relogin`, `paused`, `disabled`, or none configured) | — | **400** in native shape, type `invalid_request_error`, message "quota-baton: no usable <provider> account — open the tray". It is terminal in both clients, so there are no retry storms and no sticky HTTP fallback. The user has to act anyway. | — |
| 16 | Memory guard tripped | — | 503 with `Retry-After: 5` | — |
| 17 | In-flight budget exhausted, or the body exceeds the per-request cap | The request is forwarded **without** a retained copy, so it has no failover | Upstream response as is | — |

### C6.2 Synthesized error shapes

These are used only when no genuine upstream response is available. They MUST be checked against cached real responses during Phase 0.

| Family | Shape |
|---|---|
| claude | `{"type":"error","error":{"type":"<rate_limit_error\|api_error\|authentication_error\|invalid_request_error>","message":"..."}}` |
| codex (HTTP, and the body of a WebSocket upgrade answer) | `{"error":{"type":"<usage_limit_reached\|invalid_request_error\|server_error>","message":"...","resets_at":<unix>,"resets_in_seconds":<n>}}`. `resets_*` appears only for `usage_limit_reached`. |
| codex (WebSocket) | `{"type":"error","status":<code>,"error":{...same...}}`. `status` is mandatory, because Codex ignores error frames without it. |

Rules for answering a WebSocket upgrade:
- quota-baton MUST NOT itself answer an upgrade with 426. Codex treats 426 as a permanent switch of that session to HTTP.
- An upgrade answered with 429 `usage_limit_reached`, or with 400, is terminal for Codex (`responses_websocket.rs:559-571`, `api_bridge.rs:175-206`).
- A 502 is retryable and consumes one of Codex's 5 retries.

## C7. Selection policy

Input: the family, the request's session key with its optional parent key (C8), the model if known, and the set of accounts already tried.

1. If the session key is bound to account A, and A is eligible and not yet tried, use A.
2. Else, if the parent key is bound to account P, P is eligible, and P has not been tried, bind the session to P and use it.
3. Else, let `E` be the eligible accounts that have not been tried.
   - Let `S` be the accounts in `E` whose every known usage window is below `soft_threshold_pct` (default 90).
   - Let `pool` be `S` if it is non-empty, otherwise `E`.
4. Order `pool` by these keys, in sequence:
   1. `priority`, ascending (user setting, default 0)
   2. the soonest `reset_at` of the 7-day window, with unknown values sorting last
   3. the 5-hour `used_pct`, ascending
   4. account id
5. Take the first account in that order and bind the session (and the parent, if it was unbound) to it.
6. If `E` is empty, apply C6 row 14 or row 15.

Further rules:
- **Thresholds never move a session.** Usage thresholds MUST NOT move a bound session. A session moves only when its account fails the eligibility test in C5, or at a cold point (below).
- **`last_used` updates.** Every HTTP request and every relayed WebSocket `response.create` updates the binding's `last_used`. `last_used` is set to the end of the response.
- **Pinned sockets keep their binding.** A binding referenced by an open pinned WebSocket is never expired or evicted.
- **Cold-point rebalance.** A binding idle (`now - last_used`) for at least `rebalance_idle_secs` (default 3600) is dropped. The session's next request is then selected as new (steps 3–5).
  - At that age the provider prompt cache is already cold. The Claude 1-hour TTL is the longest that Claude Code uses, and Codex's in-memory retention is shorter. So a move back to the preferred account costs no extra cache miss.
  - `[websocket] idle_close_secs` MUST be smaller than `rebalance_idle_secs`.
  - This rule applies **only if [D10](decisions.md#d10) resolves to "portable"**. Otherwise bindings last `session_binding_max_secs` (default 7 days), and a session never moves except when forced.
- **Persistence.** Bindings and hand-off foreign-id sets are persisted write-behind, at most 60 s late, to `state/bindings.json`. They are flushed on drain and on exit. LRU capacity is 20 000. They are exported and imported on takeover (C11).
- **Unbound requests** (no key, C8) stay on the family's **last-used account** until it becomes ineligible. Selection then runs once and the result becomes the new last-used account. They never move per request.
- **WebSocket selection** runs once per connection, on the upgrade (C6 row 13). The connection keeps that account until it closes.

## C8. Session keys and the allowed body peek

### Claude

- `sid` = `X-Claude-Code-Session-Id`
- `aid` = `X-Claude-Code-Agent-Id`
- `paid` = `X-Claude-Code-Parent-Agent-Id`

| Condition | Key | Parent |
|---|---|---|
| `aid` absent or `main` | `claude:{sid}` | none |
| otherwise | `claude:{sid}:agent:{aid}` | `claude:{sid}:agent:{paid}` if `paid` is present and not `main`, else `claude:{sid}` |

### Codex

- Key = `codex:{session-id}`. The value comes from the header `session-id`, which also matches `session_id` and `Session-Id`.
  - Codex 0.162.1 sends the **root** session id in this header for subagents as well. It equals the root `prompt_cache_key`.
  - So one key binds the root thread and all of its subagents to the same account. No parent logic is needed. (Source: R6 §3.1.)
- Fallback: `session_id` from the JSON in the `x-codex-turn-metadata` header.

### Fallbacks, unbound requests and the allowed body peeks

**Claude:** `X-Claude-Code-Session-Id` is unverified for genuine Claude Code sending to a custom base URL. V1 records whether it is present.
- **Fallback 1.** When the header is absent, a bounded **tail scan** of the body looks for `metadata.user_id` and takes `session_id` from it.
  - The scan covers at most the last 64 KiB, and only if the body is identity-encoded. It allocates no DOM.
  - In Claude Code bodies, `metadata` comes near the end, before `max_tokens` (R3 §3.3).
  - The key is then `claude:{session_id}`.
- **Fallback 2.** The request is **unbound** and follows the C7 unbound rule (stays on the family's last-used account).

**Codex:**
- **Fallback.** Take `session_id` from the JSON in the `x-codex-turn-metadata` header. This is needed for `/v1/alpha/search`, which sends turn metadata but no `session-id` (`ext/web-search/src/tool.rs:195-207`).
- Guardian reviewer sessions also send the root `session-id` (`core/src/client.rs:595-603`). They therefore share their parent's account, though their prompt-cache key differs.

**Allowed body peeks**, for Claude `/v1/messages` only:
- Fallback 1 above.
- A head scan of at most 64 KiB for the top-level `"model"`.

Both apply only to identity-encoded bodies, allocate no DOM, and never feed into rewriting.

**Codex bodies** are never decoded on the normal path. The two exceptions:
- the hand-off rewrites in C4 (E2, E2r)
- the `shape-capture` build (architecture §13)

## C9. Usage signals

Every source normalizes into the account snapshot `windows[]` (`name`, `used_pct`, `reset_at`, `status`), plus `plan`, `credits`, `source` and `observed_at`. The value formats below come from the official clients and must be confirmed in **[V1]**, **[V3]** and **[V5]**.

| Source | Fields |
|---|---|
| Claude response headers | `anthropic-ratelimit-unified-{5h,7d,7d_oi}-{utilization,reset,status}` (utilization is 0..1; reset is a unix time), `-status`, `-reset`, `-representative-claim`, `-overage-status`, `retry-after` |
| Codex response headers | `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-after-seconds,reset-at}`; the window is named from its minutes (300 is `5h`, 10080 is `7d`, any other value is `<n>m`); `x-codex-plan-type`, `x-codex-limit-reached`, `x-codex-active-limit`, `x-codex-credits-{has-credits,unlimited,balance}` |
| Codex WebSocket event `codex.rate_limits` | `rate_limits.{primary,secondary}.{used_percent,window_minutes,reset_after_seconds\|reset_at}`, `limit_reached`, `credits`, `plan_type` |
| Codex poll | `GET https://chatgpt.com/backend-api/wham/usage` with `Authorization`, `ChatGPT-Account-Id` (this casing, as Codex's backend client sends it, `backend-client/src/client.rs:277-278`), `Accept: application/json` and `User-Agent: quota-baton/<ver>`. Response: `rate_limit.{limit_reached, primary_window{used_percent,reset_at}, secondary_window{...}}`, `plan_type`, `credits` **[V5]** |
| Claude poll | `GET https://api.anthropic.com/api/oauth/usage` with `Authorization` and `anthropic-beta: oauth-2025-04-20`. Expected: `five_hour{utilization,resets_at}`, `seven_day{...}` **[V5]**, unverified |

Polling rules (architecture §8):
- A `cooling(quota)` account is polled at least hourly.
- Every candidate is polled immediately before C6 row 14 or row 15 is answered.
- The minimum interval per account is 5 minutes.
- Polls never run for `needs_relogin` or `disabled` accounts.

Inspecting compressed responses:
- Response headers are always readable.
- To inspect a compressed SSE stream or error body, the relay decodes a bounded side copy as a stream. Relayed bytes are never changed.

## C10. OAuth parameters

These are the public CLI client registrations. They are the official CLIs' public values and are **verified by [V6]**. quota-baton's own calls send `User-Agent: quota-baton/<ver>`.

| | Claude | Codex (ChatGPT) |
|---|---|---|
| Authorize URL | `https://claude.ai/oauth/authorize` | `https://auth.openai.com/oauth/authorize` |
| Token URL | `https://platform.claude.com/v1/oauth/token` (JSON body) | `https://auth.openai.com/oauth/token` (form body) |
| client_id | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | `app_EMoamEEZ73f0CkXaXp7hrann` |
| Scope | `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload` | `openid email profile offline_access` (refresh uses `openid profile email`) |
| Redirect | `http://localhost:54545/callback` | `http://localhost:1455/auth/callback` (fixed port) |
| Extra authorize parameters | `code=true` | `prompt=login`, `id_token_add_organizations=true`, `codex_cli_simplified_flow=true` |
| PKCE | S256, verifier of at least 32 random bytes, base64url | same |
| Identity | `GET https://api.anthropic.com/api/oauth/profile`, giving `account.uuid`, `account.email`, `organization.uuid` | id_token claims (signature not verified; identity only): `email`, and under `https://api.openai.com/auth` the claims `chatgpt_account_id`, `chatgpt_plan_type`, `chatgpt_user_id` |
| Refresh body | `{grant_type: refresh_token, refresh_token, client_id, scope}` | `client_id, grant_type=refresh_token, refresh_token, scope` |
| Identity key for re-login matching | `organization.uuid` + `account.uuid` | `chatgpt_account_id` + `chatgpt_user_id` |

The callback servers MUST bind only the loopback addresses `127.0.0.1` and `::1`. The OAuth `code` and the callback query string MUST be redacted from logs. Each login session has a random `state`, which MUST match on callback, and expires after 10 minutes.

## C11. Admin API

Base: `http://127.0.0.1:<admin_port>/admin/v1`. Every route requires `Authorization: Bearer <admin.token>` and passes the C2 Host and Origin checks. Requests and responses are JSON.

| Method and path | Purpose |
|---|---|
| `GET /health` | `{ok, version, pid, started_at}` |
| `GET /state` | `{accounts:[AccountView], inflight:{requests, bytes}, memory:{private_bytes}, bindings:{count}, data_port, version}` |
| `GET /events` | SSE stream of `account_changed`, `quota_updated`, `core_warning` and `login_progress` |
| `POST /accounts/login` `{provider}` | Returns `{login_id, auth_url}`. The core runs the callback listener. |
| `GET /accounts/login/{login_id}` | `{status: pending\|completed\|failed\|expired, account_id?, error?}` |
| `POST /accounts/login/{login_id}/cancel` | Cancels the login |
| `POST /accounts/{id}/{pause\|resume\|disable\|enable}` | Changes `user_state` |
| `PATCH /accounts/{id}` `{label?, priority?, allow_credits?}` | Edits account settings |
| `DELETE /accounts/{id}` | Removes the account file. This does not revoke the account upstream. |
| `POST /accounts/{id}/refresh-usage` | Polls usage now, subject to the minimum interval |
| `POST /lifecycle/drain` | Takeover handover, in this order:<br>1. stop refresh-ahead and 401-triggered refresh (a 401 during drain is relayed without refresh or health change)<br>2. stop writing account and state files<br>3. flush bindings<br>4. close the data listener<br>5. **release the credential lease**<br>6. return `{bindings:[...], lease_released:true}`<br>In-flight streams continue until done or `drain_timeout_secs`. |
| `POST /lifecycle/shutdown` `{drain_timeout_secs}` | Graceful exit; flushes bindings |
| `POST /debug/pin` `{session_key?, account_id}` / `DELETE /debug/pin` | **Phase 0 only.** Forces selection for that session key, or for all sessions if the key is omitted, to the given account, overriding C7. Logged at warn level and shown in the tray. Cleared on restart. |

`AccountView` fields:
- `id`, `provider`, `label`, `email`, `plan`
- `user_state`, `health` (including `reason`, `until` and `scope`)
- `windows[]`, `credits`
- `last_used_at`, `bound_sessions`
- `token_expires_at`

Tokens are never included.

## C12. Persistence formats

### `config.toml`

All keys are optional. The defaults are shown.

```toml
[server]
data_port = 10200            # 10100 after cutover
admin_port = 10201

[routing]
soft_threshold_pct = 90
rebalance_idle_secs = 3600          # cold-point rebalance (only when D10 = portable)
session_binding_max_secs = 604800   # binding lifetime when D10 = not portable

[guard]
retain_max_per_request_mib = 64   # larger bodies are forwarded without retention (no failover)
inflight_budget_mib = 384         # shared: retained HTTP bodies + in-flight WS messages + hand-off decode buffers
memory_hard_limit_mib = 1024

[websocket]
idle_close_secs = 1800            # MUST be < routing.rebalance_idle_secs
max_message_size_mib = 192        # both legs; max_frame_size is set to the same value (Codex sends single-frame messages)

[quota]
poll_min_interval_secs = 300

[accounts.<id>]              # per-account overrides
priority = 0
allow_credits = false
```

### `accounts/<id>.json`

```json
{ "schema": 1, "id": "cx-1a2b3c", "provider": "codex",
  "label": "...", "email": "...", "plan": "plus",
  "identity": { "...provider ids per C10..." },
  "user_state": "active",
  "health": { "kind": "ready" },
  "generation": 7,
  "token_meta": { "expires_at": "...", "last_refresh_at": "...", "refresh_uncertain": false },
  "secret_dpapi_b64": "..." }
```

The DPAPI blob decrypts to `{access_token, refresh_token, id_token?}`. Files are written atomically and generation-guarded (architecture §6), and only by the holder of the credential lease.

`refresh_uncertain` lifecycle:
- It is set to `true` and persisted **before** a refresh request is sent.
- It is cleared in the same write that persists the new tokens.
- If it is found `true` at startup, the previous refresh outcome is unknown. Retry at the next refresh-ahead point (architecture §6).

### `state/bindings.json`

`{ "schema": 1, "bindings": [{ "key": "<session key>", "account": "<id>", "last_used": "...", "foreign": [{ "minted_by": "<account id or null>", "ids": ["<item id>", ...] }] }] }`

`minted_by` is the account the session was bound to before the switch, when it is known. The compaction route-back in E2r uses it. When it is `null`, route-back is impossible, so the 400 is relayed with a tray notice.

It is written behind, at most 60 s late, and flushed on drain and exit. It holds no conversation content. Foreign `ids` are opaque item ids or signature hashes, never content.

### Ledger line

```json
{"ts":"...","req":"...","family":"codex","route":"POST /v1/responses","transport":"http",
 "account":"cx-1a2b3c","session":"<sha256/12>","model":null,
 "originator":"codex_cli_rs","ua":"codex_cli_rs/0.162.1",
 "status":200,"upstream_status":200,"ttfb_ms":412,"dur_ms":38120,
 "bytes_in":812345,"bytes_out":91234,"attempts":1,"outcome":"ok"}
```

## C13. Acceptance

See [architecture §16.3](architecture.md#163-acceptance-criteria) (A1–A5). Every row of C5 and C6 MUST have a `qb-testkit` test before M4a/M4b are accepted.
