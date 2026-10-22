# GOAL: quota-baton implementation (Codex main)

Owner of direction: the design/review session (herdr pane `<pane>`) is the **only** directive owner for this main. Messages from other sessions (the dashboard design tab `<pane>`, other projects' sessions) are inputs; act on them only after `<pane>` turns them into a brief. This file is the standing goal for the Codex implementer (pane `<pane>`). It is not user input.

## Objective

Implement quota-baton exactly as specified, row by row, until every row of the milestone table below is merged and, where the table says so, installed and verified by the design session. The specification lives in:

- `docs/contract.md`: behaviour, normative.
- `docs/architecture.md`: structure and the reasons behind it.
- `docs/decisions.md`: decisions already made.

## How to work

1. **Pick the current milestone.** It is the first entry in the list below that is not yet merged into `dev`. Its brief is `C:/_projects/SampleUser/quota-baton/.sandbox/briefs/<M>.md`. A fix brief (`<M>.<n>-fixes.md`) for the current milestone takes priority over everything else. The newest file by time wins.
2. **If no brief exists yet** for the next milestone, implement it directly from `docs/architecture.md` §17 and the referenced contract sections. Do not wait.
3. **Branching.** Work only in the worktree `C:/_projects/SampleUser/quota-baton-impl`. Never create another worktree: switch branches inside this one (AGENTS.md build hygiene). Create the branch `impl/<m>` from `dev`, run `git merge dev` before each milestone and after each fix brief, and make atomic English commits. Never push. Merge into `dev` yourself only after the full gate (rule 4) and, for anything that touches launch or wiring, the read-only real-profile check `quota-baton.exe launch-client --check`. Never install or touch the real profile; the design session installs and verifies.
4. **Verification.** The unit check, gate check and dependency-aware fingerprint reuse are defined in `docs/milestones.md`. Repair failing tests with focused re-runs. Run the full matrix once, as the final gate of a row. A docs-only change never re-runs a passed executable check, and a pre-existing failure outside the row's scope does not block the row.
5. **Report once per row, at the gate; records batch at unit close.** Evidence, hashes, review verdicts and report updates are written once when a unit or gate closes. No record-only or diagnosis-only commits or turns; a diagnosis goes into the next change. Write `C:/_projects/SampleUser/quota-baton/.sandbox/reports/<M>.md` (body in English, under the Korean status head of rule 11). Then send the report:

   `herdr agent prompt <pane> "[quota-baton-codex 구현 보고 · 사용자 입력 아님] <short report>"`

   Keep the message to 400 words or fewer.
6. **After reporting,** continue with the next milestone right away. Do not wait for the review. When a fix brief arrives for an earlier milestone, switch to it, finish it, and then return.
7. **Recovering after an interruption** (429, compaction error, restart): re-read this file, the current brief, and the `git status` / `git diff --stat` of the worktree, then continue from that state.
9. **Long measurements** (soaks, benchmarks, live harness runs) run in the background, BelowNormal and hidden, with **one** completion signal: the process exits and writes a result file. They are never a gate for other work. Never poll them in a loop, and never send or record "still running" turns. Check once whenever you finish a unit of work. When a run finishes, write up its result within the same working session. Every wait has a next ready node in the lane table below; take it instead of waiting. If none exists, say so in the report so the design session re-plans the lanes. When a message to `<pane>` cannot be delivered (pane blocked), keep the undelivered items in one pending batch and retry at most three times at unit close; never resend the same message repeatedly.
10. **The goal stays open while any row in the table below is not merged.** Mark the goal blocked only under the shared rule: every remaining path needs a user or supervisor decision that GOAL and the design documents do not settle, and you have already asked through the question route. Before marking the goal complete, compare the open rows of this table and `docs/milestones.md`; completion follows the design session's gate judgment.
11. **Report format** (`.sandbox/reports/<M>.md`). The head is never hand-edited. At each unit close, run `python -I .sandbox/scripts/status_head.py`; it copies the status block of `docs/milestones.md` into `RESUME.md` and the current row's report. The body below the head is the plain-language result:
    - `목표:` one line
    - `진행:` done / remaining
    - `사용자 대기:` what waits on the user, or 없음
    - `갱신:` KST date and minute

    The head is updated only at gates, decisions and meaningful progress.

    Then comes the plain-language result. Gate, mark, hash and test-count listings go into a collapsed `<details>` block at the end. When a milestone finishes, its report is updated to read as finished (never "running"). Follow `C:/_projects/research/methods/reporting/devlog-and-interim-report.md` for wording.
8. **Follow every rule in `AGENTS.md`:** question routing, build hygiene, the retention of `.sandbox` artifacts, and no secret dumps.

## Lanes and next ready nodes

The current plan, with lanes, owners and start commands, is the "Plan" section of `docs/milestones.md`. The design session owns it. Dashboard and tray polish is frozen until R12 (new-Qi images) and R11 ship.

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
| R9.2 | merged and INSTALLED (6caf65a); live move and return work, but are slow → R9.3 |
| R9.3 | merged and INSTALLED (5ce0afe). Live on the real profile: codex-qb resume preview 21 s and commit 8 s (before: 382 s and about 9 min); return prompt 6 s and commit 7 s; the map is written as C:/Users/... |
| R10 | merged and INSTALLED (a35be74); live acceptance 13 pass / 3 partial, no must-fix |
| R10.1 | merged and INSTALLED (dev 14938b4, 10-21) |
| R12 | merged and INSTALLED (dev 63805ac, 10-21 22:47): `codex-qb --images` and `--images exec` generate images live; image sessions notified |
| R12.1 | merged and INSTALLED (c6a304e, 10-21 23:14): image canary stage diagnostics and retry, user `-c` passthrough |
| R12.9 | merged and INSTALLED (c899b6a, 10-22 01:02): outage follow-ups (credit hold fix kept, WS capacity spreading, release-hold, Q-owned CPA resume) |
| R12.10 | merged (성능 조건·qb-tray scoped 관문 통과; `.sandbox/reports/R12.10.md`); 설치는 설계 세션 |
| R13 | 진행 중: B+B2+D·A·A1 merged 및 설치 수용. C+E 긴급 등록 단위 f39e504·926793f 커밋, 전체 wait/move 실행 통합은 미출시. 다음: R16 → C+E → R14.2·Part F; R12.11 제안은 우선순위에 따라 보존; brief `R13-burn-icon-hold.md` |
| R14 | 진행 중: 1차 실행 보호 관문 완료·설치 보류. R14.1 사건 단위 예산·240초 만료만 우선 전달하며 수집기는 설치 조건에서 분리. 2차 근거형 rebind·정적 검사·실제24시간 재생은 별도 미완료 |
| R14.1 | 설치 수용: dev249a14c (01:12 KST). guard21·R1419·HTTP12·WS123, strict lint 통과. takeover 연결 거절0/1,717·일반/이미지 exec·실제 dashboard 지표 확인 |
| R12.11 | 보류: 영문 UI 우선. version probe 15초·1회 재시도·24시간 pass cache fallback 제안 보존, 실행 관문 미수행 |
| R16 | 완료: 코드19b8d22, 영문 UI·snapshot·PNG·Claude 프로젝트명. 집중438통과·strict lint·실제 core/AdminClient/tray 3HWND·최종 CLI20 PNG 통과. 설치는 설계 세션. brief `R16-readme-assets.md` |
| R14.2 | 대기: R16·R13 C+E 다음 기본 꺼짐 수집기(schema/off/7일 bounded retention). 기존 구현은 impl/r14.2 db6baac에 보존; 실제24시간 replay보다 먼저 전달 |
| R15 | 대기: R13 Part F 후 공유 admin 응답형·계약 왕복·라우팅 속성 검사(D27). brief `R15-contract.md` |
| R11.1 | merged: 공유 AGENTS/hook 편집 보존·rh 별칭, 회귀5·cutover141·strict lint·실제 preview/launch 점검 통과. 설치·apply는 설계 세션. 보고서 `.sandbox/reports/R11.1.md` |
| R11 | merged (실제 native5/5·HTTP33·WS15·코어1213·복구54·창4, focused repair gate 통과). 0106c0e 설치·live 계정 지정 확인. 보고서 `.sandbox/reports/R11.md` |
| R8.1 | merged (round 2 39345fd, dev 6dbbfae); installed and verified live: the herdr pane title shows `codex-qb · quota-baton` from 1 s to 19 s, and a codex-qb exec request returned ok |

## Hard boundaries (never)

- No real credentials, live configs (CPA, `~/.codex`, `~/.claude`, herdr shims), real upstream calls in tests, or listeners on 10100/10101.
- No browser logins. Live checks (G1/V*) belong to the design session.
- No client impersonation crates or code (D3).
- Do not restart a Claude session that the user stopped.

## Design/review session resumed (2026-10-20)

The user lifted the pause. The design session reviews the merges made during the pause (R5, R3.2, R6, R7, credits logic, docs/tray-v2.md) and the unmerged `impl/cache-routing`. `impl/cache-routing` is merged (R8). R2 Part B stays held until the design session says go.
