# Live forecast observations

The optional N7-L logger records hourly origins and later observations at one
and three hours. It does not enable a pooled forecast: both forecast values
remain null until a separately accepted model exists.

The existing configuration enables collection on the next core start:

```toml
[routing_replay]
enabled = true
```

Data is separate from the routing recorder, under `state/forecast-log` in the
qb data home. `checkpoint.json` carries pending observations across orderly
takeovers; `day-*.jsonl` contains sequenced origins and outcomes. Copy this
directory only after an orderly stop for a consistent offline export. It
contains compact account IDs and quota facts, not tokens, emails or bodies.

The sampler runs independently of the memory guard, once per minute. Each
origin freezes its usable account cohort. Remaining capacity is the sum of
each account's lowest remaining fraction across its windows, without plan
weights. User changes, including pause/resume and credit permission changes,
exclude affected pending origins. Missing or stale observations remain
unavailable rather than zero. Unclean restarts mark pending observations as
restart gaps.

The logger and provider chart share the Not Used classification: paused,
disabled, Claude Free/no Claude Code access and re-login accounts are excluded.
On native startup, a successful account-store list also removes quota, usage
and forecast cache entries for missing accounts; a failed list never becomes
an empty account set.

Codex `0m` and durationless `primary`/`secondary` slots are not capacity
windows and are excluded from remaining-capacity arithmetic. Positive native
durations, including `43200m` (30 days), retain their exact window identity.
An account with no capacity windows is unavailable, never assumed full.

A clean restart also checks the saved native account generation. If it changed
while collection was stopped, matching final settings cannot prove an unchanged
cohort: pending observations are excluded and counted as an observation gap.
This can conservatively exclude an offline automatic update too. Automatic
updates observed during live collection do not by themselves change the cohort.

For an installation check, confirm `forecast_log_open` with `stopped=false`,
then check that `last_sample` advances in the checkpoint and journal origins
appear. Record on/off, the journal byte increase per elapsed hour and the
checkpoint's `status.losses`, `unavailable` and exclusion counters. A live
checkpoint normally has `clean=false`. After shutdown, verify `clean=true`
and check for `forecast_log_close_failed` before claiming clean continuity:
core shutdown can complete even when this optional logger fails to close.
Startup or sample failures use `forecast_log_start_failed` or
`forecast_log_sample_failed`. Preserve the existing directory on failure;
deleting it discards pending observations and continuity evidence.

Warnings include finite `phase` and `reason` values, without raw errors or account
names. The checkpoint's `last_failure` retains the first diagnostic. An invalid
input sample increments `unavailable` and is retried on the ordinary sampling
schedule; it must not stop all future collection or manufacture an origin.
Storage, bounded journal capacity and persistence failures remain explicit
collection failures. The message
`Forecast log first sample recorded` confirms the first committed sample.

An upgrade can preserve an old stopped capture that has no origins,
outcomes, sequence, sample, pending work or journal. While holding the Store
lease after the old writer closed, it renames that checkpoint-only directory
to a sibling `forecast-log.failed-*` archive and creates a fresh capture. The
new checkpoint's `recovery` records the archive name and prior losses. Both
clean and unclean old checkpoints qualify; no earlier observations are filled
in. The known empty `sample/window_name` failure may also be preserved and
restarted, including its prior recovery receipt; that original receipt remains
inside the archived checkpoint. Captures containing history or other recorded
failure reasons are not reset automatically. Keep the sibling archives when
exporting recovery evidence.

Persistence is bounded to a 16 MiB checkpoint and 64 MiB total journal. During
active sampling, whole daily segments are pruned once their end is at least
seven days old; stopping collection does not automatically delete old files.
Observations sampled more than 120 seconds after
their target or using source data older than 300 seconds are unavailable.
These are collection limits, not forecast accuracy claims.

The seven-day collection interval starts with the installed producer's first
actual origin. Model work and forecast acceptance remain separate gates under
the fixed criteria in `GOAL.md`.
