# GOAL: quota-baton implementation (Codex main)

Owner of direction: the design/review session (herdr pane `<pane>`). This file is the standing goal for the Codex implementer (pane `<pane>`). It is not user input.

## Objective

Implement quota-baton exactly as specified, milestone by milestone, until M8 is complete and merged. The specification lives in:

- `docs/contract.md`: behaviour, normative.
- `docs/architecture.md`: structure and the reasons behind it.
- `docs/decisions.md`: decisions already made.

## How to work

1. **Pick the current milestone.** It is the first entry in the list below that is not yet merged into `dev`. Its brief is `C:/_projects/SampleUser/quota-baton/.sandbox/briefs/<M>.md`. A fix brief (`<M>.<n>-fixes.md`) for the current milestone takes priority over everything else. The newest file by time wins.
2. **If no brief exists yet** for the next milestone, implement it directly from `docs/architecture.md` §17 and the referenced contract sections. Do not wait.
3. **Branching.** Work only in the worktree `C:/_projects/SampleUser/quota-baton-impl`. Never create another worktree: switch branches inside this one (AGENTS.md build hygiene). Create the branch `impl/<m>` from `dev`, run `git merge dev` before each milestone and after each fix brief, and make atomic English commits. Never push. Never merge into `dev` yourself; the design session merges after review.
4. **Verification.** Before a gate report, run the full set once: `cargo fmt --check`, strict clippy for the default features and for all features, workspace tests for the default and full feature sets, and the standalone `qb-core` tests. Run them one after another in the default target with `CARGO_INCREMENTAL=0`. **For a small fix inside a milestone**, run only the focused tests, plus clippy on the crate you touched. Do not re-run the whole matrix for each small fix. Record the gate run's **fingerprint** once in the report: the commit hash, each command and its exit code, and the test totals. The design session reuses that result for the same commit and runs its own check only on a merge result that differs from it. Unit checks: the focused test names covering the change, plus clippy for the touched crate.
5. **Report once per milestone, at the gate.** Write `C:/_projects/SampleUser/quota-baton/.sandbox/reports/<M>.md`. Then send the report:

   `herdr agent prompt <pane> "[quota-baton-codex 구현 보고 · 사용자 입력 아님] <short report>"`

   Keep the message to 400 words or fewer.
6. **After reporting,** continue with the next milestone right away. Do not wait for the review. When a fix brief arrives for an earlier milestone, switch to it, finish it, and then return.
7. **Recovering after an interruption** (429, compaction error, restart): re-read this file, the current brief, and the `git status` / `git diff --stat` of the worktree, then continue from that state.
9. **Long measurements** (soaks, benchmarks, live harness runs) run in the background, BelowNormal and hidden, with **one** completion signal: the process exits and writes a result file. They are never a gate for other work. Never poll them in a loop, and never send or record "still running" turns. Check once whenever you finish a unit of work. When a run finishes, write up its result within the same working session.
10. **The goal stays open while any row in the table below is not merged.** Mark the goal blocked only under the shared rule: every remaining path needs a user or supervisor decision that GOAL and the design documents do not settle, and you have already asked through the question route.
11. **Report format** (`.sandbox/reports/<M>.md`). The file opens with a current-status head in Korean:
    - `목표:` one line
    - `진행:` done / remaining
    - `사용자 대기:` what waits on the user, or 없음
    - `갱신:` KST date and minute

    Then comes the plain-language result. Gate, mark, hash and test-count listings go into a collapsed `<details>` block at the end. When a milestone finishes, its report is updated to read as finished (never "running"). Follow `C:/_projects/research/methods/reporting/devlog-and-interim-report.md` for wording.
8. **Follow every rule in `AGENTS.md`:** question routing, build hygiene, the retention of `.sandbox` artifacts, and no secret dumps.

## Lanes and next ready nodes (re-planned 2026-10-18)

| Lane | Items | Waits on | Next ready node while waiting |
|---|---|---|---|
| A: cutover tooling | R3 (codex-qb, claude-qb) → R2 Part B (retire-cpa / restore-cpa) | nothing (synthetic tests only) | — |
| B: tray | R1-tray.2 (notifications) | R3 finishing (same worktree, serial) | — |
| C: live | adopt/refresh live checks, the real dry run, `wire --apply`, `retire-cpa --apply` | the design session and the user (D9) | lanes A and B |
| D: background | measurements, soaks | completion signal | any open row of lanes A and B |

## Milestone order (architecture §17)

| Milestone | Status |
|---|---|
| M0, M1a, M2, M3, M4a, M4b, M5, M1b, M6a, M4c, M6b, M7a, M6b.2, M7b, M4d, M7c, M8, M6c | merged, including the M7c 24 h addendum (no upward trend). Running the cutover on the live profile is the user's decision (D9) and out of scope for you |
| R1 | merged (all five groups plus R1-tray.1) |
| R2 | D17: Part A (adopt + manual refresh + refresh regardless of user_state) right after the current R1 group, MUST merge before 10-22; then R1 groups 3-5; then Part B (retire-cpa/restore-cpa); brief `R2-adopt-retire-cpa.md` |
| R3 | merged (codex-qb and claude-qb) |
| R3.1 | dry-run display fixes from the real-profile dry run; before R1-tray.2 continues; brief `R3.1-fixes.md` |
| R1-tray.2 | situation-specific notifications naming the account (user request); right after R3; brief `R1-tray.2-notifications.md` |

## Hard boundaries (never)

- No real credentials, live configs (CPA, `~/.codex`, `~/.claude`, herdr shims), real upstream calls in tests, or listeners on 10100/10101.
- No browser logins. Live checks (G1/V*) belong to the design session.
- No client impersonation crates or code (D3).
- Do not restart a Claude session that the user stopped.
