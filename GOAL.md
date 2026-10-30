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
5. **Report once per row, at the gate; records batch at unit close.** Evidence, hashes, review verdicts and report updates are written once when a unit or gate closes. No record-only or diagnosis-only commits or turns; a diagnosis goes into the next change. This is enforced: in the implementation worktree a pre-commit hook rejects a commit whose staged files are only `GOAL.md`, `docs/milestones.md` or `docs/questions-for-supervisor.md`; put evidence and record updates into the implementation or gate commit they describe. Write `C:/_projects/SampleUser/quota-baton/.sandbox/reports/<M>.md` (body in English, under the Korean status head of rule 11). Then send the report:

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

## Outcome series (root audit 2026-10-28; definitions fixed here, the denominators never change)

Only these two series get metric rows in `docs/milestones.md`. A row is written only when an item is **accepted** (installed and passed its defining check), and never for preparation, component checks or briefs. Other work (user fixes such as R17, R17.1, R18) is reported in the report body and gets no series.

- **Series B "결정적 수용 (분모 3)"** — the three outputs that remain decisive for the routing goal:
  1. native continuation after a 5h reset, accepted in ordinary builds (accepted 2026-10-24, cc12102);
  2. **N6r accepted replay** (definition refined 2026-10-29 by the blind Opus max + Fable xhigh review, `.sandbox/reviews/N6r-stagnation-*.md`; denominator unchanged): the installed build records a **decision tape** at the router's selection entries (`choose_with_hold_at_admitted_proof_and_root_hint` and `place_launch_for_issue_*`): the inputs, the admission answers, a compact pre-decision state slice and the outcome, including Hold / None / Blocked / MoveHeld. `rebind` is a completeness cross-check. Over >= 24 h of that real capture, the installed and the candidate router are both run on every recorded decision. The verdict is valid only if (a) **identity**: the installed router reproduces its own recorded decisions, and (b) **power**: one planted decision difference is reported as exactly one difference and FAILs. Accepted when the candidate's aggregate moves are at most the installed router's (margin 0).
  3. **N7 accepted pooled forecast** (definition fixed 2026-10-29 by the same review, before any outcome was seen): the installed build logs every pooled forecast and its inputs live, carried across takeovers. Unit: account-equivalents of pooled remaining capacity, R = the sum over the usable cohort at the forecast origin of each account's lowest remaining fraction across its windows, which is the quantity the dashboard shows. Accepted when, over >= 7 days and >= 100 origins per provider and horizon, p90 |R_hat - R| <= 0.20 at 1 h and <= 0.40 at 3 h. These bounds are fixed and do not scale with the number of accounts. The forecast must also be no worse than persistence with scheduled resets. Pooled scoring needs N >= 2. An origin whose cohort the user changes within the horizon is excluded and counted, never scored as zero. Then the number is shown in the installed UI.
  Current value: 1/3. The old series ("새 경로 출시 2/2", "남은 출시 (분모 3)", the legacy 17/17 count) are closed and are not continued.
- **Series C "Linux 실제 수용 (분모 1)"** — L1 P5 accepted on the real home server per `.sandbox/reports/L1-evidence/p5-acceptance-checklist.md` (Incus box through the proxy device, key revocation, takeover upgrade with 0 refused connects, reboot survival, one browserless login per provider). Current value: 0/1.

**Data-fed gates (adopted 2026-10-29 from the same review).** A gate whose input is captured data has its own producer node, merged and installed first; no consumer is built without that producer. Its first unit ends with an end-to-end run on at least 1 h of real capture that includes the identity check, so a missing input is found on day one. Every install line reports capture health (recorder on/off, bytes per hour, losses). A unit closes with a verdict, and "insufficient" is a verdict.

**Stagnation rule.** If the next unit that works on a Series B item closes without accepting it, the design session runs a blind work-structure review (Opus max and Fable xhigh, independently, then compared) on how that item is being attacked, and adopts its result before the next unit on that item.

## Lanes and next ready nodes

The current plan, with lanes, owners and start commands, is the "Plan" section of `docs/milestones.md`. The design session owns it. Dashboard and tray polish is frozen until R12 (new-Qi images) and R11 ship.

## Milestone order (architecture §17)

| Milestone | Status |
|---|---|
| M0, M1a, M2, M3, M4a, M4b, M5, M1b, M6a, M4c, M6b, M7a, M6b.2, M7b, M4d, M7c, M8, M6c | merged, including the M7c 24 h addendum (no upward trend). Running the cutover on the live profile is the user's decision (D9) and out of scope for you |
| R1 | merged (all five groups plus R1-tray.1) |
| R2 | 완료(사용자 결정 2026-10-29): Part A merged. Part B는 별도 retire-cpa 명령 없이 수동 절차로 수행: CPA 작업 `cliproxyapi-proxy`·`cliproxyapi-quota-watch` 비활성화, CPA 프로세스 PID 종료, Claude 4계정 `accounts adopt`(CPA 파일 출처) → 모두 qb 직접 갱신. CPA 인증 파일은 이름을 바꾸지 않고 그대로 둠(작업 비활성화로 재시작 차단, 되돌리기 가능) |
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
| R13 | 진행 중: B+B2+D·A·A1 및 C+E 주 경로 설치 수용. N5a-c·N10·N5i 관문 보존; N5d-native 일반 release0.164 지원 새 실행 실제7/7 관문·12:10 설치 수용(별도 행). Part F 기본 사실 공유 계산 bc2a34a 집중503·실제 한영10HWND/2PNG 수용, 전체 관문·실패 집중 보완 완료(5f6d5f6), 원본 행렬 실패 보존·설치 인계. pooled runway는 실제24시간 보정/백테스트 전 계속 미표시; brief `R13-burn-icon-hold.md` |
| R14 | 진행 중: R14.1 실행 보호·R14.2 최소 기록기 설치 수용. N6t 계정 봉인·승인 영수증·정적 검사와 health·WS 용량·C7 재분산·실패/재연결·명시적 모델 동의·저장소·자격증명·핀 증거 전달 단위 검증 완료; owner 증거형 MoveReason·단일 이동 경로 집중 검증 완료(정적 반례67·semantic red4·private compile2 포함); dev 결합 실제 모델 컨텍스트 보완·Strict/런처 fixture 정리 후 N6t 관문 완료(ed2cb87, 입력별 전체 검사·집중 보완 지문). 단일 근거형 이동 경로 self-merge; 실제24시간 재생 N6r은 별도 미완료 |
| R14.1 | 설치 수용: dev249a14c (01:12 KST). guard21·R1419·HTTP12·WS123, strict lint 통과. takeover 연결 거절0/1,717·일반/이미지 exec·실제 dashboard 지표 확인 |
| R12.11 | merged: df5bf72·집중58통과/1명시제외·strict CLI lint·real launch check·두 반례101→복원0. version15초·한 번 재시도·24시간 pass fallback, 잘못된 출력은 즉시 거절. 설치는 설계 세션 |
| R16 | 완료: 코드19b8d22, 영문 UI·snapshot·PNG·Claude 프로젝트명. 집중438통과·strict lint·실제 core/AdminClient/tray 3HWND·최종 CLI20 PNG 통과. 설치 수용 dev006449a·02:26 KST·takeover 거절0/1,811. brief `R16-readme-assets.md` |
| R16.1 | 완료3737fe2·집중405·strict lint·D27 3HWND·KO PNG4개 동일: 영어 탭·표 제목·사용량 요약 폭 보완. 한국어 출력 보존, 1024×640·1320×900 scale2 실제 renderer 회귀·D27 경로 통과, self-merge 후 설계 세션 설치 |
| R16.2 | 완료·설치 수용: dev ddf8d87, 04:34 KST; 실제 dashboard 확인·takeover 연결 거절0/1,959 |
| N10 | merged·설치 수용(dev ef75624, 16:52; takeover 연결 거절0/1,278): D30 자동 외부 사용 감지(설정 없음)와 WS spill anti-herding. 외부 사용 표시는 현재·다음 window 동안 기존 세션 이동 대상에서 제외, 새 배정은 최후 수단. own-slot 대기 뒤 spill D20 charge/약한 근거 cap·새 사용량 관측 전 대상별1개. 4세션 reset 회귀·외부 상승/자체 트래픽/clean window 해제·Daybreak 소스 연구. N5c 바로 뒤·N5d 앞 |
| R13.G | 완료·설치 인계: 먼 5h 한도 이동·WS 426 fallback·launch 활동 보존·사용 불가 계정 재개·오류 표시. 코어11·R11 HTTP10·native-owner HTTP2·실제 WS4·콘솔1·인접 회귀·기본/testkit strict lint·제품 build/read-only check 통과; Sol 지적 보완. v0.4.0 설치·공개는 설계 세션 |
| N5d-native | 관문·설치 완료: f154b6a·일반 release native0.164 지원 새 실행 실제7/7(HTTP/WS 재개·pause/input veto·정상 종료). ownership39·entry13·strict lint·read-only check·fmt0. 임시 대화 수명 정리 전 봉인 보류; 모든 봉인 뒤 UI 취소 보존. Claude TUI 영구 이동, 미지원 argv/버전은 이동 유지. 이전 전체 검사 실패·집중 복구 입력별 지문 유지, 실제24시간 N6r 별도. 보고서 `.sandbox/reports/R13.N5d-native.md` |
| R13.UI | 완료: 세션 행35 DIP·메모 열 앞 긴급 토글. 한영 1024/1320 geometry·tray345·strict lint·실제 core/AdminClient/tray4HWND 관문 통과. 보고서 `.sandbox/reports/R13.UI.md`; 설치는 설계 세션 |
| N11 | 완료: dev 기존 tray6실패는 모두 오래된 fixture. 제품 변경 없이 compact summary/tooltip·측정 geometry·normal startup logging fixture 복구. 대상·인접36검사·strict tray lint·fmt 통과; 보고서 `.sandbox/reports/N11.md` |
| R9.5 | 완료(N3): 즉시 안내·5초 heartbeat·단계별 안전 취소·명시적 복구·한영 읽기 쉬운 확인. 합성 실제 CLI 집중 관문·read-only launch check 통과, 설치 인계. 보고서 `.sandbox/reports/R9.5.md` |
| R9.6 | 완료: DB·index·첫 사용자 줄 제목 순서, 48칸·한영 출력·단일 집계표 숨김. 집중 관문 통과; 보고서 `.sandbox/reports/R9.6.md` |
| R16.3 | 완료·설치 수용(41cb129, 05:26 KST, takeover 거절0/1,889): 집중464·strict lint·KO/EN 실제10HWND·합성8PNG 통과. 제공자 주간/30일 계정분·권장선·음영·소진 사실 표시. 설치는 설계 세션 |
| R16.4 | 완료·설치 수용: 32b52ea, 06:07 KST; 실제 coverage4/4·takeover 연결 거절0/1,923 |
| R16.5 | 완료: 7cab418 잔여량 이력 유지·소진0·리셋100·한영 결과 문구. 집중466·strict lint·실제 core/AdminClient/tray10HWND·export2PNG 확인. 19353ac 설치 수용(06:58)·takeover 거절0/1,920; 다음 N5a |
| R14.2 | 완료(N6 최소): 기본 꺼짐·schema3·bounded 7일 보존·off switch·실제 합성 HTTP/종료 집중 관문. 초기 cut/전체 owner/실제24시간 replay는 N6t/N6r 별도; 128 MiB 기록 활성화. 설치 동결 해제: 여러 epoch의 24시간 export 허용, epoch별 초기 상태·loss manifest로 각각 재생한 뒤 합산; 단일 epoch를 기다리지 않음 |
| R13.H | 완료·설치 수용(82dee98, 10-24 16:16, v0.5.0): 집중 관문 완료·dev 병합: bound WS 용량 제한은 같은 계정 HTTP 426 경로, 새 출발 계정 소진 후 유일한 복귀처에 한정된 복귀 예외·이동 예산 보존. 집중 관문 후 dev 병합, 설치는 S2 |
| R15 | 완료(N8): 공유 중첩 admin 계약·실제 응답 왕복·정적 guard·라우팅 속성. b492eda 최종 보완; 원본 기본16실패 원인별 집중 복구, 전체 기능3899/0/36제외·코어1310/0/1제외·기본/전체 strict lint·build/read-only check 통과. 실제 core→AdminClient→tray KO/EN HWND10 및 추가PNG2 직접 검수. 설치 S2; 지문 R15-evidence/delivery-fingerprint.json |
| L1 | 진행 중: P5 서버 수용 대기(홈 서버 꺼짐). 구현 P0–P4 관문 완료·P4 설치 3217fb2 수용·P5 배포 준비 관문 완료: D31, brief `.sandbox/briefs/L1-linux-headless.md`. 안내 포함 소스53aeb9a의 Bookworm Rust1.93.1 배포본(최대 GLIBC2.34), Ubuntu24/systemd255 인증 READY·정상 TERM·포트 닫힘·Linux strict 통과. 배포 묶음과 원본 실패·집중 보완은 `.sandbox/reports/L1-P5.md`. 실제 공급자 완료 로그인 0/2·실제 Incus 칸·철회·업그레이드·재부팅·공유 계정 외부 사용 감지는 P5 실제 수용으로 남음. GPU 배포 수용(reload2442/0 동일 PID); LF 속성·packaged unit CR 거절 관문1445ee3 완료. N6r·N7 각 브랜치 보류 |
| R17 | 완료·v0.6.0 설치/공개 수용: install.ps1 upgrade 연결2102/0·자동 시작 등록·PATH 추가. 최초 실제 설치 결함은 R17.1에서 보완 |
| R17.1 | 완료: ASCII/BOM 없는 설치·제거 스크립트, 잠긴 백업 정리 보류·finally 트레이 복구, 실제 클라이언트 탐지·wire 환경 일치. PS5.1/7 각28통과·레거시 백업 혼합 형식 재관문 각28통과·독립 검수 통과·read-only launch check0. 보고서 `.sandbox/reports/R17.1.md`; 공개 main 반영은 설계 세션 |
| R18 | 설치 수용: dev1e8a4be·2026-10-28 20:25 KST·takeover1505/0·일반 Codex/Claude 실행 확인. Part 1 Free 계정 배정 제외, Part 2 공급자 날짜 사실, Part 3 최신 요금제와 제거된 창 반영 수용. 원본 실패와 집중 보완의 입력별 증거는 .sandbox/reports/R18.md. N6r/N7은 별도 브랜치·미병합 유지 |
| R19 | 설치 수용: dev3788c36·2026-10-29 04:16 KST·takeover845/0·Codex/Claude smoke·실제 출처/갱신 주체·계정 순서 확인. 구현·관문 완료: 갱신 주체 공통 표시·배정 제외 계정의 하단 44px 간결 행·접기 기억. 휠 선측정 최대30.373ms → 최종3.005ms(연속 입력3.694ms), 각72개 입력 누락0; 핸들러→제출 측정이며 물리 화면 지연 아님. tray902·allfeature4151·단독Core1476·집중bin15·Linux strict·실제 core/AdminClient 한영6PNG 검수 완료. 원본 실패와 집중 보완·UIA null 제한 보존. 보고서 `.sandbox/reports/R19.md`. N6r/N7 각 브랜치·미병합 유지 |
| R15.UI | 완료: 기존64 DIP 시작 열에 짧은 날짜·전체 시각 tooltip·활동 열8 DIP 간격. 한영·grouped/flat·6개 폭 실제 글꼴 회귀·기본610/전체903·strict lint·실제 core/AdminClient 한영2PNG 통과. 보고서 `.sandbox/reports/R15.UI.md`; 설치는 설계 세션. 다음 impl/n6r-tape Unit1; 구 r14-replay 중단·보존 |
| N6r-T Unit1 | 완료: insufficient: 합성 native 결정10·관측 이동1·loss0. 실제 producer/reference와 별도 CLI candidate 모두 입력10개 재생 거절, identity/power 미통과·재생 이동수 null. impl/n6r-tape WIP 보존·dev 병합 없음. 실제 native read-input 경계 미완성; valid verdict로 설계 세션 수용. 다음은 N7-L 설치 관문 뒤 채택된 raw-fact kernel·permit slice1; selection slice2. 보고서 `.sandbox/reports/N6r-T.Unit1.md` |
| N6r-T slice1 | 진행 중: 실제permit real2 identity445/445 수용·기존 guard power insufficient_not_exercised 보존. slice2/schema8 새 실제1h 선택·permit638/638 identity PASS, model2/640(0.3125%)·loss0·InitialWitness 존재; shared raw-fact fault14 정확 검출 및 margin0 PASS. 실제no30guard는637동일계정+1출발없음으로 insufficient_not_exercised. 40MB/h(config override)·512MiB 기본값 양OS 집중관문 완료; v8 독립디스크·과거루트 봉인후 archive/remove 운영 인계. >=24h 수용 별도·다중epoch6h 봉인 권고. 이전schema7 budget3270 insufficient·역사10-22 입력부족 보존. 보고서 `.sandbox/reports/N6r-T.tape8-real1.md` 7b3f9ce11:02KST 설치 수용(takeover1024/0·CodexOK), live512MiB·e02 11:02:48 시작; 이전루트154.8MB 검증아카이브 후 제거, 설계 세션6h간격4회봉인 예약(e02..e05). 캡처 중 추가설치 금지·24h 봉인export 대기. |
| R18-window-hold | 완료·설치 수용(f40bd53,00:05 KST,takeover1995/0,codex-qb OK): 제거된 창에 남은 실제429 quota hold를 원인 경로와 무관하게 해제, 새 헤더·현재 거절·더 강한 health·사용자 pause 보존. 의도된 native RED와 집중51/strict, 원본 기본3868/1의25행번호 보완·guard9 및 allfeature4279/0 보존. dev2ed0971 결합 후 core1604/0·필수8회귀·Windows기본/전체check·일반CLI build, private fixture12/0·Linux22/0/check/제품기본·전체strict 통과. report `.sandbox/reports/R18-window-hold.md`; 실계정·설치는 설계 세션 |
| R13.CLI | 완료·설치 인계: urgent 절대 wait/move의409 인계 거절에만 최대4회/총10초 제한 재시도·확정409 뒤 connect-only 공백 처리, 다른 변경/모호한 send 재시도 없음. core HTTP상태·정제된 사유 표시. 실제core HTTP1·플랫폼6·양기능 strict, Windows CLI671/0/3제외·677/0/4제외, Linux15+15+1+6/check/strict·청소 완료. 원본fixture실패 보존·독립 source PASS; report `.sandbox/reports/R13.CLI.md` |
| R13.Launch | merged: ordinary takeover now carries validated managed bindings and existing digest grants over bounded authenticated memory-only handover; actual old-URL/no-C8 POST and WS pass between patched cores. Legacy bootstrap/relaunch and managed Linux exec reload limits remain explicit. No install before10-31 finalseal; this ships after the schema9 producer install. Report `.sandbox/reports/R13.Launch.md` |
| R20 | 완료·dev 인계: Windows 실제리스 확인 후 PID/nonce 단일 socket export·ready 전 acceptor 보존·미전송 본문 인계. cold-takeover 회귀2개를 실제 red0/2→수정 후 전체 takeover10/0으로 복구. 최종 새200회 인계413.37초·TCP41213/HTTP14285 모두 성공·timeout/non2xx0·수명201/201. 기본3983/3 원본과 해당3실패 집중복구 보존; 전체기능4398/0/38제외·단독Core1703/0/2제외·strict2·일반build/read-only check0·Linux2+6/check/strict3 및 Windows전용 후속의 입력별 재사용 통과. Linux 사용자 문서와 수명 내부 문서 분리, architecture14 갱신. 최초 이전버전404 fallback·자연 core1초 timeout 미재현/별도 OS인과control 한계 보존. N6r24h 캡처 중 설치 보류, 보고서 `.sandbox/reports/R20.md` |
| N6r-T capacity producer | 진행 중·설치 인계: schema9 raw 측정readset/동일opaque계정links·키drop후replay3·percent정책 exact dev·가중정렬OFF, UI/CLI unchanged. 측정15/nativeClaude1/reader14/static39·Windows 원행렬 전체4420/1/38제외·기본4010/1/20제외의 같은 writer fixture 실패 보존/보정가족5양구성 통과, Core1726/0/2제외·strict2·Linux15+1+6/check/strict3·일반build/check 통과. R20포함dev병합 후10-31 11:04seal뒤설치; schema8원형24h판정이 먼저. 보고서 `.sandbox/reports/N6r-T.capacity-producer.md` |
| R21 | 진행 중·impl/r21 1ad42fa 보존/미병합: measured token-room 후보 코드/합성관문 수용자료완료, 역사export/schema8 readset부재로 weighted 실제판정 insufficient. owner가 producer-first 승인; schema9 warm+>=24h 뒤 percent대weighted paired margin0이 첫 실제후보판정. |
| N7-L | 진행 중: 복원력 c84076a 10-29 13:49 설치 수용·수집 on. 첫 sample origins2/sequence2/loss0, last_sample 60초 갱신·stoppedfalse·unavailable0·journal1분3.7KB 확인. 7일 시계 2026-10-29 13:49 시작; 최초 N7 채점 2026-11-05 13:49 KST. Claude chart 2/2·기타2 제외 확인. takeover1034/1의 드문 listener timeout은 후속 유지. 보고서 `.sandbox/reports/N7-L.resilience.md`; 모델 작업은 7일 후 |
| R11.1 | merged: 공유 AGENTS/hook 편집 보존·rh 별칭, 회귀5·cutover141·strict lint·실제 preview/launch 점검 통과. 설치·apply는 설계 세션. 보고서 `.sandbox/reports/R11.1.md` |
| R11 | merged (실제 native5/5·HTTP33·WS15·코어1213·복구54·창4, focused repair gate 통과). 0106c0e 설치·live 계정 지정 확인. 보고서 `.sandbox/reports/R11.md` |
| R8.1 | merged (round 2 39345fd, dev 6dbbfae); installed and verified live: the herdr pane title shows `codex-qb · quota-baton` from 1 s to 19 s, and a codex-qb exec request returned ok |

## Hard boundaries (never)

- No real credentials, live configs (CPA, `~/.codex`, `~/.claude`, herdr shims), real upstream calls in tests, or listeners on 10100/10101.
- No browser logins. Live checks (G1/V*) belong to the design session.
- No client impersonation crates or code (D3).
- Do not restart a Claude session that the user stopped.

## Design/review session resumed (2026-10-20)

The user lifted the pause. The design session reviews the merges made during the pause (R5, R3.2, R6, R7, credits logic, docs/tray-v2.md) and the unmerged `impl/cache-routing`. `impl/cache-routing` is merged (R8). R2 Part B was completed by the user's manual adoption procedure on 2026-10-29 (0e29393); the former hold is closed.

