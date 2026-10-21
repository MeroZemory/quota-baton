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
   - `x-openai-actor-authorization`, including all duplicates and casing variants. D23's `qb-local` value is a local image-tool compatibility sentinel, never upstream authorization and never a substitute for C2 authentication.
   - `Origin` (rejected anyway, C2)
2. Add the credential headers for the selected account.

| Family | Credential headers |
|---|---|
| claude (OAuth) | `Authorization: Bearer <access_token>`. In addition, `anthropic-beta` MUST contain `oauth-2025-04-20`: append it to the client's comma-separated list if it is missing, or create the header. Upstream needs this beta to accept OAuth bearer tokens. |
| codex (OAuth) | `Authorization: Bearer <access_token>` and `ChatGPT-Account-ID: <chatgpt_account_id>`. The second replaces any value the client sent. This is the header that genuine Codex sends in ChatGPT auth mode (`model-provider/src/auth.rs:105-106`). |
| API-key providers (later) | That provider's key header only |

3. Nothing else may be added, removed or rewritten. In particular, the following are left as the client sent them: User-Agent, originator, version, session or installation identifiers, beta lists (apart from the single append above), header casing, and the TLS or HTTP client configuration.

   **Known limitation (M2 review S4):** the HTTP client library inserts `accept: */*` when a request has no `Accept` header. Genuine Claude Code and Codex always send `Accept`, so this is accepted until evidence shows an effect.

   A candidate header exception joins this table only through the same register process as C4:
   - **H2.** For Claude, add `extended-cache-ttl-2025-04-11` when the body uses 1-hour cache markers. Pending **[V1]**.
   - **H3.** Withdrawn (2026-10-13). V3 passed without a routing hint.

### Request header read and removed (dashboard label, R6)

- `x-quota-baton-label`: an optional, client-side session label (the launcher's folder name). The core reads it into memory for the dashboard and **removes every occurrence before capture or forwarding**, including HTTP retries and WebSocket handshakes. Duplicate or invalid values supply no label. The launcher encodes a UTF-8 basename as `qb1:<base64url without padding>` so Korean names fit ASCII header APIs; a bounded plain label is also accepted. The decoded basename lives in memory only and is never persisted or logged.
- `x-quota-baton-project`: an optional registered local project ID (32 lowercase hex digits). All occurrences are removed before capture or upstream forwarding; duplicates/invalid values are rejected. It associates a classified root session with a project policy before first selection. An unknown ID, absent session identity or conflicting existing association is rejected. It is separate from display labels and grants no credit/credential permission. Only authenticated successor handover may carry the validated ID between local cores.

### Response to client

- Forward the status code and every header except hop-by-hop headers. Keep `Content-Encoding` and do not decode.
- Add `x-quota-baton-account: <local account id>`. This is a short local id, never an email or an upstream identifier.
- On an HTTP rate 429 relayed after the rate hold (C6.3), add `retry-after` when it is absent. Use integer seconds, computed after jitter, at least 1 and never above 60; the Claude SDK ignores larger values. This cannot be done for a WebSocket error frame, because frames are payload (C4).

## C4. Body invariance

- The bytes of every forwarded HTTP request body MUST equal the bytes received from the client. This holds for compressed and uncompressed bodies alike.
- The payload of every relayed WebSocket message MUST equal the payload received. Frames themselves are per hop: masking, fragmentation and deflate may differ.
- Response bodies and WebSocket messages are relayed unchanged.
- Explicitly forbidden: rewriting the model name; rewriting identity or metadata (`metadata.user_id`, device or installation ids); adding or removing tools, flags or fields; forcing `stream` or `store`; computing or recomputing any client signature; injecting or merging conversation history.

**Exception register.** An entry needs evidence from Phase 0, a test, and the user's approval.

| Id | Status | Rule | Gate |
|---|---|---|---|
| E1 | Withdrawn (2026-10-13) | Null a foreign `diagnostics.previous_message_id` | V1: genuine Claude Code does not send `diagnostics`. |
| E2 | Withdrawn (2026-10-13) | Strip foreign Codex reasoning items | In the tested V2 HTTP cases, the request completed with token counts consistent with the items being ignored. Genuine Codex replays them verbatim, too. This is not a universal compatibility guarantee. |
| E3 | Withdrawn (2026-10-13) | Strip foreign Claude thinking blocks | Tested V2 HTTP cases completed, including inside a tool-use turn. Stripping is itself a history edit, which preserved-thinking models may reject. Acceptance and preservation remain model/path dependent. |
| E2r | Withdrawn (2026-10-13) | Strip and retry after a foreign-content rejection | The tested V2 cases did not establish a need for this rewrite. They do not prove foreign-content rejection impossible. Compaction cannot generally be stripped to recover missing history; sticky bindings reduce moves but cannot guarantee remaining on the minting account. |
| E4 | Accepted under D23 (2026-10-21) | The image routes generate operation-specific OpenAI-shaped local errors for conclusively classified authentication, quota and unsupported-endpoint failures. They preserve request bytes, successful responses, moderation errors and unclassified/incompletely inspected errors. | D23's approved error taxonomy; r12_images classified-failure, moderation pass-through and incomplete-error regressions. |

There are no conversation-payload exceptions. Encrypted and signed conversation items are relayed exactly as the client sent them, which is what the genuine clients themselves do ([D10](decisions.md#d10)). This preserves pass-through correctness; it does not certify cross-account resumption, cache reuse or preserved reasoning.

## C5. Account state

Each account has two independent fields:
- `user_state` ∈ {`active`, `paused`, `disabled`}. Only the user changes it.
- `health` ∈ {`ready`, `cooling{reason, until, scope}`, `needs_relogin{reason, since}`}. Only upstream signals change it, plus the expiry of `until` and a completed re-login.

`scope` is either `account` or `model:<model>`.

`reason` is one of:
- `quota` (usage window exhausted, observed or rejected)
- `rate` (reserved; data-plane rate limits use pacing instead, C6.3)
- `auth` (a 401 that persists after a successful refresh)
- `permission` (403 with a known model)
- `payment` (402)
- `store_error` (credential persist failure)
- `link_stale` (a linked import whose CPA file has no newer token by expiry; D5)

**An account is eligible for a request when all of these hold:**
- `user_state == active`
- `health == ready`, or a `cooling` scoped to a model **other than** the request's model
  - **Unknown model** (M4a review S4): a model-scoped cooling does **not** exclude the account.
    - This covers Codex HTTP, any WebSocket upgrade, and messages over 1 MiB.
    - If the account then rejects the model again, row 6 or row 6a fails over as usual.
  - Only account-scoped states exclude an account whose model is unknown.
- not in the **reconnect exclusion** of the request's session key (C6 row 13a)
- a credential is present
- the account is not in the request's tried set

| From | Event | To |
|---|---|---|
| any health | Terminal refresh error: `refresh_token_invalidated`, `refresh_token_reused`, `refresh_token_expired`, `refresh_token_revoked`, `invalid_grant`, or any 401 from the token endpoint | `needs_relogin` |
| any health | Upstream 401 carrying a revocation or invalidation signal (C6 row 4), on an **owned** account | `needs_relogin` |
| any health | **Linked account, any 401** (rows 4 and 5, data plane or usage poll; V5 2026-10-13) | Re-read the CPA file first. A newer token means retry with it, and no health change. No newer token means `cooling(link_stale)`. A linked account **never** enters `needs_relogin`, because only CPA can re-login it. Anthropic revokes the old access token as soon as CPA refreshes, so this case is normal. |
| `needs_relogin` or `cooling(link_stale)` (linked) | A CPA re-import or linked reload finds a newer token for the same identity | `ready` |
| `cooling(link_stale)` or `ready` (linked) | Adoption (`POST /accounts/{id}/adopt`, D17) | `ready`, `source.mode=adopted`; `auth_failures=0`, `auth_probe_pending=false`. The id is unchanged. |
| `ready` | Upstream 401 with the **current** token, a refresh succeeds, then a 401 again with the **new** token | `cooling(auth, until=now+15m, account)`, doubling on each repeat up to 4 h. A probe request runs when it expires. |
| — | Upstream 401 with a token that is **no longer current** (another request already refreshed) | **no change**. The request retries once with the current token. |
| `needs_relogin` | Re-login completes for the same identity | `ready` |
| `ready` | Usage limit rejection (C6 row 2) | `cooling(quota, until=reset_at, scope)` |
| `ready` | Usage snapshot shows an **account window** (`5h`, `7d`, Codex primary or secondary) at 100 % or more, or `limit_reached`, except permitted Codex credit continuation below; or a Codex account is serving from credits and `allow_credits=false` | `cooling(quota, until=<reset_at of the limiting window>, account)`. The limiting window is the one at or over its limit (for credits: **observed** ≥ 99 %, or the one named by `x-codex-active-limit`; a projection never cools, C7). Never use the latest reset across all windows (M4a review M1, M2). If several windows are at their limit, use the **latest** of *those* resets. If no window qualifies, use the soonest known reset. |
| `ready` | A **sub-bucket** window (e.g. Claude `7d_oi`) at 100 % or more | `cooling(quota, until=<its reset>, model:<the request's model>)`. With the model unknown, no change. Sub-buckets never cool the whole account (M4a review S5; matches row 2). |
| — | Request-rate limit (C6 row 3), whatever its `Retry-After` | **no health change**. Per-account pacing only (C6.3). Data-plane 429s no longer produce `cooling(rate)` (D14, review M5). |
| `ready` | 403 when the request's model is known (C6 row 6) | `cooling(permission, until=now+1h, model:<model>)` |
| — | 403 when the model is unknown (always the case for Codex) | **no account-wide change**. Handled per request (C6 row 6). |
| `ready` | Billing rejection: 402, or a pre-commit 400 whose `error.type` is `billing_error` (C6 row 7) | `cooling(payment, until=now+6h, account)`, doubling on each repeat up to 24 h. Only that account changes. It clears only when `until` passes. A usage poll or a small successful request never clears it, because a small request proves nothing about funding a large-context one (C6.4). |
| `ready` | Persisting rotated tokens fails | `cooling(store_error, until=now+5m, account)`, while the write keeps retrying. The tray shows a critical warning. |
| `cooling` | `until` passes | `ready` (`auth`: after a successful probe) |
| `cooling(quota)` | **Early reset** (M4a review S3). A **fresh** observation, newer than the cooling's start, of the window that caused the cooling shows `used_pct` < 100 **and** a `reset_at` that differs by more than 300 s from the one recorded when the cooling started. Cooling accounts are polled at least hourly (C9). | `ready` |
| `cooling` (any) | A new cooling event arrives | The **later** `until` wins, and an account-scoped cooling is never replaced by a model-scoped one (M4a review S2) |
| — | 404, 400, 409, 413, 422, 5xx, 529, 507, network errors, anything during a takeover drain | **no change** |

**Codex credit entitlement and subscription observations.** `allow_credits` is permission, never proof of funds. Unlimited credits are available; otherwise an explicit `has_credits=false` or a finite balance at or below zero means no credits. With neither negative signal, `has_credits=true` or a positive balance means available; missing evidence remains unknown. A Codex account with permission and available credits (or process-local entitlement proven by a successful paid response, with no explicit absence) is not cooled solely because a subscription window reaches 100 % or a usage snapshot reports `limit_reached=true`. The snapshot flag alone is subscription evidence, not an actual request rejection (R12.9 Update 2, 2026-10-21). Explicit upstream request quota rejection, rejected status, auth and payment holds still apply. Zero-length or opaque unknown Codex windows never cause a window hold. Explicit provider `primary`/`secondary` quota evidence and its reset remain authoritative under the C5 table even when that response omits duration; they never invent a display window. The observed 99 % preventive hold applies only to credit-capable Codex accounts with permission off, or an observed paid-serving claim with permission off.

Account metadata records `quota_credit_releasable` separately from optional window/reset evidence (missing in old records means false). Only subscription observations or disallowed-credit observations set it true. An actual upstream quota rejection permanently makes the current hold non-releasable by credit settings; later numeric snapshots must not reclassify that hold as releasable. Enabling credits or observing newly available credits may release only a releasable hold, after reclassification confirms no remaining quota rejection. Existing model, auth and payment holds are preserved. An authenticated operator may explicitly clear a quota hold for a probe with `POST /admin/v1/accounts/{id}/release-hold` and `{ "confirm": "<id>" }`, including an old hard hold. Other health classes, tokens and user settings remain unchanged. The CLI requires typed account-id confirmation; the tray offers the action for a quota-cooled account with available credits.

The operator probe supersedes older snapshot quota evidence for selection and health reduction, without rewriting usage history. Numeric windows and rejection statuses retain separate freshness. It is single-flight until a serving request succeeds (a successful usage poll does not complete it); cancellation permits another attempt. Fresh observations and actual rejections still apply, and credit permission is never bypassed. A known paid-serving claim with credit permission off refuses explicit release. This override is process-local; after restart old evidence may require another explicit release.

Credit snapshots merge once, in observation order, and the exact merged result is persisted. Partial headers and stream events preserve missing windows; complete polls may replace the window set. New explicit absence invalidates old finite balances and old unlimited flags. A new entitlement flag does not inherit a contradictory old balance, and a new numeric balance does not inherit old entitlement flags. Restored legacy snapshots are normalized by the same rules. A fresh non-credit active limit or explicit paid-serving false ends a prior paid-serving claim; explicit credit absence and turning permission off also close the dashboard paid span. Credit capability or a poll alone never starts actual paid-serving display. Success proof is kept outside the serialized snapshot; missing metadata can use it for continued selection, but restart cannot restore it. Permission off closes display/history without erasing entitlement; a later on still needs fresh success to show actual paid use. Explicit absence, a non-credit serving transition, and recovery of all exhausted subscription windows remove process-local proof; a reset of only one window preserves proof while another remains at its observed credit threshold.

`user_state` transitions come from admin actions: pause, resume, disable, enable. A `disabled` account is neither selected nor polled.

`health` and `user_state` are persisted in the account file and survive restarts. A `cooling` whose `until` has already passed when it is loaded becomes `ready`.

**WebSocket connection capacity (R12.9).** A `400 websocket_connection_limit_reached` is a WS-only per-account capacity observation, not a health transition. Pending dials and live upstream sockets reserve slots atomically. A rejected dial learns a conservative cap from the other reserved sockets (minimum one probe slot); an in-stream error learns from the current reserved count and is still relayed immediately. New upgrades exclude full accounts and may use another eligible account under the session policy; strict anchors wait. If every usable account is full, wait only inside the existing WS upgrade deadline. Dropping a pending or live socket releases its reservation. With no local sockets, a rejected dial backs off 250 ms before probing. The cap is process-local and independent of quota and request-rate pacing. In-stream ledger rows include a sanitized upstream error code, never the error message or payload.

## C6. Upstream outcome mapping

**Terms.**
- **Commit** is the first upstream event that is not an error. For HTTP, response headers and leading events wait in a bounded hold until then (architecture §5 step 7). For WebSocket, see §9.
- **Failover** means re-running C7 without the accounts already tried, using the retained copy.
- **Attempt limits.** A request tries at most 4 distinct accounts. Each account has two independent same-account retry budgets: at most one token retry (row 5) and at most one network retry (row 11). So an account gets at most two extra attempts, and using one budget never consumes the other. These retries do not count as a new account.
- **Before acting on a decision** (a failover, or a WebSocket close), the health change is recorded first, so the next selection cannot pick the same account again.

| # | Upstream condition | Before commit | Client sees | Health change |
|---|---|---|---|---|
| 1 | 2xx | — | Relayed stream or body | Quota snapshot updated from headers (C9) |
| 2 | Usage limit: Claude 429 with unified `5h` or `7d` status `rejected` (sub-bucket only, e.g. `7d_oi`, means model scope); Codex 429 or error type `usage_limit_reached`, or `x-codex-limit-reached: true` | Failover | Next account's response; if none is eligible, row 14 | `cooling(quota, until=reset)` |
| 3 | Request-rate limit: status 429 without a quota signal (row 2); a Codex `error.code` of `rate_limit_exceeded` or `slow_down`; a Claude `error.type` of `rate_limit_error` without a window rejection. Live example (Codex via the previous proxy, 2026-10-14): body `{"detail":"Rate limit exceeded"}`. **Not** row 3: WebSocket frames whose `error.code` is `websocket_connection_limit_reached` or `previous_response_not_found`. They are relayed at once, whatever their `status`, and Codex reconnects by itself. | **Rate hold** on the same account (C6.3). **Never** failover. | The in-place retry's response, or after the hold the 429 as is (C3 adds `retry-after`) | None. Pacing only (C6.3) |
| 4 | 401 whose body signals revocation: Codex "invalidated oauth token" text or code `token_revoked`, or an Anthropic `authentication_error` saying the token was revoked | Failover | Next account's response; if none, row 15 | `needs_relogin` |
| 5 | Other 401, from the data plane **or a usage poll** (R8 I-07) | If the failed token is not current, retry once with the current token.<br>If it is current:<br>• **owned** account: a single-flight refresh, then one retry<br>• **linked** account: a single re-read of the CPA file, then one retry if the file holds a newer token<br>A terminal refresh error, a 401 with the new token, (linked) no newer token, **or a refresh that is transient, skipped or throttled** triggers failover (M4a review M4). In the last case the account gets `cooling(auth, 5 min)` so it is not reselected at once. | Next account's response; if none, row 14 or row 15 | Per C5: `needs_relogin` on a terminal refresh error. `cooling(auth)` on a 401 with a new token. Linked with no newer token: `cooling(link_stale)` |
| 6 | 403 | Failover. If a **second** account also returns 403 for the same request, stop and relay that response. | Next account's response, or the second 403 as is | Known model: `cooling(permission, model)`. Unknown model: none. **Account-level denial** (e.g. Claude "OAuth ... not allowed for this organization"): `cooling(permission, account, 6h)` plus a tray notice (R8 I-18). |
| 6a | 400 "model is not supported when using Codex with a ChatGPT account" (plan cannot serve the model; R8 I-17) | Failover, as for row 6 | Next account's response, or the 400 as is if none | `cooling(permission, model:<model>, 6h)`. The model name comes from the error text, because Codex bodies are not decoded. The C7 gate then skips the account for that model. |
| 7 | Billing rejection: 402, or a 400 whose `error.type` is `billing_error` (Claude; for example "Credit balance is too low"). This is checked **before** row 9. | Failover. The session's binding moves to the account that serves it; no other binding changes. | Next account's response; if none, the upstream billing response as is | `cooling(payment)` for that account only |
| 8 | 404 | No failover | Upstream response as is | **None** |
| 9 | 400 (other than rows 6a, 7 and 12b), 409, 413, 422 | No failover | Upstream response as is | None |
| 10 | 408, 5xx, 529, 507, `overloaded_error`, `server_is_overloaded` | No failover | Upstream response as is (clients run their own backoff) | None |
| 11 | Connect, TLS or DNS failure, header timeout, or the upstream closes before any byte | One retry on the same account after 250 ms, then give up | 502 in native shape (C6.2) | None |
| 12 | Error after commit (SSE `error` or `response.failed` event, connection drop) | **Never retried** | Relayed as is, then the stream ends | Signals inside the event apply (e.g. `usage_limit_reached` leads to `cooling(quota)`) |
| 12a | An error **event** before commit, e.g. a first SSE event that is `error` or `response.failed` | Treated like the matching status row (2–7, 10) | As that row | As that row |
| 12b | Pre-commit 400 with an encrypted-content error code: Codex `invalid_encrypted_content` or `thinking_signature_invalid`, Claude "Invalid `signature` in `thinking` block" | No failover, as row 9 | Upstream response as is | None. The ledger records the error code (C12), so a change in server behaviour shows up. |
| 13 | WebSocket upgrade | Dial upstream first. Classify the handshake response with rows 2–7 and 11, and fail over before answering the client. | 101 with an open upstream; otherwise row 14, 15, or 502 for a pure network failure | As in those rows |
| 13a | WebSocket: before commit, an `error` from rows 2 or 4–7 answers a `response.create` | First apply the matching row's health logic. For a 401 that is the row 5 token handling, including `cooling(auth)` (M4a review M3).
Close with 1012 **only if** a different eligible account exists for the reconnect. In that case:
1. Add the pinned account to the session key's **reconnect exclusion** (TTL 120 s, in memory), so that the reconnect cannot reselect it.
2. Do **not** relay the frame.
3. Close the client socket with 1012.
Otherwise relay the error frame as is, which ends the turn without burning Codex's retries. Codex reconnects with full input, and row 13 selects another account.<br>A **rate** error (row 3) follows C6.3 instead: never a reconnect exclusion, never a move to another account. | No error; the turn continues on another account | As in rows 2–7 |
| 13b | WebSocket: an error frame after commit, or any other error | — | Frame relayed as is | As in rows 2–12 |
| 13c | WebSocket: the pinned account becomes ineligible while the socket is idle | — | Client socket closed (1000); Codex reconnects silently on its next request | — |
| 14 | No eligible account, and **at least one** active account is in `cooling(quota or auth)` or has an auth probe running (M4a review S6). A poll of every candidate runs first (C9). | — | **HTTP:** 429. Replay the cached genuine limit response of the account whose reset comes soonest (status, body and limit headers, at most 64 KiB, with the original `Content-Encoding` kept), with `retry-after` set to the remaining seconds. Without a cache, use the synthesized shape (C6.2). **WebSocket upgrade:** native 429 with a `usage_limit_reached` body, which is terminal for Codex. | — |
| 15 | No usable account at all, and no active account is in a cooling that will expire. The accounts are `needs_relogin`, `paused`, `disabled`, or none are configured. | — | **400** in native shape, type `invalid_request_error`, message "quota-baton: no usable <provider> account — open the tray". It is terminal in both clients, so there are no retry storms and no sticky HTTP fallback. The user has to act anyway. | — |
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

### C6.3 Rate hold and per-account pacing (D14)

A request-rate limit (row 3) means the account is being asked too fast, not that it is out of quota. Moving sessions can lose prompt-cache affinity and require full-context retransmission, which can overload the destination too. Cross-account cache behaviour is not universally established. This cascade was observed live on 2026-10-14 under the previous proxy (60 re-bindings at 01:30 KST, 150–350k uncached tokens per moved session).

Rate limits are therefore absorbed on the same account. An individual rate limit does not directly move a session or cause failover. Persistent overload may cause a separately budgeted C7.1 discretionary move, subject to root policy. The design was independently reviewed (`.sandbox/reviews/D14-rate-hold-review.md`; F1–F10 refer to facts from the Codex 0.162.1 source).

**Codex behaviour this relies on (review F2–F7).**
- Codex retries a rate 429 only when the response carries retry advice: the HTTP `Retry-After` header, or a `headers` map inside a WebSocket error frame.
- Without advice the turn ends at the first occurrence. The message "exceeded retry limit, last status: 429" is the error's name, not proof that five retries happened.
- With advice, Codex retries up to 5 times and sleeps until the deadline each time.
- It drops the WebSocket on any stream error and reconnects with the full input.
- Its WebSocket connect timeout is 15 s.

**Pacing state.** Each account has `rate_backoff_until`, `rate_step` and `rate_release_interval`, held in memory only.
- **Not a health state.** It is not persisted, never changes eligibility and never moves a binding by itself. A persistent, client-visible limit on a concentrated account is handled by C7.1.
- **Owner.** The account runtime owns it, under one lock. The HTTP relay, the upgrade path and the WebSocket pump all update it there.
- **Backoff window.** A rate 429 starts or extends the window: `rate_backoff_until = max(rate_backoff_until, now + max(retry_after, 2^rate_step s))`, with ±20 % jitter on the proxy estimate, never reducing the server-advised minimum. The window is never shortened.
- **Step.** A 429 raises `rate_step` (cap 5) only if its request was **sent after** the current window was set. That makes one step per window, not one per 429 in flight. `rate_step` decays by one after each full backoff interval with no rate 429. The final step does not expire while the FIFO still contains waiting requests: a draining queue defers that final quiet interval. Once it drains and a full quiet interval elapses, the last step can reach zero. This prevents decay from releasing the entire queued tail as a new burst. A committed 2xx does **not** reset it.
- **Release interval.** It starts at 250 ms. It doubles for each backoff window that saw a rate 429, up to 5 s. It shrinks by 25 % after each 60 s with no rate 429, down to 250 ms.

**Admission.** Every request to an account passes its admission gate, whether or not it has a retained copy.
- **An account that is not pacing has no gate.** If `rate_step` = 0 and there is no future `rate_backoff_until`, every request is admitted at once, with no spacing and no limit on concurrency. Pacing exists only after a rate 429.
- **While the account is pacing** (`rate_step` > 0, or a future `rate_backoff_until`), the gate is open when `now ≥ rate_backoff_until` and the previous release was at least `rate_release_interval` ago. Once `rate_step` has decayed to 0, spacing stops.
- Waiting requests are released in arrival order, one per interval. One release task per account enforces the spacing.
- While a request waits, its body is retained or back-pressured.
- A request whose wait reaches `rate_hold_secs` (default 60) is sent anyway, once. Its response is relayed as is, and C3 adds `retry-after` to a 429. The proxy never synthesizes a rate 429; answering with a synthesized 429 at once was the previous proxy's fail-fast failure. This send-once rule applies to initial admission, HTTP retries, and WebSocket resends or same-account redials. A forced send receives no further rate retry. Local hold expiries never count toward the C7.1 overload trigger. Eligibility and strict policy are still rechecked before any send (D14, D20 §4).

**HTTP: retry in place.** This applies to a pre-commit rate 429 when the request has a retained copy.
- Pass the admission gate again, then retry the same account from the retained copy.
- Retry at most `rate_hold_max_retries` times (default 3), keeping the total hold within `rate_hold_secs`.
- These retries count neither toward the 4-account limit nor toward the token and network budgets. The ledger's `attempts` does include them.
- After the last retry, relay the last 429 as is. Do the same at once when the request has no retained copy (C6 row 17).
- During a storm, retained copies count against the in-flight budget (architecture §13). When it runs out, later requests have no retained copy, so their 429 is relayed.

**WebSocket upgrade.** No hold on an upgrade lasts longer than `ws_upgrade_hold_secs` (default 10), because Codex's connect timeout is 15 s. This covers both the rate hold and the C7 reset hold. If the pacing ends later than that, answer the upgrade at once as if the hold had expired. The overall envelope includes refresh and dial (D20 §4); the send-once expiry rule applies within it.

**Open WebSocket, pre-commit rate error.** The proxy keeps the last `response.create` of each socket until commit. Codex sends nothing else on the socket before commit. This copy counts against the in-flight budget, like an HTTP retained copy. Take the first case that applies:
1. **The upstream socket is still open.** Wait for the pacing delay, then resend the kept message on the same upstream socket. Do not relay the error frame. The pacing delays together stay within `rate_hold_secs`, with at most `rate_hold_max_retries` resends.
2. **The upstream socket closed, and the message has no `previous_response_id`.** Dial a new upstream socket on the same account, subject to admission, and resend the message there.
3. **The error frame carries retry advice.** Relay it as is, immediately. Codex retries after the advised delay.
4. **Otherwise.** Do not relay the frame. Close the client socket with 1012, with **no** reconnect exclusion. Codex treats the close as retryable and reconnects with the full input. The binding is unchanged, so the reconnect goes back to the same account, and admission paces its upgrade (at most `ws_upgrade_hold_secs`). Relaying the frame would end the turn at once (F3).

If the upstream socket closes while a resend is pending, close the client socket with the same code. Pending resend and same-account redial admission waits follow the send-once rule above when their hold expires. The Codex reconnect behaviour for close 1012 is documented in architecture §9 and D14 review F5. A local expiry is not an upstream client-visible rate observation.

Phase 0 item **V8** observes the genuine `wss://chatgpt.com` rate-limit shape. It records which of these occur:
- a handshake 429
- an error frame, with or without `headers`
- `response.failed` with `rate_limit_exceeded`
- a close

It also records whether the socket stays usable afterwards.

**Placement.** New placements (C7 steps 3–5) skip an account that is pacing whenever another candidate at the same step is not pacing. This affects placement only. Bound sessions never move because of pacing. Within C7.1's partition (accounts with room first), the skip applies to whichever set is used.

**Ceiling.** Absorption is bounded. Under a sustained limit, a Codex HTTP turn survives about (5 + 1) × (`rate_hold_secs` + the advised delay), roughly 5–6 min. After that, Codex's own budget ends the turn. A limit that lasts that long means too many sessions share the account. If they are concentrated on it while another account has room, the overload rebalance (C7.1) moves a bounded number of them at their next request. Otherwise the total is too high for the accounts available, and reducing it is the orchestrator's job (D14).

**Observability.**
- `AccountView` (C11) gains `pacing {backoff_until, step, release_interval_ms, waiting}`.
- The tray shows a pacing account as "속도 조절 중".
- The ledger records, per request:
  - `rate_wait_ms` and `rate_retries`
  - the outcome `rate_hold_ok` or `rate_hold_relayed_429`
- The ledger also counts relayed 429s per session.

### C6.4 Mixed pools, stale affinity and misleading errors (the previous proxy incident, 2026-10-17)

**The incident.** The previous proxy served Claude from one model pool that mixed 4 OAuth subscription credentials with 4 static API-key credentials.
- The OAuth credentials answered 429 for their weekly or rate limits, and the previous proxy cooled them.
- They stayed registered in the model pool, so CPA's 1 h session affinity kept binding sessions to exhausted credentials.
- Claude Code then showed the misleading "Credit balance too low". The proxy logs held only 429 quota evidence; no credit ledger had failed.

The workaround was applied on the previous proxy side and verified there. This section records what quota-baton must guarantee, so that the same failure cannot happen here. Most of it follows from C5–C7; the rules below make it explicit and testable.

1. **One credential kind per pool.** Every account has exactly one kind: an OAuth subscription account (owned or CPA-linked). Static API-key credentials are not a supported kind today. If they are added later (O14), they form a separate pool per provider. A request may mix pools only when a config option explicitly enables it, and the selection order between pools is the owner's decision recorded in O14.
2. **Affinity is revalidated on every request.** A binding never forces an account. C7 step 1 uses the bound account only if it is eligible (C5) at that moment. A binding to a cooling, unavailable, paused or disabled account is re-placed at the next request boundary. There is no time-based affinity that outlives eligibility.
3. **State before selection.** On a quota, rate, auth or billing signal, the health change (reason, scope and `until`, taken from the reset headers when present) is recorded atomically **before** any failover or new selection (C6 "Before acting on a decision"). The next selection, including a concurrent one, cannot pick the account again.
4. **Unavailable accounts leave the catalog path.** `/v1/models` (O9) is answered by an eligible account, chosen like any other request. A cooling or unavailable account never answers it.
5. **Native errors, never a rewritten credit error.**
   - The four classes stay distinct: 429 quota or rate (C6 rows 2 and 3), 401 auth or re-login (rows 4 and 5), billing (row 7: 402, or a 400 with `error.type` `billing_error`), and 5xx or transport (rows 10 and 11).
   - A billing rejection is never treated as a rate limit, and a rate limit is never treated as billing.
   - While another account is eligible, the client sees that account's response.
   - When none is eligible, the client sees the genuine upstream response of the right class (row 14 replays a cached genuine limit response, row 15 is the "no usable account" 400).
   - quota-baton never synthesizes or relabels a credit or billing message. A 402 reaches the client only when an upstream sent it.
6. **Billing is per account.** Credits are never aggregated across accounts. Claude's `overage-status` header (C9) is recorded as information only and is never read as a credit balance. A Codex `credits` value belongs to its own account and drives only `allow_credits` (C5).
7. **Provider isolation.** A usage poll, a monitor or an event of one provider never changes the health or user state of another provider's accounts. The C9 pollers are per provider, and every health write carries the account id and provider.
8. **Observability.** Per provider, `/state` (C11) and `quota-baton status` show the counts of eligible, cooling (by reason) and unavailable accounts. The ledger and these views show, per account, the provider, the credential source kind, the health reason and `until`, with no secrets.

**Tests** (testkit fakes; each asserts the account that served, its source kind, and the recorded `until`):
- exhausted bound account plus a healthy one: the next request of the bound session is served by the healthy account, and the binding moves
- stale affinity after a restart: a binding imported for an account that is now cooling is not used
- 429 with reset headers: `cooling(quota)` with `until` equal to the header reset, recorded before the failover attempt is sent
- 401 revocation: `needs_relogin` (owned) or `link_stale` (linked), then failover
- 402, and a 400 `billing_error` ("Credit balance is too low"): `cooling(payment)` for that account only, failover, the session's binding moves, other sessions' bindings stay, and the native response is relayed only when no account is eligible
- after a billing cooling, a small successful request elsewhere or a usage poll does not clear it; only `until` does
- `/v1/models` while one account cools: answered by the eligible account
- all accounts exhausted: the genuine 429 is replayed with `retry-after`, and the body never contains a credit or billing message unless one came from upstream
- a Codex usage poll or 401 never changes a Claude account, and the reverse
- the `/state` counts match the scenario

## C7. Selection policy

These rules originate in D11, with session spreading in D15 and explicit routing and anti-herding in D20 (decisions.md).

**Input:** the family, the request's session key with its optional parent key (C8), the model if known, and the accounts already tried.

**Placement is serialized** per family, so a burst of new sessions sees each earlier decision.

**Projected usage.** For each window it is the last observed `used_pct`, plus an in-flight allowance for each request in flight (`inflight_allowance_pct`), plus outstanding placement charges (`placement_allowance_pct`, default 2 percentage points per new or moved group). Each window retains a charge until its own observation is strictly newer than the placement. Equal or older observations do not clear it. Rules from M4b and D20:
- **Gating windows.** Only the account windows count for the gate and for ordering: `5h` and `7d` (Codex primary/secondary as named by C9). Sub-buckets such as `7d_oi` and other names never gate the account and never set the weekly ordering. They act only through model-scoped cooling (C5) (S1).
- **Non-standard Codex windows (design owner, 2026-10-20).** A Codex primary or secondary window with another length (`<n>m`, C9; e.g. the Free plan's single `43200m`) is an account window. It gates and orders in a slot chosen by its length:
  - `n` ≤ 1440 takes the 5-hour slot: `gate_5h_pct` and the 5-hour ordering key.
  - `n` > 1440 takes the weekly slot: `gate_weekly_pct`, the near-reset rule and the weekly ordering key.
  - When a slot holds two windows, both gate, and the ordering key uses the window with the higher projected usage.
  - Unknown, stale and past-reset rules apply unchanged.
- **Past reset.** A window whose `reset_at` has passed counts as 0 % used and unstarted until a new observation arrives (F2).
- **Unknown or stale.** A window is stale when its observation is older than 15 min **and** the account has carried traffic newer than that observation.
  - Unknown or stale usage **sorts last** in ordering but **does not fail the gate**: it is not known to be over (F1).
  - Traffic records older than the latest observation do not count.
  - Restored bindings do not count as traffic.
- **Projection.** The in-flight projection is used only for placement. It never creates a cooling, for example the credits hold (S5). Coolings come only from observed values (C5).

**Evidence and anti-herding (D20).** A placement is a new C7 steps 3–6 group, a C7.1/cold-point move, or a group moved off an ineligible account under C5. Forced C5 moves take the placement charge and apply the cap in target choice (D20 §4). Bound reuse, parent-binding reuse in step 2, restored bindings and same-account retries are not placements. Observations use request-send time for headers/WS events, or poll-start time for polls, in the account's monotonic sequence. Receipt time is not evidence age. Only a strictly later as-of observation clears a charge; the same instant does not.

Evidence is weak when any gating window is unobserved, its observation precedes the latest placement, or its reset passed without a new observation. `placed_since_obs` counts placements newer than the oldest gating-window observation, or all placements while any window is unobserved. An account is capped only when weak and this count is at least `weak_evidence_new_sessions` (default 1).

Apply the cap in both branches of step 4, in step 5 and in C7.1, after the room partition and pacing skip. Uncapped accounts come first in C7 order. When every candidate in that set is capped, use the fewest placements since observation, then C7 order. New-session and forced C5 placement never refuse merely because all accounts are capped; a cap cannot prevent a forced move when an eligible account exists. C7.1 instead retains the group and its unexpired mark when every target with room is capped. Placement charges never affect the cold-point gate check and never cool or move a bound eligible session.

After any placement, wait `placement_poll_after_secs` (120) and check that every gating window has a newer observation. If not, issue one read-only GET poll using its own per-account `placement_poll_min_interval_secs` (300) budget, independent of the normal minimum, while respecting poll backoff and C9 exclusions. If this independent budget refuses a placement poll, the regular poll schedule still runs. The ledger records `placement{evidence: fresh|weak, capped, placed_since_obs, charge}` and `poll{reason: placement_evidence}`.

**The gate** (new-session placement):
- every 5-hour window is projected below `gate_5h_pct` (default 90)
- every weekly window is projected below `gate_weekly_pct` (default 90), or below `gate_weekly_near_reset_pct` (default 98) when its weekly reset is within `near_reset_secs` (default 18 000)
- the account is not drained
- the account is not known to be unable to serve the requested model (learned from earlier 400/403 errors for that model)

**Steps:**
1. If the session key is bound to account A, and A is eligible (C5) and not yet tried, use A. Two exceptions:
   - **Reset hold:** A is in `cooling(quota)` and its limiting window resets within `reset_hold_secs` (default 60). Hold the request until the reset instead of moving it; the intent is to preserve cache affinity; warmth is not guaranteed.
     - **During a request (F3).** The hold also applies when A answers *this* request with a pre-commit usage limit whose reset is within `reset_hold_secs`. The request then waits until the reset and retries A once from the retained copy, instead of failing over. This applies to HTTP, and to a WebSocket **upgrade**. An error on an already-open socket keeps the row 13a behaviour.
     - **Bound (S2).** The hold never exceeds `reset_hold_secs`, which stays well below both clients' timeouts (Claude Code 600 s; Codex stream idle 300 s). On a WebSocket **upgrade** it never exceeds `ws_upgrade_hold_secs` (10 s), because Codex's connect timeout is 15 s (C6.3). If the reset has not taken effect when the hold ends, normal failover applies at once. No separate client-timeout estimate is used.
     - **Scope (S3).** The hold is per request. A request whose hold ended does not stop the next request of that session from holding.
   - **Cold point:** see below.
2. Else, if the parent key is bound to account P, P is eligible and P has not been tried, bind the session to P and use it.
3. Else, let `E` be the eligible accounts that have not been tried, and let `G` be the accounts in `E` that pass the gate.
4. Partition `G` into `G_room` (the accounts with room, C7.1: `active < spread_threshold`) and the rest.
   - If `G_room` is non-empty, apply the C6.3 pacing skip and then the D20 uncapped/round-robin preference within it; order the remaining candidates by:
     1. the soonest weekly `reset_at`. Resets within 3 h of each other count as tied.
     2. among tied accounts, the soonest 5-hour `reset_at`, with unstarted windows last
     3. the lower projected usage on the binding window
     4. account id

     Take the first account.
   - If `G_room` is empty but `G` is not, order `G` by `active / spread_threshold` ascending, then by the pacing skip, then the D20 uncapped/round-robin preference, then by 1–4 above, and take the first.
   - A full account is never a refusal.
5. If `G` is empty but `E` is not, apply the same room partition to `E`, then the C6.3 pacing skip and D20 uncapped/round-robin preference. Take the most headroom within that set. If no account has room, first take the lowest `active / spread_threshold`, then apply pacing and D20 preferences, then headroom. This is the 2B fallback.
6. Bind the session, and the parent if it was unbound, to the chosen account.
7. If `E` is empty, apply C6 row 14 or row 15.

**"Unusable" stays narrow.** It means:
- a window at its limit, as **observed** (C5), except the Codex subscription-only limit when permitted credits are available. On credit-capable accounts with `allow_credits=false`, an *observed* 99 % counts as the limit. A *projected* 99 % only affects the gate and ordering for new placements; the account stays eligible, including for the 2B fallback. A projection never makes an account unusable and never produces a synthesized 429 or 400.
- auth still failing after one refresh (C5)
- the user pausing or disabling the account

Overload, 5xx, short rate limits and network errors retry in place (C6).

Further rules:
- **Usage gates never move a session.** Placement charges are also excluded from the cold-point gate check. Gates and usage MUST NOT move a bound session. A bound session moves only in three cases, and then only at a request boundary:
  - its account fails the eligibility test in C5
  - it is cold and the cold-point rule applies
  - it is marked by the overload rebalance (C7.1)
- **`last_used`.** Every HTTP request and every relayed WebSocket `response.create` updates the binding's `last_used`, set to the end of the response. Idle time is measured over the whole session identity group, sub-agents included.
- **Pinned sockets keep their binding.** A binding referenced by an open pinned WebSocket is never expired or evicted.
- **Cold point.** A binding is cold when it has been idle (`now - last_used`) for at least `rebalance_idle_secs`, default 3600. That is never below the longest prompt-cache TTL in use for the provider. This is an operational idle threshold for considering a move, not proof that every provider cache has expired or that moving preserves reasoning.
  - A cold session is re-placed (steps 3–6) **only if its current account fails the gate or has `active ≥ spread_threshold`** (C7.1). In the first case a forced move would probably come soon anyway; in the second the account is already full. Either way, moving at a cold boundary is cheaper than moving mid-session.
  - A cold-point target must be eligible, pass the gate, have known non-stale quota and have room, including pending mark reservations. If no such target exists, the session stays (D20 §4).
  - Otherwise the binding stays, for up to `session_binding_max_secs` (default 7 days).
  - Why not re-place every cold session? In the tested V2 cases, moves completed with evidence consistent with earlier hidden reasoning being dropped (D10). Compatibility is not guaranteed outside that evidence. A move that is not needed is therefore not free.
  - `[websocket] idle_close_secs` MUST be smaller than `rebalance_idle_secs`.
- **Moves.**
  - They happen only at request boundaries, atomically for the whole session group.
  - A running stream is never moved.
  - Every move is logged to the ledger with: reason, account from and to, request bytes, and the cache-hit result of the next turn when the usage fields show it.
- **Persistence.** Bindings are persisted write-behind, at most 60 s late, to `state/bindings.json`, and flushed on drain and on exit. LRU capacity is 20 000. They are exported and imported on takeover (C11).
- **Unbound requests** (no key, C8) stay on the family's **last-used account** until it becomes ineligible. Selection then runs once, and the result becomes the new last-used account.
- **WebSocket selection** runs once per connection, on the upgrade (C6 row 13). The connection keeps that account until it closes.
- **Drained account at a cold point (S7).** A drained account fails the gate, so a cold session on it is re-placed. The idle threshold is intended to reduce the cache cost of that move; it is not a zero-cost or compatibility guarantee. A warm session stays until it goes cold.
- **Drain vs pause.**
  - Drain stops new placements on the account, and existing sessions stay.
  - Pause (C5 `user_state`) moves existing sessions away at their next request.
  - Neither is required for normal use.

### C7.1 Session spreading and overload rebalance (D15)

D11 places every new session on the account with the soonest weekly reset until its gate fails, so sessions concentrate on one account while others sit idle. D14 made rate limits never move a session. This subsection adds the user's rule (D15):
- Spread new sessions before an account is full.
- When sessions are already concentrated on an account that keeps hitting its rate limit while another account has room, move a bounded number of them, accepting possible cache loss and full-input retransmission cost.
- When sessions are already well spread, a rate limit moves nothing.

The design was independently reviewed (`.sandbox/reviews/D15-review.md`).

**Terms.**
- **Group.** The unit C7 binds (C8): the Codex root `session-id`, or a Claude root session together with its agents. Everything in this subsection counts groups, never keys.
- **Active.** A group bound to account X is active when one of these holds:
  - it has a request in flight on X
  - it has an open pinned WebSocket on X with a response in flight
  - its `last_used` is within `active_window_secs` (default 300)

  `active(X)` is the number of active groups bound to X. It measures current load, not cache warmth.
- **Spread threshold.** `spread_threshold` (default 5, overridable per account with `[accounts.<id>]`). It is a spreading heuristic, not a measured capacity (D15), and it never refuses a request.
- **Room.** Account B has room when `active(B) < spread_threshold(B)`.
- **Request boundary.** A new HTTP request, or a WebSocket upgrade. These are not boundaries: C6.3 in-place retries, C7 reset-hold retries, and same-account token or network retries (C6).

**Placement** of new sessions is C7 steps 3–5:
- Accounts with room come first.
- If none has room, the account with the lowest `active / spread_threshold` is used.
- The C6.3 pacing skip applies within whichever set is used.

**Overload trigger.** The account runtime evaluates it each time a rate 429 reaches a client on account A. Under C6.3 that is a relayed HTTP 429, a relayed WebSocket rate error frame, or a 1012 close for a rate error. A is **overloaded** when all of these hold:
1. Within the last `overload_window_secs` (default 600), such events hit at least **3 distinct groups** bound to A.
2. `active(A) > spread_threshold(A)`.
3. An eligible account B of the family passes the gate, has room, and `active(A) − active(B) ≥ 2`.

Some outcomes never count toward the trigger, because they are not account concentration:
- upstream 429s that the hold absorbed
- row 2 (quota)
- row 10 (5xx, 529, overloaded), including model-capacity errors such as "Selected model is at capacity"

**Marking.** When A is overloaded, victims are chosen among the groups bound to A.
- **Order:** smallest last request bytes first, since that is the cost of the uncached re-send; ties go to the longest idle time.
- **Skipped:** any group with a request or a WebSocket response in flight, or unknown full-input cost. Restored and continuation-only input has unknown cost until full input is known; `victim_skipped_unknown_cost` measures this exemption.
- **How many:** at most `min(active(A) − spread_threshold(A), 2 − marks already made for A in this window)`. Marking stops as soon as `active(A) − active(B) < 2` would hold after the move.
- **The mark** is `{from: A, target: reserved B, expires: now + overload_window_secs}`. Its reservation counts against destination room until execution, replacement or expiry. It is held in memory with the bindings and is not exported on takeover.
- **Idle pinned socket:** if the marked group's pinned socket is idle, close the client socket with 1000, as in C6 row 13c. The reconnect's upgrade is then the group's next request boundary.
- **Ledger:** log `overload_detected {account, active, threshold, groups_hit, marked}`. When conditions 1 and 2 hold but no account has room, log it once per window too, so that "well spread, no move" is visible.

**Moving.** At a marked group's next request boundary:
- Move if the mark is unexpired, `active(A) > spread_threshold(A)` still holds, and an eligible account with room passes the gate. Then skip step 1, run steps 3–6 restricted to accounts with room, and bind the whole group there (C7 "Moves").
- The marked target is a capacity reservation, not a constraint. At execution, reselect among currently eligible accounts with room, using C7 order and destination-specific guards; a disabled or cooling reserved target cannot prevent moving to another eligible one.
- If the only targets with room are capped, or any remaining discretionary guard vetoes the move, stay on A and keep the unexpired mark while `active(A) > spread_threshold(A)` still holds (D20 §4). Otherwise clear the mark and stay on A. A guard veto never converts the move to a forced C5 move.
- Log the move when it executes: `binding_move` with reason `overload`, the request bytes, and the next turn's cache result.

**What never moves.**
- The request that received the 429. Row 3 never fails over.
- Any group that is not marked. A rate limit alone moves nothing.
- Anything when no account has room. The sessions are then well spread, and a 429 only paces.
- A running stream, an in-flight request, or a WebSocket with a response in flight.

**Discretionary movement guards (D20).** Rebalance and cold-point moves require dwell 1,800 seconds, revisit 3,600 seconds, at most 3 discretionary group moves and 4 discretionary provider moves per 600 seconds, and known full input at most 8 MiB. Group and provider pool budgets count only discretionary moves. D20 §4 removes the destination-quiet guard. A target needs eligibility, the C7 gate, room, and non-stale quota evidence; no recent successful request is required. Weak targets accept one moved group until observed; all-capped move destinations retain the mark rather than invoking the new-session fallback. A move cannot take the target above its threshold or leave it more loaded than the source. Forced C5 moves are recorded but never blocked by or counted against these guards, including the retained-history cap. Account ID determines the provider movement pool.

**Speed.** The cure is deliberately slow: at most 2 moves per account per window. Placement is the main lever.

**Interaction.**
- **Pacing (C6.3):** unchanged.
- **Drain:** a drained account fails the gate, so it is never a target. Its groups may be victims.
- **Pause:** unchanged; it causes forced moves.
- **Takeover:** marks, the 429 history and the window budget live in memory only. The new instance rebuilds `active` from the imported `last_used` and starts with an empty history.

**Observability.**
- `AccountView` (C11) gains `active_sessions` and `spread_threshold`. The tray shows the count against the threshold.
- The ledger records `overload_detected` and `binding_move{reason: overload}`. These are the inputs for tuning `spread_threshold`.

### C7.2 Explicit root-session routing intent

`routing.mode` defaults to `rebalance` (existing C7/D15 behaviour). `recover` keeps an eligible bound account and permits only recovery from ineligibility. `strict` does not automatically change its local account ID and requires a classified root identity. It may refresh/reconnect that account, but pause, quota, auth, payment and credit restrictions still prevent dispatch. An unavailable locked account returns a provider-shaped HTTP 409 `routing_policy_blocked`; it must not trigger selection of a different account. Discretionary movement guards retain a healthy current account; forced C5 recovery is never refused by those budgets. Strict authority alone fails closed on policy persistence failures.

The authenticated routing admin API registers opaque projects and applies root-session overrides. Effective settings are global defaults → explicitly associated project baseline → explicit root override. Children inherit the root. A project edit affects new associations; existing baselines stay until explicitly edited. Display labels never select policy. Session handles are full SHA-256 hashes of canonical C8 root keys, separate from the short dashboard display IDs.

Only explicit project/session intent and strict anchors require durable policy writes. Default routing history remains in memory. Policy files are bounded to 16 MiB, 20,000 roots and 1,024 projects. A policy write, recovery or startup failure blocks dispatch only for affected strict sessions, including a strict first placement. Non-strict sessions continue on eligible accounts, quarantine damaged evidence and raise `core_warning`. Normal binding LRU eviction cannot erase a strict anchor. Takeover retires the old writer before handover.

`reset_wait_secs` is a root/project setting in 0–120 seconds; global defaults retain `routing.reset_hold_secs` (60). It is separate from credit permission. Global `routing.mode = strict` is rejected: strict requires a classified root policy. Churn safeguards in C7.1 govern discretionary movement only; inability to satisfy them retains a healthy bound account. Unknown full-input cost suppresses discretionary movement. Idle socket pins protect bindings without counting as active demand.

Associating a project preserves an explicit root override unless `inherit: true` / CLI `--inherit` is supplied. A history-only root follows current global defaults. Startup captures strict anchors before pruning legacy bindings. Serving rechecks current intent and account eligibility after pacing/reset waits, including WS create/reconnect. Readers use immutable intent snapshots; disk writers never hold the binding/read lock across I/O. Forecast remains observation-only. The pre-commit deadline and WebSocket upgrade envelope are active, as ratified by D20 §4.

`routing.request_timeout_secs` (default 300 s) bounds the whole HTTP pre-commit path, including preparation, selection and holds, refresh, dial and retries. If it expires before response headers, return a provider-shaped HTTP 504. It never cuts off an already committed stream. The WebSocket upgrade has its own overall `ws_upgrade_hold_secs` envelope (default 10 s), including refresh and dial, below Codex's 15 s connect timeout (C6.3).

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

**Codex bodies:** D22 launch-scoped requests permit a bounded side-copy peek of the top-level `model` in HTTP responses/compact requests and WebSocket `response.create` frames, including bounded decompression for HTTP. This validates the model against the bound account's catalog; it never supplies a session identity or rewrites forwarded bytes. Search requests use the launch's current model because their schema has no model selector. Unscoped Codex requests retain the original no-decode path. The debug-only `shape-capture` and `phase0` builds have the additional inspection allowed by architecture §13.

## C9. Usage signals

Every source normalizes into the account snapshot `windows[]` (`name`, `used_pct`, `reset_at`, `status`), plus `plan`, `credits`, `source` and `observed_at`. The value formats below come from the official clients and must be confirmed in **[V1]**, **[V3]** and **[V5]**.

| Source | Fields |
|---|---|
| Claude response headers | `anthropic-ratelimit-unified-{5h,7d,7d_oi}-{utilization,reset,status}` (utilization is 0..1; reset is a unix time), `-status`, `-reset`, `-representative-claim`, `-overage-status`, `retry-after` |
| Codex response headers | `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-after-seconds,reset-at}`; the window is named from its minutes (300 is `5h`, 10080 is `7d`, any other value is `<n>m`); `x-codex-plan-type`, `x-codex-limit-reached`, `x-codex-active-limit`, `x-codex-credits-{has-credits,unlimited,balance}` |
| Codex WebSocket event `codex.rate_limits` | `rate_limits.{primary,secondary}.{used_percent,window_minutes,reset_after_seconds\|reset_at}`, `limit_reached`, `credits`, `plan_type` |
| Codex poll | `GET https://chatgpt.com/backend-api/wham/usage` with `Authorization`, `ChatGPT-Account-Id` (this casing, as Codex's backend client sends it, `backend-client/src/client.rs:277-278`), `Accept: application/json` and `User-Agent: quota-baton/<ver>`. Response: `rate_limit.{limit_reached, primary_window{used_percent,reset_at}, secondary_window{...}}`, `plan_type`, `credits` **[V5]** |
| Claude poll | `GET https://api.anthropic.com/api/oauth/usage` with `Authorization`, `anthropic-beta: oauth-2025-04-20`, `Accept: application/json` and an honest `User-Agent`. **Verified live (V5, 2026-10-13):** 200, shape `five_hour{utilization, resets_at, …}`, `seven_day{…}`, and nullable sub-buckets (`seven_day_opus`, `seven_day_sonnet`, `seven_day_oauth_apps`, plus others with opaque names). `utilization` is a **percent 0..100** here, so do not guess the scale; only the response *headers* use fractions 0..1. `seven_day_opus` and `seven_day_sonnet` are model-scoped sub-buckets. Other names are ignored. |

Polling rules (architecture §8):
- A `cooling(quota)` account is polled at least hourly.
- A poll answered with 401 is handled exactly like a data-plane 401 (C6 rows 4 and 5, including the linked-account rule in C5).
- **Linked accounts at startup and before each poll:** if the CPA file shows a newer `last_refresh` than the stored token, reload it first. The quota watcher ignored 2,704 such 401s and stayed blind for days (R8 I-07).
- Every candidate is polled immediately before C6 row 14 or row 15 is answered.
- The minimum interval per account is 5 minutes. A D20 placement-evidence poll has its own budget of one per account per 5 minutes and may run inside the regular interval; it follows the same backoff and the exclusions below.
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

The callback servers MUST bind only the loopback addresses `127.0.0.1` and `::1`. The OAuth `code` and the callback query string MUST be redacted from logs. Each login session has a random `state`, which MUST match on callback, and expires after 10 minutes. The callback listener MUST close on completion, cancellation or expiry; the previous proxy left port 1455 open after a re-login (R8 I-50).

## C11. Admin API

Base: `http://127.0.0.1:<admin_port>/admin/v1`. Every route requires `Authorization: Bearer <admin.token>` and passes the C2 Host and Origin checks. Requests and responses are JSON.

| Method and path | Purpose |
|---|---|
| `GET /health` | `{ok, version, pid, started_at}` |
| `GET /state` | `{accounts:[AccountView], inflight:{requests, bytes}, memory:{private_bytes}, bindings:{count}, data_port, version}` |
| `GET /events` | SSE stream of `account_changed`, `quota_updated`, `core_warning`, `login_progress`, `sessions_changed` and `activity`. The tray and dashboard share one subscription. Session changes are debounced to at most once per second; a lag marker requires resynchronization. |
| `GET /sessions?range=active\|hour\|today\|all` | Bounded account-grouped session metadata. IDs are the 12-hex ledger root hash; counts distinguish missing evidence from zero. Folder basenames and client/model metadata are memory-only. |
| `GET /sessions/{id}` | Session detail, bounded request summaries and placement timeline. Only this route may include `original_short_id`, the first 8 characters of the root ID, retained in memory only. Full raw IDs, content and paths are never returned. |
| `GET /activity?days=1..7&limit=1..2000&since_seq=...` | Bounded 7-day activity, with stable sequence numbers for updated aggregates. No labels, raw ID prefixes, paths or upstream error bodies. |
| `GET /usage-history?hours=1..48` | Observed account-window history and reset/credit spans. Unknown observations remain gaps; remaining usage is a dashboard projection. |
| `GET /routing` | Global defaults, registered projects, durable root policies and observed full routing handles; never raw root IDs, tokens or project paths. |
| `POST /routing` | Typed `kind: project` registration/update (`id?`, `name`, `settings`) or `kind: session` update (`id`, `settings?`, `project?`). Mutations require the running owner and are refused during drain. Settings contain `mode: strict\|recover\|rebalance` and `reset_wait_secs: 0..120`. |
| `POST /accounts/login` `{provider}` | Returns `{login_id, auth_url}`. The core runs the callback listener. |
| `POST /accounts/import` `{source:"cpa", mode:"linked"\|"adopted", activate:[...]}` | Imports from CPA's auth directory (D5). Returns per-identity metadata, never tokens. `mode:"adopted"` is refused: adoption is per existing account via `/accounts/{id}/adopt`. |
| `POST /accounts/{id}/adopt` `{from:"cpa_file"\|"store"\|{"path":"..."}, expected_generation}` | Takes refresh ownership of a linked account (D17). Refused with 409 while CPA can still refresh it (process running and file loadable), on a generation mismatch, or when the file's identity does not match. Returns `{id, generation, from, token_expires_at}`; never tokens. |
| `POST /accounts/takeover` `{provider:"codex"\|"claude", path, phase:"inspect"\|"commit", inspect_digest?, replace?, keep_source?}` | Takes refresh ownership of an account from another app's home (D18). `inspect` returns `{email, plan, provider_account_id, source_path, source_last_refresh, source_mtime, owner_alive, existing_id?, inspect_digest}` and writes nothing. `commit` needs the `inspect_digest` of unchanged file bytes. It is refused with 409 when the owner looks alive, the file changed within 120 s or since inspect, or the identity exists without `replace`. It is refused with 422 for API-key auth. On success it stores `source.kind="takeover"`, renames the source file to `<name>.taken-over-by-quota-baton-<YYYYMMDD>` unless `keep_source`, runs one manual refresh, and returns `{id, renamed_to?, refresh:{ok, token_expires_at}}`. It never returns tokens. |
| `POST /accounts/{id}/refresh` `{force?:bool}` | Manual single-flight refresh, with the same gate and write-ahead as refresh-ahead. Refused for linked accounts ("CPA가 갱신하는 계정입니다"), for `drain` or `needs_relogin`, and while `refresh_uncertain=true` unless `force` is given or the current access token can no longer serve. Returns `{outcome, token_expires_at, last_refresh_at}`. |
| `GET /accounts/login/{login_id}` | `{status: pending\|completed\|failed\|expired, account_id?, error?}` |
| `POST /accounts/login/{login_id}/cancel` | Cancels the login |
| `POST /accounts/{id}/{pause\|resume\|disable\|enable}` | Changes `user_state` |
| `PATCH /accounts/{id}` `{label?, drain?, allow_credits?}` | Edits account settings |
| `DELETE /accounts/{id}` | Removes the account file. This does not revoke the account upstream. |
| `POST /accounts/{id}/refresh-usage` | Polls usage now, subject to the minimum interval |
| `POST /lifecycle/drain` | Takeover handover, in this order:<br>1. stop refresh-ahead and 401-triggered refresh (a 401 during drain is relayed without refresh or health change)<br>2. stop writing account and state files<br>3. flush bindings<br>4. close the data listener<br>5. **release the credential lease**<br>6. return `{bindings:[...], lease_released:true}`<br>In-flight streams continue until done or `drain_timeout_secs`. |
| `POST /lifecycle/shutdown` `{drain_timeout_secs}` | Graceful exit; flushes bindings |
| `POST /debug/pin` `{session_key?, account_id}` / `DELETE /debug/pin` | **Phase 0 only.** Forces selection for that session key, or for all sessions if the key is omitted, to the given account, overriding C7. Logged at warn level and shown in the tray. Cleared on restart. |

`AccountView` fields:
- `id`, `provider`, `label`, `email`, `plan`
- `user_state`, `health` (including `reason`, `until` and `scope`)
- `windows[]`, `credits` (balance retains its decimal value), `credits_in_use` (Codex: paid credits allowed and current successful serving evidence indicates credits under C5; capability alone is insufficient)
- `pacing {backoff_until, step, release_interval_ms, waiting}` (C6.3)
- `last_used_at`, `bound_sessions`
- `active_sessions`, `spread_threshold` (C7.1)
- `pool_eligible` identifies membership in the C6.4 `pools.eligible` count; it is not a new-session placement promise. `quota_cooling` carries the persisted limiting-window evidence (C5), and `observed_at` is the usage snapshot's observation time, not the last admin read time.
- `/state` also carries `pools: {<provider>: {eligible, cooling: {<reason>: n}, unavailable}}` (C6.4)
- `token_expires_at`
- `quota_credit_releasable` (boolean, missing means false; C5 observation-only hold provenance)

Tokens are never included.

## C12. Persistence formats

### `config.toml`

All keys are optional. The defaults are shown. Numeric values outside their documented bounds emit a `core_warning` and are clamped to the existing bounds; they do not refuse startup. Unknown keys are ignored with a warning in every table, including `[routing]` and `[routing.churn]` (D20 §4).

Routing bounds: `request_timeout_secs` and `rate_hold_secs` are 1–600; `ws_upgrade_hold_secs` is 1–14; `reset_hold_secs` is 0–120; `weak_evidence_new_sessions` is 1–1000. Percentages are 0–100; non-finite values clamp to an endpoint, with NaN falling back to that field's default. The near-reset weekly percentage cannot be lower than the ordinary weekly percentage. Polling, retry, binding, spreading and activity intervals/counts have a minimum of 1; `near_reset_secs` may be 0. Under `[routing.churn]`, dwell/revisit are 0–86400 seconds, the window is 1–86400 seconds, group moves are 1–16, pool moves 1–32 and retained move bytes 1–67108864. `quota.poll_min_interval_secs` is 1–3600. WebSocket idle time and message size must be positive; idle time is clamped below the rebalance idle interval. The original configuration file is never rewritten by these corrections. Invalid types and unsafe upstream endpoints still refuse startup; global `strict` remains invalid (D20 §3).

An account's optional `spread_threshold` has the same minimum of 1 as the global threshold. Unspecified account thresholds continue to inherit the global value. Routing and WebSocket timer intervals without a tighter bound above are capped at 4294967295 seconds (over 136 years) so Windows and Chrono deadline arithmetic remains representable.

Port numbers are 0–65535 (0 retains the existing ephemeral-port behavior). Server drain time is 0–4294967295 seconds. Guard MiB fields retain their unsigned minimum of 0, and the tray burst count is 0–4294967295. These schema bounds are normalized before unsigned deserialization too. Unknown-key warnings retain the text `Unknown configuration key` and additionally carry the stable `unknown_config_key` code.

```toml
[server]
data_port = 10200            # 10100 after cutover
admin_port = 10201

[routing]
mode = "rebalance"                # recover also allowed; strict is per project/root only
request_timeout_secs = 300
forecast_shadow = false
placement_allowance_pct = 2.0     # D20 charge per new or moved group
weak_evidence_new_sessions = 1
placement_poll_after_secs = 120
placement_poll_min_interval_secs = 300
gate_5h_pct = 90
gate_weekly_pct = 90
gate_weekly_near_reset_pct = 98     # applies within near_reset_secs of the weekly reset
near_reset_secs = 18000
inflight_allowance_pct = 1          # per in-flight request, added to projected usage; tuned from the ledger
reset_hold_secs = 60
rate_hold_secs = 60                 # C6.3 total hold per request for request-rate 429s
rate_hold_max_retries = 3           # C6.3 in-place retries per request
ws_upgrade_hold_secs = 10           # any hold on a WebSocket upgrade; Codex connect timeout is 15 s
spread_threshold = 5                # C7.1: active session groups per account before placement spills to the next; a heuristic, never a refusal
active_window_secs = 300            # C7.1: a group counts as active for this long after its last response
overload_window_secs = 600          # C7.1: overload trigger window, mark budget (2 per account) and mark expiry
rebalance_idle_secs = 3600          # cold point; >= the longest prompt-cache TTL in use
session_binding_max_secs = 604800

[routing.churn]
dwell_secs = 1800
revisit_secs = 3600
window_secs = 600
max_group_moves = 3
max_pool_moves = 4
max_move_bytes = 8388608

[guard]
retain_max_per_request_mib = 64   # larger bodies are forwarded without retention (no failover)
inflight_budget_mib = 384         # shared: retained HTTP bodies + in-flight WS messages
memory_hard_limit_mib = 1024

[tray]
theme = "light"                  # "light", "dark", "system"; tray-v2.md §1
burst_new_sessions = 30           # per client per 10 minutes; toast on excess (R8 I-40)

[dashboard]
resolve_claude_project = true     # filename-only Claude project lookup; false disables it

[websocket]
idle_close_secs = 1800            # MUST be < routing.rebalance_idle_secs
max_message_size_mib = 192        # both legs; max_frame_size is set to the same value (Codex sends single-frame messages)
upstream_deflate = false          # D12: decided by the M3 A/B measurement; the loopback leg never negotiates deflate

[quota]
poll_min_interval_secs = 300

[accounts.<id>]              # per-account settings (also set from the tray)
drain = false
allow_credits = false
spread_threshold = 5         # optional per-account override (C7.1)
```

### `accounts/<id>.json`

`source` is `{"kind": "oauth"}`, `{"kind": "takeover", "from": "<source path>", "at": "<rfc3339>"}` (owned, refreshes like `oauth`; D18), or `{"kind": "cpa", "mode": "linked" | "adopted", "dir": "<CPA auth dir>", "match": {"account_uuid" | "account_id": "...", "email": "..."}}`. A linked account MUST NOT call any token endpoint. Its token comes only from re-reading the matched CPA file (architecture §6).

`source.mode`: `linked` never calls a token endpoint. `adopted` is owned: it uses refresh-ahead, 401 refresh and manual refresh. `dir` and `match` are kept on an adopted account so that `restore-cpa` can re-link it. `id` is stable across mode changes.

DPAPI is called with the application entropy, the ASCII bytes `quota-baton/v1/account-secret`, and with `CRYPTPROTECT_UI_FORBIDDEN`.

```json
{ "schema": 1, "id": "cx-1a2b3c", "provider": "codex",
  "label": "...", "email": "...", "plan": "plus",
  "source": { "kind": "cpa", "mode": "linked", "dir": "...", "match": { "account_id": "...", "email": "..." } },
  "identity": { "...provider ids per C10..." },
  "user_state": "active",
  "health": { "kind": "ready" },
  "quota_credit_releasable": false,
  "generation": 7,
  "token_meta": { "expires_at": "...", "last_refresh_at": "...", "refresh_uncertain": false },
  "secret_dpapi_b64": "..." }
```

`quota_credit_releasable` defaults to false when missing (C5).

The DPAPI blob decrypts to `{access_token, refresh_token, id_token?}`. Files are written atomically and generation-guarded (architecture §6), and only by the holder of the credential lease.

`refresh_uncertain` lifecycle:
- It is set to `true` and persisted **before** a refresh request is sent.
- It is cleared in the same write that persists the new tokens.
- If it is found `true` at startup, the previous refresh outcome is unknown. Retry at the next refresh-ahead point (architecture §6).
- `restore-cpa` is refused for an account whose flag is `true`, and the flag is never written into a CPA file.

### `journal/retire-cpa.json`

`{ "schema": 1, "started_at", "tasks": [{"name", "was_enabled", "owned_by": "retire"|"wiring"}], "process": {"pid", "path", "start_time", "stopped"}, "accounts": [{"id", "from": "cpa_file"|"store"|"backup", "cpa_file"?, "before_generation", "after_generation"?, "verified_refresh"?, "status": "pending"|"adopted"|"skipped", "reason"?}], "guard": {"kind": "files", "renames": [["<from>","<to>"]], "done"}, "completed_at"? }`

Each step is journaled before it runs and marked after it completes. `retire-cpa --apply` resumes, and `restore-cpa --apply` reverses from any step. `unwire --apply` is refused while this file exists.

### `state/bindings.json`

`{ "schema": 1, "bindings": [{ "key": "<session key>", "account": "<id>", "last_used": "...", "first_seen_at": "...", "client_kind": "codex_cli" }] }`

It is written behind, at most 60 s late, and flushed on drain and exit. It holds no conversation content.

`first_seen_at`, `bound_since` and `client_kind` are optional for backward compatibility. Dashboard labels, paths, model evidence, request counters and displayed raw-ID prefixes do not enter this file.

### Dashboard history and window geometry

`state/activity.jsonl` holds bounded, compacted 7-day typed activity. `state/usage-history.json` holds bounded 48-hour account-window observations, reset markers and credit spans. Both use atomic replacement and write-behind no more than 60 s late. Retention bounds account counts, points and physical allocations as well as time. Restart cannot reconstruct volatile counters; views mark that evidence unknown.

The activity archive also retains a typed sequence checkpoint when all events have expired. New activity after restart must have a sequence greater than every previously issued cursor; the checkpoint is not an activity event and is not returned in queries.

The tray writes `state/ui.json` atomically with schema and geometry only: position, size, DPI and maximized state. It never writes search text, selected session, filters, labels, charts or detail payloads. Dashboard data routes require the same authentication and Host/Origin checks as all other admin routes.

### Ledger line

```json
{"ts":"...","req":"...","family":"codex","route":"POST /v1/responses","transport":"http",
 "account":"cx-1a2b3c","session":"<sha256/12>","model":null,
 "originator":"codex_cli_rs","ua":"codex_cli_rs/0.162.1",
 "status":200,"upstream_status":200,"ttfb_ms":412,"dur_ms":38120,
 "bytes_in":812345,"bytes_out":91234,"attempts":1,"outcome":"ok","upstream_error_code":null}
```

## C13. Acceptance

See [architecture §16.3](architecture.md#163-acceptance-criteria) (A1–A5). Every row of C5 and C6 MUST have a `qb-testkit` test before M4a/M4b are accepted.
