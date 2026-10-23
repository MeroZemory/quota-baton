# GOAL: quota-baton implementation (Codex main)

Owner of direction: the design/review session (herdr pane `<pane>`) is the **only** directive owner for this main. Messages from other sessions (the dashboard design tab `<pane>`, other projects' sessions) are inputs; act on them only after `<pane>` turns them into a brief. This file is the standing goal for the Codex implementer (pane `<pane>`). It is not user input.

## Objective

Implement quota-baton exactly as specified, row by row, until every row of the milestone table below is merged and, where the table says so, installed and verified by the design session. The specification lives in:

- `docs/contract.md`: behaviour, normative.
- `docs/architecture.md`: structure and the reasons behind it.
- `docs/decisions.md`: decisions already made.

## How to work

1. **Pick the next ready node.** The plan in `docs/milestones.md` lists nodes with their real inputs, lane, start and pass signal. Among ready nodes, take the most important one (user-blocking bugs, money and safety and long waits, user requests, small fixes, structure). Serialize only the shared resources named there: S1, the implementation worktree, with one branch and one cargo at a time; S2, the live install. Start long waits and build-free work as background lanes with one completion signal. Briefs live in `C:/_projects/SampleUser/quota-baton/.sandbox/briefs/`.
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
| R9.4 | 완료: 정규 .jsonl DB 경로의 압축 형제 탐색·하드링크 이동·재개·doctor 복구. 집중 관문 통과, self-merge 후 설계 세션 설치. 보고서 `.sandbox/reports/R9.4.md` |
| R10 | merged and INSTALLED (a35be74); live acceptance 13 pass / 3 partial, no must-fix |
| R10.1 | merged and INSTALLED (dev 14938b4, 10-21) |
| R12 | merged and INSTALLED (dev 63805ac, 10-21 22:47): `codex-qb --images` and `--images exec` generate images live; image sessions notified |
| R12.1 | merged and INSTALLED (c6a304e, 10-21 23:14): image canary stage diagnostics and retry, user `-c` passthrough |
| R12.9 | merged and INSTALLED (c899b6a, 10-22 01:02): outage follow-ups (credit hold fix kept, WS capacity spreading, release-hold, Q-owned CPA resume) |
| R12.10 | merged (성능 조건·qb-tray scoped 관문 통과; `.sandbox/reports/R12.10.md`); 설치는 설계 세션 |
| R13 | 진행 중: B+B2+D·A·A1 merged 및 설치 수용. C+E 긴급 등록 단위 f39e504·926793f 커밋, 전체 wait/move 실행 통합은 미출시. 현재: N5a HTTP·N5b WS/SSE 집중 관문 완료(N5b library909·WS139·HTTP115, 기본/testkit strict lint·fmt; c79b69c); N5c 긴급 정책 실행 집중 관문 완료; N10 설치 완료; N5d 시간 제한 종료·판단/HTTP 검증 보존, 전체 native 수용은 N5d-native 후속; N5i 집중 관문 완료(테스트 실행324·WS target1·실제 첫 클릭1); 설치 수용(dev27499b8·22:11); 측정·백테스트는 독립 lane; R12.11 제안은 우선순위에 따라 보존; brief `R13-burn-icon-hold.md` |
| R14 | 진행 중: 1차 실행 보호 관문 완료·설치 보류. R14.1 사건 단위 예산·240초 만료만 우선 전달하며 수집기는 설치 조건에서 분리. 2차 근거형 rebind·정적 검사·실제24시간 재생은 별도 미완료 |
| R14.1 | 설치 수용: dev249a14c (01:12 KST). guard21·R1419·HTTP12·WS123, strict lint 통과. takeover 연결 거절0/1,717·일반/이미지 exec·실제 dashboard 지표 확인 |
| R12.11 | merged: df5bf72·집중58통과/1명시제외·strict CLI lint·real launch check·두 반례101→복원0. version15초·한 번 재시도·24시간 pass fallback, 잘못된 출력은 즉시 거절. 설치는 설계 세션 |
| R16 | 완료: 코드19b8d22, 영문 UI·snapshot·PNG·Claude 프로젝트명. 집중438통과·strict lint·실제 core/AdminClient/tray 3HWND·최종 CLI20 PNG 통과. 설치 수용 dev006449a·02:26 KST·takeover 거절0/1,811. brief `R16-readme-assets.md` |
| R16.1 | 완료3737fe2·집중405·strict lint·D27 3HWND·KO PNG4개 동일: 영어 탭·표 제목·사용량 요약 폭 보완. 한국어 출력 보존, 1024×640·1320×900 scale2 실제 renderer 회귀·D27 경로 통과, self-merge 후 설계 세션 설치 |
| R16.2 | 완료·설치 수용: dev ddf8d87, 04:34 KST; 실제 dashboard 확인·takeover 연결 거절0/1,959 |
| N10 | merged·설치 수용(dev ef75624, 16:52; takeover 연결 거절0/1,278): D30 자동 외부 사용 감지(설정 없음)와 WS spill anti-herding. 외부 사용 표시는 현재·다음 window 동안 기존 세션 이동 대상에서 제외, 새 배정은 최후 수단. own-slot 대기 뒤 spill D20 charge/약한 근거 cap·새 사용량 관측 전 대상별1개. 4세션 reset 회귀·외부 상승/자체 트래픽/clean window 해제·Daybreak 소스 연구. N5c 바로 뒤·N5d 앞 |
| R13.G | 완료·설치 인계: 먼 5h 한도 이동·WS 426 fallback·launch 활동 보존·사용 불가 계정 재개·오류 표시. 코어11·R11 HTTP10·native-owner HTTP2·실제 WS4·콘솔1·인접 회귀·기본/testkit strict lint·제품 build/read-only check 통과; Sol 지적 보완. v0.4.0 설치·공개는 설계 세션 |
| N5d-native | 진행 중: 실제 native0.164 제품 런처→TUI→core→합성 HTTP fallback에서 추가 사용자 조회 없이 재개1회·반복2 episode·동일 UUID 후속200·각 정상종료0·pause 무재개 통과(757c1a3). CLI169·proto5·core13 및 strict lint, 독립 Sol source PASS. 기본 WS quota producer·Claude continue 전체 수용 미완료, 일반 자동 재개 비활성. 이 호환성 결정은 관문 질문으로 전달하고 독립 N6t 진행 |
| R13.UI | 완료: 세션 행35 DIP·메모 열 앞 긴급 토글. 한영 1024/1320 geometry·tray345·strict lint·실제 core/AdminClient/tray4HWND 관문 통과. 보고서 `.sandbox/reports/R13.UI.md`; 설치는 설계 세션 |
| N11 | 완료: dev 기존 tray6실패는 모두 오래된 fixture. 제품 변경 없이 compact summary/tooltip·측정 geometry·normal startup logging fixture 복구. 대상·인접36검사·strict tray lint·fmt 통과; 보고서 `.sandbox/reports/N11.md` |
| R9.5 | 완료(N3): 즉시 안내·5초 heartbeat·단계별 안전 취소·명시적 복구·한영 읽기 쉬운 확인. 합성 실제 CLI 집중 관문·read-only launch check 통과, 설치 인계. 보고서 `.sandbox/reports/R9.5.md` |
| R9.6 | 완료: DB·index·첫 사용자 줄 제목 순서, 48칸·한영 출력·단일 집계표 숨김. 집중 관문 통과; 보고서 `.sandbox/reports/R9.6.md` |
| R16.3 | 완료·설치 수용(41cb129, 05:26 KST, takeover 거절0/1,889): 집중464·strict lint·KO/EN 실제10HWND·합성8PNG 통과. 제공자 주간/30일 계정분·권장선·음영·소진 사실 표시. 설치는 설계 세션 |
| R16.4 | 완료·설치 수용: 32b52ea, 06:07 KST; 실제 coverage4/4·takeover 연결 거절0/1,923 |
| R16.5 | 완료: 7cab418 잔여량 이력 유지·소진0·리셋100·한영 결과 문구. 집중466·strict lint·실제 core/AdminClient/tray10HWND·export2PNG 확인. 19353ac 설치 수용(06:58)·takeover 거절0/1,920; 다음 N5a |
| R14.2 | 완료(N6 최소): 기본 꺼짐·schema3·bounded 7일 보존·off switch·실제 합성 HTTP/종료 집중 관문. 초기 cut/전체 owner/실제24시간 replay는 N6t/N6r 별도; 128 MiB 기록 활성화. 설치 동결 해제: 여러 epoch의 24시간 export 허용, epoch별 초기 상태·loss manifest로 각각 재생한 뒤 합산; 단일 epoch를 기다리지 않음 |
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

