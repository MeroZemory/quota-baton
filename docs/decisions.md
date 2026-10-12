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
## D5. Accounts only through quota-baton's own OAuth — Accepted (user, 2026-10-13)


Reasons:
- shared refresh-token families get revoked

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
## D10. Mid-session hand-off policy — Proposed (decided per provider family by Phase 0 test V2, then user approval)

A session changes account only when its account becomes ineligible (contract C7). Which branch applies depends on whether the encrypted conversation items that clients replay are accepted by another account. For Codex these are encrypted reasoning and remote-compaction blobs; for Claude, thinking signatures. That is unknown, and the evidence conflicts (architecture §2.2).

- **Portable:** pure pass-through. The only cost of a switch is one cold prompt cache.
- **Not portable:**
  - apply hand-off rewrites E2 (Codex) and E3 (Claude) from contract C4
  - the cutover tool offers plaintext Codex compaction
  - a switched session loses earlier reasoning continuity, but keeps working

Rejected alternative: keeping full transcripts in the proxy, as earlier proxies do. It does not fix account-bound blobs, and it is the measured cause of their memory use (R6, R7).

E2r, the reactive strip-and-retry rule, is **proposed** as a safety net for both branches. It recovers sessions whose binding memory was lost, and it covers changes in server behaviour. It needs the user's approval, as every C4 exception does.

<a id="d11"></a>
## D11. Account-switching policy — Accepted (decided by a P2 blind panel at the user's request, 2026-10-13)

The user delegated this decision to independent reviewers. Three models answered a neutral sheet blind: Opus 5.5 max, Fable 5.1 xhigh and Codex gpt-6.1-sol. The sheet, the answers and the union of conditions are in `.sandbox/reviews/switch-policy-blind-panel.md`. The normative rules are in contract C7.

**Goal:** minimize tokens re-read after cache misses, keep perishable quota from expiring unused, and need no user attention.

1. **New sessions: soonest weekly reset first (unanimous).**
   - The choice is limited to accounts that pass a gate on projected usage (in-flight requests included): 5-hour below 90 %, weekly below 90 %, or below 98 % within about 5 h of its reset.
   - If none passes, the account with the most headroom is used.
2. **Live sessions: move only when forced or free ("staged 1B"; Opus and Fable chose 1B, Codex 1A).**
   - **Forced:** the account becomes unusable (a limit, auth still failing after a refresh, or a user pause). A limit that resets within 60 s holds the request instead.
   - **Free:** after at least 1 h idle, when the cache is cold on every account.
     - Until V2 shows that conversation items are portable, a cold session moves only if its current account fails the gate. This keeps Codex's objection: idle does not prove that reasoning continuity survives a move.
     - After V2, it is re-placed like a new session.
3. **Rejected:** restoring immediately when a preferred account recovers (unanimous), and moving warm sessions early to use expiring weekly quota (unanimous). New-session placement serves that goal without a cache cost.
4. **No priority list.** The only manual placement setting is per-account **drain** (no new sessions; existing sessions stay). `allow_credits` stays off by default.
5. **Tuning from data:** the ledger records moves, the weekly quota left unused at each reset, and forced moves per gate crossing.
6. **Caution:** the 88 % post-switch Codex cache figure may reflect shared prefixes rather than true cross-account sharing. Codex rules are not relaxed before a clean test.

<a id="d12"></a>
## D12. WebSocket permessage-deflate per leg — Proposed (design owner, 2026-10-13, after the design pre-check)

Codex offers permessage-deflate on every socket (`responses_websocket.rs:538-541`).

**Loopback leg (Codex ↔ quota-baton): declined.** Compression saves nothing on loopback, and it would cost CPU and per-socket zlib state.

**Upstream leg (quota-baton ↔ chatgpt.com): used.** Full-input messages are uncompressed: p95 33 MB, raw p99 51 MB. Text compresses 4–18×. A full resend happens after every idle close, every 60-minute server close and every failover.

**Library and memory.**
- This needs a deflate-capable WebSocket client, such as the openai-oss-forks tungstenite that Codex itself uses.
- The compressor is configured to keep as little idle state as the library allows. The A1 soak measures the per-pair memory.

**Alternative:** no deflate upstream. This is simpler and uses less memory per pair, but every full resend goes up uncompressed.

