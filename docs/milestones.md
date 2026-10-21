# Milestones: unit and gate checks

This file names the verification for each unit and each row. The row list and the order live in `GOAL.md`; this file does not repeat them. Commands run in `C:/_projects/SampleUser/quota-baton-impl`, one after another, in the default target with `CARGO_INCREMENTAL=0`.

## Status (single source for every report head)

<!-- status:begin -->
현재 마일스톤: R11 (실행별 계정·모델 목록 마무리)
목표: R11의 모델 배정과 실행 검증을 마무리한다
핵심 지표: 새 경로 출시 1/2 · 전체 기능 코어1213·CLI29 통과; 복구50/54 보완 중
사용자 대기: 없음 (R2 Part B: CPA 종료·복원 작업만 보류)
갱신: 2026-10-22 08:22 KST (병렬 import 기한 초과4건 단계 추적; 창·native 독립 검증)
<!-- status:end -->

The Codex main updates this block at each unit and gate close; the design session edits it when priority or approval changes. `.sandbox/scripts/status_head.py` copies it verbatim into the head of `.sandbox/reports/RESUME.md` and the current row's report. The Codex main runs that script at every unit close; nobody hand-edits a report head.



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
| A2 | R11, launch-scoped account and per-account catalog (D22), plus per-account aliases. Integration preparation runs **now**, independent of the frozen R12 source, in `.sandbox/r11-prep/`: no shared target, no builds that touch the impl worktree. The integration lands right after A1 | prep: a background `codex-qb exec` worker started by the main (one completion signal). Integration: Codex main `<pane>` | brief `.sandbox/briefs/R11-launch-account.md` | gate passes, alternation release gate passes, self-merged |
| A2.5 | R12.10: dashboard lag (urgent, user). Frame rebuilt on every paint and every 500 ms tick, ~300 ms per frame, 60 % CPU idle | Codex main `<pane>` | brief `.sandbox/briefs/R12.10-dashboard-perf.md`; starts when the current R11 unit closes, before R13 | idle CPU < 2 %, interaction p95 < 16 ms, focused gate, self-merged; design session installs |
| A3 | R13: burn rate and projected exhaustion, dashboard icon (D24), cache-lifetime hold (D25) | Codex main `<pane>` | brief `.sandbox/briefs/R13-burn-icon-hold.md`; starts when R11 merges | gate passes, `launch-client --check` passes, self-merged; design session installs |
| B | Probe: does a native `-c model_provider=qb resume` keep provider `qb`? | Codex main runs it as a **background** job during A1/A2 | the synthetic temp-home script from the D23 amendment, started in the background; one completion signal (exit code + result json) | result json written; the design session rules on resume into Qi |
| C | Install, live image generation (TUI and `codex-qb --images exec`), then **broadcast the exact command to every image-generating session**: sample-proj-a `<pane>`, sample-prj-opus `<pane>`, and any other session found running image work | design session `<pane>` | when A1 merges: build a release in a temp worktree, pre-check, install, and run one `codex-qb --images` generation | the image is produced through qb and sample-proj-a is told |
| D | Full gates and independent reviews | whoever starts them | always in the background with one completion signal, checked once at unit close | — |
| Held | R2 Part B (retire-cpa) | user decision (D9) | — | — |
| Backlog | further dashboard and tray polish; launcher move of a session whose ancestor chain lives only in the source home (refused today as "조상 전용 대화"; done by hand with `.sandbox/codex-migrate/move_one.py` for inside main on 10-22). Needs a design ruling on blocking the ancestor's resume paths (D21 panel 2) | — | not before A1 and A2 ship | — |

**No polling.** Nobody polls a running job or a pane in a loop. Every wait has the next ready node from this table.

## Fingerprint and reuse (dependency-aware)

- A fingerprint is keyed to the **inputs of that check**, not to the commit. For executable checks, the inputs are the touched crates' sources, `Cargo.lock` and the toolchain. For doc checks, they are the docs.
  - A docs-only change never invalidates a passed executable check.
  - A change to crate X invalidates only checks that depend on X.
- While a row is open, repair failing tests with **focused** re-runs of those tests and of their crate only.
- Run the **full matrix once**, as the final gate of the row. A failure outside the row's scope that also occurs on dev is recorded as pre-existing and fixed in its own unit; it does not block the row.
- Record each result once in `.sandbox/reports/<M>-evidence/`: inputs hash, command, exit code, scope and totals.

## Completion

The goal stays open while any row in `GOAL.md` is open. Before marking it complete, compare the open rows there with this file. Completion follows the design session's gate judgment, and a row that needs installation closes when the design session has installed and verified it.

## 진전 장부 (source of the "진전 한눈에" block)

Report standard 2026-10-22 (`C:/_projects/research/methods/reporting/devlog-and-interim-report.md` §3 item 1-1). `.sandbox/scripts/status_head.py` renders these tables, plus the done / in progress / remaining count from the `GOAL.md` milestone table, right under every report head. Append a row at every measurement, flat ones included; never edit old rows. When a metric's definition or denominator changes, start a new 계열 name.

<!-- metric:begin -->
| 측정 시각 (KST) | 계열 | 값 | 비고 |
|---|---|---|---|
| 2026-10-21 22:15 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 0/2 | 지표 도입 |
| 2026-10-21 22:47 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12 설치, codex-qb로 실제 이미지 생성 성공 |
| 2026-10-21 23:14 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12.1 설치, 변화 없음 |
| 2026-10-22 01:02 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12.9 설치, 변화 없음 |
| 2026-10-22 02:27 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12.10 계측 단위, 출시 수 변화 없음 |
| 2026-10-22 02:42 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12.10 텍스트 캐시 단위 2a6f6db, 출시 수 변화 없음 |
| 2026-10-22 03:02 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12.10 실제 HWND 캐시 회귀 통과; 성능 관문 미완료 |
| 2026-10-22 04:36 | 1/2 | 브러시 수명 단위 저장, 출시 수 변화 없음; 실제 HWND 회귀 통과, 성능 수용 미완료 |
| 2026-10-22 04:50 | 1/2 | R12.10 성능 단위 통과, 최종 관문 미완료; 기능 출시 수 변화 없음 |
| 2026-10-22 04:53 | 1/2 | R12.10 성능 통과, 최종 실행 검사는 OS bind 재현으로 미완료; R11 독립 통합 진행 |
| 2026-10-22 04:59 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R12.10 성능·scoped 관문 완료; 기능 출시 수 변화 없음 |
| 2026-10-22 05:08 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 WS10/10 단위 완료; native·최종 관문 미완료 |
| 2026-10-22 05:28 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 HTTP21/21·WS10/10 단위 통과; 독립 검수5건 수정 중, 미출시 |
| 2026-10-22 05:38 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 추가 독립 검수 반영 중; 하네스 자체검사 14/14, 제품 실행 결과 아님 |
| 2026-10-22 05:48 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 검수 보완 HTTP30 실행: 28통과·2 fixture 시각 비교 실패; 미출시 |
| 2026-10-22 05:56 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 보완 HTTP32/32·실제 WS15/15 통과, native·최종 관문 미완료 |
| 2026-10-22 06:01 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 수정 전 반례7/7 실패·소스 복원 확인; 최종 관문 미완료 |
| 2026-10-22 06:09 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 strict clippy·제품 build 통과; native 설정 순서 fixture 보완, 미출시 |
| 2026-10-22 06:20 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 두 번째 native 실패0/5와 workspace lint4건 수정 기록; 미출시 |
| 2026-10-22 06:24 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 native 하네스20/20 자체검사; 제품 native 수용과 별도 |
| 2026-10-22 06:45 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 workspace default2127통과·14실패·17제외; 보완 중, 미출시 |
| 2026-10-22 06:59 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 takeover9/9·reset2/2; launcher경계·native 미완료 |
| 2026-10-22 07:07 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | 실행·세션 fixture20/20, 명시적 제외3; 독립 정적 검수 통과, R11 미출시 |
| 2026-10-22 07:49 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | 모델 배정 표시 수정 전1실패→수정 후1통과; 전체 기능 실패 fixture 보완 중, 미출시 |
| 2026-10-22 07:59 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | 단독 코어705·CLI29 통과; 복구 fixture33실패 원인 보완 중, 미출시 |
| 2026-10-22 08:22 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | 전체 기능 코어1213·복구50/54; 병렬 지연4건 미해결, 미출시 |
<!-- metric:end -->

<!-- support:begin -->
| 측정 시각 (KST) | 보조 지표 | 값 | 목표 | 비고 |
|---|---|---|---|---|
| 2026-10-22 01시대 (분 미기록) | 대시보드 유휴 CPU (코어 하나 기준, 창 열림) | 60.7 % | 2 % 미만 | 최소화 시 1.4 %; R12.10 전 측정 |
| 2026-10-22 02:27 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 40.65 % | 2 % 미만 | 변경 전 30초 구간 완료; 이후 동작 구간은 도구 종료 오류로 미완료 |
| 2026-10-22 02:27 | R12.10 합성 HWND 유휴 재작성 프레임 p95 | 936.74 ms | 전체 재작성 50 ms 미만 | 변경 전 유휴 구간; 조작 p95 아님 |
| 2026-10-22 02:39 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 29.75 % | 2 % 미만 | 변경 전 재측정; 첫 hover 상관 검사 실패로 전체 실행 실패 |
| 2026-10-22 02:42 | R12.10 DirectWrite 텍스트·캐시 focused 테스트 | 10/10 통과 | 10/10 통과 | release; HWND 성능 수용 판정과 별개 |
| 2026-10-22 03:02 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 31.18 % | 2 % 미만 | 변경 전 before-final; hover 17회 뒤 외부 입력 혼입으로 실패 |
| 2026-10-22 03:02 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.2083 % | 2 % 미만 | after-sync-v3; 30초 완료, 전체 실행은 조작 시간 초과로 실패 |
| 2026-10-22 03:02 | R12.10 동기 입력→production paint v3 프레임 p95 (hover/scroll/tab) | 89.20 / 31.52 / 223.98 ms | 각 16 ms 미만 | 100회씩; 모두 보존, 실패; OS 입력 큐 지연은 측정하지 않음 |
| 2026-10-22 03:02 | R12.10 실제 HWND 캐시·UIA·자정 회귀 | 1/1 통과 | 1/1 통과 | release/testkit; 타이머 20회, hover 20회, focus 40회, 자정 시각 보정 |
| 2026-10-22 03:14 | R12.10 合成 HWND 同条件 v3 baseline 유휴 CPU | 40.57 % | 2 % 미만 | baseline 전체 완료; hover/scroll/tab 각100회, 수정 전 |
| 2026-10-22 03:14 | R12.10 同条件 v3 baseline 프레임 p95 (hover/scroll/tab) | 195.59 / 196.72 / 549.21 ms | 각 16 ms 미만 | 전체 실행 완료, 동일 30계정·40세션 fixture |
| 2026-10-22 03:14 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.1041 % | 2 % 미만 | after-repair-v3; 조작 구간은 탭 목표 미달로 실패 |
| 2026-10-22 03:14 | R12.10 동기 입력→production paint v3 프레임 p95 (hover/scroll/tab) | 1.389 / 7.210 / 71.627 ms | 각 16 ms 미만 | 각100회; hover와 scroll 통과, tab 실패; 누락·제외 없음 |
| 2026-10-22 03:14 | R12.10 스크롤 동등성·텍스트·그리기 생략 focused 테스트 | 20/20 통과 | 20/20 통과 | release: region5, text11, paint4; a47b44c |
| 2026-10-22 03:14 | R12.10 표시 시각 deadline focused 테스트 | 9/9 통과 | 9/9 통과 | 동일 release actual-Host 실행 파일; DST·자정·숨긴 행 |
| 2026-10-22 03:27 | R12.10 동기 입력→production paint v3 프레임 p95 (hover/scroll/tab) | 4.235 / 17.196 / 47.699 ms | 각 16 ms 미만 | after-tab-metric-v3; 모든100회 보존; hover만 통과, full 재작성50ms 기준 통과 |
| 2026-10-22 03:27 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.0000 % | 2 % 미만 | 30초 CPU tick 증가0; 0보다 작은 비용이라는 뜻 아님 |
| 2026-10-22 03:27 | R12.10 전체 배치 metric 캐시 focused 테스트 | 3/3 통과 | 3/3 통과 | 867cec1; warm 1회 배치 및 fresh 장면 동등성·입력 변화·경계 |
| 2026-10-22 03:27 | R12.10 실제 HWND 검수 회귀·UIA 조상 깊이 | 3/3 통과 | 3/3 통과 | Host1 (pointer release4경로·역시각·자정 포함), depth2; 새 탭 캐시 통합 전 |
| 2026-10-22 03:38 | R12.10 동기 production paint v4 프레임 p95 (hover/scroll/tab) | 9.900 / 15.527 / 12.686 ms | 각 16 ms 미만 | 각100회 전부 보존; 바깥 입력→paint scroll p95 16.668 ms로 미달 |
| 2026-10-22 03:38 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.208297 % | 2 % 미만 | after-tab-cache-v4; 전체 관문은 미완료 |
| 2026-10-22 03:38 | R12.10 새 snapshot 전체 재작성 v4 (30회, 탭별10회) p95 | 133.578 ms | 50 ms 미만 | 새 계열; 지역 재작성·탭 캐시 교체 제외; median24.996 ms, 실패 |
| 2026-10-22 03:46 | R12.10 동기 production paint v4 프레임 p95 (hover/scroll/tab) | 1.620 / 8.972 / 10.850 ms | 각16 ms 미만 | after-scroll-cpu-v4; 각100회 보존; 바깥 입력 p95 2.138/9.138/12.353 ms |
| 2026-10-22 03:46 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.052074 % | 2 % 미만 | 30초 완료; 전체 재작성 미달로 실행 실패 |
| 2026-10-22 03:46 | R12.10 새 snapshot 전체 재작성 v4 (30회, 탭별10회) p95 | 56.833 ms | 50 ms 미만 | CPU tick p95 62.5 ms(15.625 ms 단위); 전체 재작성 비용 절감 계속 |
| 2026-10-22 03:46 | R12.10 실제 HWND 탭 캐시·prepared UIA·부분 그리기 동등성 | 8/8 통과 | 8/8 통과 | Host1, prepared3, WIC픽셀4; depth 필터 오기로0회는 미검증으로 제외 |
| 2026-10-22 03:54 | R12.10 동기 production paint v4 프레임 p95 (hover/scroll/tab) | 8.289 / 6.896 / 9.305 ms | 각16 ms 미만 | after-full-repair-v4; 각100회; 입력→paint p95 8.389/7.352/12.290 ms |
| 2026-10-22 03:54 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.156218 % | 2 % 미만 | 30초 구간, 성능 수용 조건 전체 통과 |
| 2026-10-22 03:54 | R12.10 새 snapshot 전체 재작성 v4 (30회, 탭별10회) p95 | 42.431 ms | 50 ms 미만 | median19.785, max51.542 ms; 모든30회 보존 |
| 2026-10-22 03:54 | R12.10 비용 절감 후 release 회귀 | 라이브러리211, Host순수53, 실제창1 통과 | 모두 통과 | 추가 실제창 editor/tooltip fallback 통과; 최종 scoped gate 진행 |
| 2026-10-22 04:07 | R12.10 동기 production paint v4 최종 프레임 p95 (hover/scroll/tab) | 1.892 / 24.246 / 31.434 ms | 각16 ms 미만 | final-v4 실패, 각100회 전부 보존; 그리기 단계 지연 |
| 2026-10-22 04:07 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.208224 % | 2 % 미만 | 30초 완료; 전체 성능 수용은 실패 |
| 2026-10-22 04:07 | R12.10 새 snapshot 전체 재작성 v4 (30회, 탭별10회) p95 | 63.837 ms | 50 ms 미만 | final-v4 실패, 이전42.431 ms 통과만으로 병합하지 않음 |
| 2026-10-22 04:07 | R12.10 qb-tray focused 관문 | 527통과 / 0실패 / 17명시제외 | fmt·strict clippy·tests·build 통과 | e764773; 이후 브러시 재사용 변경에는 재검증 필요 |
| 2026-10-22 04:07 | R12.10 원본 v4 비교 재측정 | 첫 탭 입력 상관 검사 실패 | 같은 조건의 완전한 실행 | 미완료 수치 제외; v5는 탭 focus 사전조건 명시, 기준값 불변 |
| 2026-10-22 04:22 | R12.10 동기 production paint v5 브러시 재사용 프레임 p95 (hover/scroll/tab) | 3.266 / 64.389 / 19.084 ms | 각16 ms 미만 | after-brush-v5 실패, focus 사전조건 명시; 각100회 보존 |
| 2026-10-22 04:22 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.052083 % | 2 % 미만 | 30초, 전체 성능 수용 실패 |
| 2026-10-22 04:22 | R12.10 새 snapshot 전체 재작성 v5 (30회, 탭별10회) p95 | 68.772 ms | 50 ms 미만 | 브러시 재사용만으로 목표 충족하지 못함 |
| 2026-10-22 04:22 | R12.10 브러시 픽셀·색·대상·해제 release 라이브러리 검사 | 215통과 / 0실패 | 실제 WIC 픽셀·경계 검사 | 대상 자원 해제 추가; HWND 바이너리 가시성 오류는 수정 후 별도 확인 |
| 2026-10-22 04:36 | R12.10 줄바꿈 재사용 포함 release 라이브러리 | 220통과 / 0실패 | cached/uncached·범위·한글·대상 해제 검사 | prepared-text-stage/library.log; 실제 성능 수용과 별도 |
| 2026-10-22 04:36 | R12.10 대상 해제 수정 후 실제 HWND 캐시 회귀 | 1통과 / 0실패 | 실제 창 동작·UIA·자정 회귀 | host-brush-clear-fixed; 최초 가시성 오류 수정 뒤 확인 |
| 2026-10-22 04:50 | R12.10 동기 production paint v5 준비 텍스트 프레임 p95 (hover/scroll/tab) | 2.190 / 12.146 / 9.642 ms | 각16 ms 미만 | prepared-text-v5 입력 고정 측정 완료; 소스82ac1a71, 이후 clippy 스타일 수정 |
| 2026-10-22 04:50 | R12.10 합성 HWND 유휴 CPU (30계정·40세션, 코어 하나 기준) | 0.260350 % | 2 % 미만 | after-prepared-text-v5 30초 완료 |
| 2026-10-22 04:50 | R12.10 새 snapshot 전체 재작성 v5 준비 텍스트 (30회) p95 | 46.527 ms | 50 ms 미만 | after-prepared-text-v5 통과, 최대80.964ms 포함 모두 보존 |
| 2026-10-22 04:50 | R12.10 원본 v5 비교 | hover100·scroll100 완료, tab19회 뒤 focus 확보 실패 | 전체 실행 | 완전한 baseline에서 제외, 실패 원인 단정 안 함 |
| 2026-10-22 04:50 | R12.10 UIA 색인 비교 검사 | 2통과 / 0실패 | 기존 대상 선택과 동일 | release actual-Host 포함 타깃; native Host 별도1통과 |
| 2026-10-22 04:50 | R12.10 준비 텍스트 관문 시도 | fmt통과, clippy 조건문 스타일1건 실패 | 전 검사 통과 | 77df5ef에서 수정; 최종 관문 다시 실행 |
| 2026-10-22 04:53 | R12.10 최종 strict clippy | 통과 (exit0) | qb-tray all-targets testkit -D warnings | 스타일 수정77df5ef 이후; native 테스트 별도 |
| 2026-10-22 04:53 | R12.10 최종 qb-tray 테스트 시도 | 기존 HTTP 검사6건이 kernel bind에서 정지, 소유 실행 취소 | 전체 완료 | 제품 assertion 실패로 단정하지 않음; 테스트 통과로 세지 않음 |
| 2026-10-22 04:53 | 외부 조건 독립 Python IPv4 bind8초 | timeout | 숫자 loopback 새 bind가 반환 | qb import 없음; socket().bind(127.0.0.1:0), 프로세스 제한 시간8초 |
| 2026-10-22 04:59 | R12.10 OS 복구 후 최종 scoped 관문 | HTTP6/6 한 번씩 재통과; fmt·clippy·build exit0 | 감독 판정에 따른 종결 | interrupted aggregate 및222 library pass 보존; 완전한 전체 합계 미주장 |
| 2026-10-22 05:08 | R11 실제 WS 업그레이드 회귀 | 10통과 / 0실패 | 10/10 | HTTP 채택 redirect 수정; 다른 계정 cap을 만들던 fixture 전제 교정, 이전9/10 보존 |
| 2026-10-22 05:12 | R11 qb-core strict all-targets testkit clippy | exit0 | 경고0 | 최초 통합 스타일5건·fixture 초기화1건 수정; 실패 기록과 최종 입력별 fingerprint 보존 |
| 2026-10-22 05:28 | R11 재구성·동시 resume 포함 실제 HTTP 회귀 | 21통과 / 0실패 | 전 검사 통과 | eleventh-http-build, 이후 독립 검수 F1–F5는 별도 회귀·수정 중 |
| 2026-10-22 05:28 | R11 최신 qb-core clippy·제품 testkit build | 각 exit0 | 경고0·빌드 통과 | 입력2f6e0a0b…; 후속 F1–F5 입력으로 최종 통과 주장 안 함 |
| 2026-10-22 05:38 | R11 native 하네스 순수 자체검사 | 14/14 통과 | 14/14 | 실제 native·제품 실행 없이 오판 방지 규칙 재검증 |
| 2026-10-22 05:48 | R11 검수 보완 HTTP 회귀 | 28/30 통과 | 30/30 | 고정 입력7efebecd; 실패2건은 launch_anchors.last_used 비교, 수정·재실행 예정 |
| 2026-10-22 05:56 | R11 검수 보완 HTTP/실제 WS 회귀 | 32/32 · 15/15 통과 | 모두 통과 | 고정 입력dda712bf; F1–F5·추가 경합·재개 모델·동의·프로브·legacy 포함 |
| 2026-10-22 06:01 | R11 수정 전 반례 검출 | 7/7 예상 실패 확인 | 7/7 | F1/F3/F5·F4 초기 후보 반례, F2는 로컬 오류 대기 timeout; 현재 소스 지문 복원 |
| 2026-10-22 06:09 | R11 strict core clippy / testkit 제품 build | exit0 / exit0 | 모두 exit0 | callback 타입 복잡도1건은 type alias로 수정; 입력9d09e0bb |
| 2026-10-22 06:09 | R11 첫 native 설정 시도 | 0/5 완료 | 5/5 완료 | wire-before-serve 하네스 오류로6.8초 종료; job 정지·잔류 home0 확인 |
| 2026-10-22 06:14 (초기 결과 소급 기록) | R11 최초 strict clippy | exit101 · 타입 복잡도1건 | exit0 | 행동 변경 없이56e0fde alias 수정 후35.8초 exit0; 실패 로그 보존 |
| 2026-10-22 06:20 | R11 두 번째 실제 native | 0/5 완료 | 5/5 완료 | A picker·Daybreak 표시 성공, 첫 WS 추론 실패; cleanup 정지true·잔류0 |
| 2026-10-22 06:20 | R11 workspace default strict clippy | 최초exit101 → 수정 뒤exit0 | exit0 | 스타일4건5dc8a2f; 이전 실패 지문 보존, all-features·tests 진행 |
| 2026-10-22 06:24 | R11 합성 WS 하네스 순수 검사 | 20/20 통과 | 전 검사 통과 | 실제 parser/handler/finish, 프로세스·네트워크 금지; 제품 실행 아님 |
| 2026-10-22 06:45 | R11 workspace default 전체 실행 | 2127통과 / 14실패 / 17제외 | 전 검사 통과 | 입력a5fdf36b, 1324.9초; m4b1·title1·R9 9·takeover3 실패 모두 보존 |
| 2026-10-22 06:59 | R11 reset-deadline·재요청 focused | 2/2 통과 | 2/2 | setup 뒤 실제남은시간·양수사전조건 유지 |
| 2026-10-22 06:59 | R11 실제 두 코어 takeover 집중 | 처음8/9 → 보완9/9 | 9/9 | catalog와SSE script 분리; 모든 strict·401·refresh·lease 조건 유지 |
| 2026-10-22 06:59 | R11 기존 launcher focused | title1통과·R9 15통과/3실패 | 모든 지원경로 통과 | UUID없는 선택의 DB·TTY 경계가 원인; 양성/경계 분리 후 재검증 |
| 2026-10-22 06:59 | R11 세 번째 native | 0/5 완료 | 5/5 | warmup·일반 image viewing 도구의 과도한 거부 진단; cleanup정지true·잔류0 |
| 2026-10-22 07:07 | R11 기존 launcher·세션 focused 최종 | 20통과 / 0실패 / 3제외 | 모든 지원경로 통과 | 입력7126328e, R9 b316c05e·title e971f584, 독립 정적 closeout PASS |
| 2026-10-22 07:07 | R11 native warmup·tool 하네스 순수 검사 | 27/27 통과 | 전 검사 통과 | frozen6381b346, 독립 정적 PASS; 실제 native 통과 아님 |
| 2026-10-22 07:49 | R11 전체 기능 workspace 최종 1회 | 2355통과 / 36실패 / 31제외 | 실패0 | CLI1·복구35; 기존 실패 보존, 집중 재검증 |
| 2026-10-22 07:49 | R11 실제 HTTP 모델 배정 표시 반례 | 수정 전0/1 → 수정 후1/1 | 원인 회귀 증명 | 초기 배정과 동의한 모델 전환, 실제 admin/detail/activity/inference |
| 2026-10-22 07:49 | R11 모델 배정·제목 집중 단위 | 코어591·트레이223·제목1 통과; fmt·기본 strict clippy exit0 | 관련 전 검사 통과 | actual HWND 캡처와 native 5경우는 아직 미완료 |
| 2026-10-22 07:59 | R11 단독 코어·CLI 집중 실행 | 코어705/705·CLI29/29; 복구21통과/33실패 | 실패0 | 기한 초과 재발 없음; app-server의 기존 명시적 거부32·신규 양성 fixture 스키마1 보완 |
| 2026-10-22 08:05 | R11 복구 실패 분류 정정 (07:59 행) | grammar27·스키마1·import기한초과5 | 실패0 | 이전 기한초과0 기록은 오류; 원본 로그·정적 분류 JSON 보존, 시간 제한 유지 |
| 2026-10-22 08:22 | R11 변경 코어 전체 기능 crate | 1213통과·0실패·4제외 | 실패0 | core-allfeatures-repair; 입력 aa13cf7d…, 981.7초 |
| 2026-10-22 08:22 | R11 복구 fixture 두 번째 focused 실행 | 개별3/3, 병렬50통과·4실패 | 병렬54/54 | 문법·스키마 보완 통과; 4건 모두 초기 import20초 기한 초과, 기한 유지 |
<!-- support:end -->

<!-- gaps:begin -->
| 남은 차이 | 지금 | 넘어야 할 값 | 다음 행동 |
|---|---|---|---|
| R11 실행별 계정 | HTTP33·WS15, 코어591·트레이223·기본 clippy 통과 | native·최종 관문 통과 | CLI29·standalone705·전체 기능 코어1213 통과; 병렬 복구4건 추적, 실제 창·Codex 5경우 검증 |
| 대시보드 지연 (R12.10) | 합성 실제 창 성능·최종 scoped gate 통과 | 실제 설치 확인 | 설계 세션이 설치하고 사용자 환경 확인 |
| R13 소진 예측·아이콘·차트·캐시 보류 | 설계 완료, 병합 0/3 단위 | 3/3 단위 병합과 설치 | R11 뒤 B+B2+D, A, C+E 순서 |
<!-- gaps:end -->
