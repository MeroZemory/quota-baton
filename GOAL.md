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
4. **Verification.** Run every check before reporting: `cargo fmt --check`, strict clippy for the default features and for all features, workspace tests for the default and full feature sets, and the standalone `qb-core` tests. Run them one after another in the default target with `CARGO_INCREMENTAL=0`.
5. **Report once per milestone, at the gate.** Write `C:/_projects/SampleUser/quota-baton/.sandbox/reports/<M>.md`. Then send the report:

   `herdr agent prompt <pane> "[quota-baton-codex 구현 보고 · 사용자 입력 아님] <short report>"`

   Keep the message to 400 words or fewer.
6. **After reporting,** continue with the next milestone right away. Do not wait for the review. When a fix brief arrives for an earlier milestone, switch to it, finish it, and then return.
7. **Recovering after an interruption** (429, compaction error, restart): re-read this file, the current brief, and the `git status` / `git diff --stat` of the worktree, then continue from that state.
8. **Follow every rule in `AGENTS.md`:** question routing, build hygiene, the retention of `.sandbox` artifacts, and no secret dumps.

## Milestone order (architecture §17)

| Milestone | Status |
|---|---|
| M0, M1a, M2, M3, M4a, M4b, M5, M1b, M6a, M4c, M6b, M7a, M6b.2, M7b, M4d, M7c | merged (M7c 24 h trend result pending as an addendum) |
| M8 | brief `M8.1-fixes.md`; cutover tool: journaled wire/unwire with dry-run. Building it is in scope; running it on live configs is the user's decision and is out of scope for you |

## Hard boundaries (never)

- No real credentials, live configs (CPA, `~/.codex`, `~/.claude`, herdr shims), real upstream calls in tests, or listeners on 10100/10101.
- No browser logins. Live checks (G1/V*) belong to the design session.
- No client impersonation crates or code (D3).
- Do not restart a Claude session that the user stopped.
