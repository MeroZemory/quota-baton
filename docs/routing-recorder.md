# Routing metadata recorder: minimum delivery

The minimum-delivery description below is historical. Current enabled builds use
the decision-local tape described at the end of this page, with the same opt-in
configuration.

This optional diagnostic recorder is off by default. It does not change account
selection, move protection or request retry rules. D29 in [decisions.md](decisions.md)
owns the purpose and the later real-trace replay requirement.

The operator can opt in before starting a core:

```toml
[routing_replay]
enabled = true
disk_limit_mib = 512
hourly_encoded_byte_limit = 40_000_000
```

Set `enabled = false` and restart or take over to stop capture. The default is
`false`; an off start creates no capture directory, key or writer. The disk budget
defaults to 512 MiB and must be between 16 and 512 MiB, including when capture is off. Invalid configuration
is rejected rather than silently clamped.

The `hourly_encoded_byte_limit` option defaults to 40,000,000 encoded bytes per
hour. Operators can set it between 1 and 512,000,000 bytes, including when capture
is off; invalid values are rejected rather than clamped.

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

## Decision-local selection and permit tape

Current builds write `state/decision-tape-v8/`. The native offline reader also
accepts sealed schema-5/6/7 exports; older roots are neither rewritten nor removed.
Each epoch uses the configured encoded-byte hourly stop-go budget (40,000,000 by
default) and retains explicit loss counters. Export the whole sealed epoch with
its header, manifest and all segments; a live or partial directory is insufficient.

Permit inputs retain native quota status strings, including literal `true` and
`false`, without interpreting them as allowed or rejected. The startup witness
commits restored request-cost uncertainty without clearing it or claiming a
complete owner image. The manifest counts `permit_not_comparable_reasons` and
`producer_failure_reasons` using closed diagnostic enums. Their checked totals
must equal the aggregate counters. No raw error or rejected payload is retained.

The tape covers selection and permit proposals; completed rebinds are completeness
markers, not replayed commits. More than
1% not-comparable records makes a replay insufficient. A tape containing only
same-account associations cannot establish the power of a changed move guard;
zero observed differences on such a tape is not power acceptance.

The current `state/decision-tape-v8` root has its own disk budget across its epochs.
Legacy sibling roots are not charged to it. Dictionary, decoded-epoch and epoch-slot
bounds are separate from disk space; a long capture can consist of several sealed
epochs, each replayed from its own initial witness.
