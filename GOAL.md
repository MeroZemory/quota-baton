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
| R2 | Part A merged (adopt, manual refresh, refresh regardless of user_state). Part B (retire-cpa/restore-cpa) is held by the user's design/review pause below; do not change the real profile or install. Resume only when that hold is lifted. Brief: `R2-adopt-retire-cpa.md` |
| R3 | merged (codex-qb and claude-qb) |
| R1-tray.2 | merged (notifications) |
| R4 | merged (plan tier, credits_in_use, precondition) |
| R5 | merged (`6feb99e`; full gate passed, including default/full workspace, standalone core, strict clippy and actual native captures/actions/memory). Report: `.sandbox/reports/R5.md`. Latest approved healthy bars are green; credits confirm every time |
| R3.2 | merged (`f3fc3f7`; full gate: default 966, full 994, standalone core 328; strict clippy passed). Report: `.sandbox/reports/R3.2.md`. codex-qb/claude-qb set the window title (launcher title + Codex `[tui] terminal_title = []`, Claude title env), with an in-place upgrade path; after R5; brief `R3.2-launcher-titles.md` |
| R6 | merged (`2245050`; full gate: default 1,056, full 1,085, standalone core 363; strict clippy and native Codex/Claude inspect/cancel/new/replace passed). Report: `.sandbox/reports/R6.md`. D18 takeover preserves ids, protects one-use refresh recovery, and keeps 30-day windows visible |
| R7 | merged (`4bbe855`; full gate: default 1,176, full 1,205, standalone core 410; strict clippy and actual spawned-core dashboard captures/actions passed). Report: `.sandbox/reports/R7.md`. Separate dashboard, shared SSE, bounded session/activity/usage views and memory-only labels; no real profile/install changes |
| R8 | merged (dev 269ac3b). Combined-tree test 1399/0/14 (`.sandbox/reports/merge-269ac3b/test.log`). Installed dev 20cb486 on 10-20 17:58; live codex-qb request ok, placement ledger shows evidence=fresh, charge=2 |
| R8.2 | merged (20cb486, ff; m4a_ws 89/0) |
| R8.3 | merged (67fb1ed; tray PE subsystem 2); installed 10-21 03:01, a normally started tray has no console (no conhost child) |
| R8.4 | merged (`922a1f1`; scoped gate 256 pass, 7 explicit live tests excluded; strict qb-tray clippy and fmt, actual light/dark overview/exhausted HWND captures; Sol max closeout PASS). Report: `.sandbox/reports/R8.4.md` |
| R9 | merged (dev 11851c2). Merge review INSTALL-AFTER-FIXES → R9.1 before install |
| R9.1 | merged (fd7a8dc), but the live install was ROLLED BACK: every codex-qb launch failed on the real profile → R9.2 |
| R9.2 | URGENT: accept R3-era wiring journals (no install_completed), real-profile read-only check before install-ready, wire upgrade tolerating Codex config.toml rewrites; before R10; brief `R9.2-live-install-blocker.md` |
| R10 | dashboard v2 + chip-limit colour in the dashboard and tray (user-approved, O1=A); O2 duplicate quota_reached; O3 default labels; after R9; brief `R10-dashboard-v2.md` |
| R11 | launch-scoped account and per-account model catalog (D22, resolves O13: /daybreak through codex-qb); after R10; brief `R11-launch-account.md` |
| R8.1 | merged (round 2 39345fd, dev 6dbbfae); installed and verified live: the herdr pane title shows `codex-qb · quota-baton` from 1 s to 19 s, and a codex-qb exec request returned ok |

## Hard boundaries (never)

- No real credentials, live configs (CPA, `~/.codex`, `~/.claude`, herdr shims), real upstream calls in tests, or listeners on 10100/10101.
- No browser logins. Live checks (G1/V*) belong to the design session.
- No client impersonation crates or code (D3).
- Do not restart a Claude session that the user stopped.

## Design/review session resumed (2026-10-20)

The user lifted the pause. The design session reviews the merges made during the pause (R5, R3.2, R6, R7, credits logic, docs/tray-v2.md) and the unmerged `impl/cache-routing`. `impl/cache-routing` is merged (R8). R2 Part B stays held until the design session says go.
