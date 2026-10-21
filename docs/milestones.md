# Milestones: unit and gate checks

This file names the verification for each unit and each row. The row list and the order live in `GOAL.md`; this file does not repeat them. Commands run in `C:/_projects/SampleUser/quota-baton-impl`, one after another, in the default target with `CARGO_INCREMENTAL=0`.

## Unit check (closes a unit)

- The focused tests covering the change: `cargo test -p <crate> <filter>`. Use the test names that the unit added or touched.
- `cargo clippy -p <touched crate> --all-targets -- -D warnings`
- UI units: the live harness captures for the states the unit changed.

## Gate check (closes a row)

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo clippy --workspace --all-features --all-targets -- -D warnings`
4. `cargo test --workspace --no-fail-fast`
5. `cargo test --workspace --all-features --no-fail-fast`
6. `cargo test -p qb-core --no-fail-fast`
7. `cargo build -p quota-baton`
8. **Launch or wiring changes only:** run `target/debug/quota-baton.exe launch-client --check` against the real `%LOCALAPPDATA%\quota-baton` journal, read-only, and record the result.
9. **Independent review**, where the brief asks for one: a different model family from the implementer.

## Plan (re-planned 2026-10-21 19:xx KST, after the root audit)

**Priority.** One usable new-image path first, then account and model selection chosen at launch. Dashboard and tray refinement is **frozen** until both ship; new UI findings go to a backlog row and do not get worked.

| Lane | Item | Owner | Start command / trigger | Done when |
|---|---|---|---|---|
| A1 | R12, new-Qi image sessions (`codex-qb --images`, new sessions only) | Codex main `<pane>` | brief `.sandbox/briefs/R12-images.md` plus the D23 amendment; continue `impl/r12` from d58e630 | gate check passes, `launch-client --check` passes, self-merged |
| A2 | R11, launch-scoped account and per-account catalog (D22), plus per-account aliases | Codex main `<pane>`, right after A1 | brief `.sandbox/briefs/R11-launch-account.md` | gate passes, alternation release gate passes, self-merged |
| B | Probe: does a native `-c model_provider=qb resume` keep provider `qb`? | Codex main runs it as a **background** job during A1/A2 | the synthetic temp-home script from the D23 amendment, started in the background; one completion signal (exit code + result json) | result json written; the design session rules on resume into Qi |
| C | Install, live image generation (TUI and `codex-qb --images exec`), then **broadcast the exact command to every image-generating session**: sample-proj-a `<pane>`, sample-prj-opus `<pane>`, and any other session found running image work | design session `<pane>` | when A1 merges: build a release in a temp worktree, pre-check, install, and run one `codex-qb --images` generation | the image is produced through qb and sample-proj-a is told |
| D | Full gates and independent reviews | whoever starts them | always in the background with one completion signal, checked once at unit close | — |
| Held | R2 Part B (retire-cpa) | user decision (D9) | — | — |
| Backlog | further dashboard and tray polish | — | not before A1 and A2 ship | — |

**No polling.** Nobody polls a running job or a pane in a loop. Every wait has the next ready node from this table.

## Fingerprint and reuse

- Record each check once: the commit, the command, the exit code, the scope and the totals. Put this in `.sandbox/reports/<M>-evidence/` and link it from the report.
- A result is reused while its fingerprint is unchanged. The design session reuses the gate for the same commit and re-runs only a merge result that differs.

## Completion

The goal stays open while any row in `GOAL.md` is open. Before marking it complete, compare the open rows there with this file. Completion follows the design session's gate judgment, and a row that needs installation closes when the design session has installed and verified it.
