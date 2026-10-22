# Routing metadata recorder: minimum delivery

This optional diagnostic recorder is off by default. It does not change account
selection, move protection or request retry rules. D29 in [decisions.md](decisions.md)
owns the purpose and the later real-trace replay requirement.

The operator can opt in before starting a core:

```toml
[routing_replay]
enabled = true
disk_limit_mib = 256
```

Set `enabled = false` and restart or take over to stop capture. The default is
`false`; an off start creates no capture directory, key or writer. The disk budget
must be between 16 and 512 MiB, including when capture is off. Invalid configuration
is rejected rather than silently clamped.

Files live under the core home's `state/routing-replay/`. There is one writer and
one root lock. Each start creates a separate epoch with fresh pseudonyms. A takeover
drains and joins the writer before handing over the credential lease, including
when an old request still holds a producer reference. Capture setup or storage
failure warns with a fixed code and does not prevent the ordinary relay starting.

The minimum producers record explicit allowlisted account state and candidate
collection boundaries/membership. Account and model identifiers use capture-local,
domain-separated HMAC pseudonyms. No prompt, response body, access/refresh token,
email, label, account source path or raw session identifier belongs in the schema.
The manifest declares schema, provenance, known build fingerprints, per-kind and
owner counters, losses and durable sequence coverage. Unknown fingerprints stay
unknown.

The queue, individual records, segment count, epoch count and root bytes are
bounded. Young data is not deleted to make room: capture stops at its cap and
records the loss. Running maintenance retains at most seven days. UTC rollback
does not authorize guessing an old epoch's age; known current-epoch monotonic age
can still expire its segments. The recorder does not erase data while the core is
off or started with capture disabled. An operator who needs immediate removal must
stop the writer and remove the dedicated capture directory deliberately.

Grouped file sync establishes the durable prefix before counters advance. Failed
write/sync and crash tails remain explicit losses or unknowns. A clean close does
not make the tape a complete replay input: `initial_state_reconciled`,
`owner_hooks_complete`, `replay_ready` and `actual_trace_accepted` remain false in
this minimum delivery. Policy, binding, quota, accounting, request and capacity
owner coverage is incomplete. A sanitized 24-hour export and a production-router
differential gate are separate work; neither an empty archive nor these synthetic
storage tests satisfy them.
