# Milestones: unit and gate checks

This file names the verification for each unit and each row. The row list and the order live in `GOAL.md`; this file does not repeat them. Commands run in `C:/_projects/SampleUser/quota-baton-impl`, one after another, in the default target with `CARGO_INCREMENTAL=0`.

## Status (single source for every report head)

<!-- status:begin -->
현재 마일스톤: R13 N5a HTTP 대기·초기화 재개
목표: HTTP429 완료·저장소 근거72검사 통과; 실제 대기 허가·무효화 알림·초기화 재개 연결
핵심 지표: 남은 출시 1/3 (R14 보호 설치, R13 C+E 실행, R13 F 통합 예측); 이전 계열 2/2 종료
사용자 대기: R2 Part B(retire-cpa) 실행 여부 결정 — 남은 CPA 연결 계정을 qb가 직접 갱신하도록 넘기고 CPA를 멈출지. 선택: 지금 / 보류. 추천: 보류(CPA 연결 Claude 계정이 아직 CPA 갱신에 의존). 풀리는 것: R2 Part B 구현과 설치.
갱신: 2026-10-23 08:07 KST (N5a HTTP429 근거 단위 ce351c2·집중91통과·대조 검사 통과; 대기 실행 미완료)
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
9. **UI changes (D27, hard rule):** at least one capture through the real path (spawned core, real AdminClient, real tray process). FakeBridge captures alone do not pass.
10. **Independent review**, where the brief asks for one: a different model family from the implementer.
11. **Routing changes (D29):** replay at least 24 complete hours of real, body-free decision inputs through the candidate and the installed router. Follow the evidence rules below. Missing inputs or incomplete coverage do not pass. The explicitly requested R14 first delivery ships its runtime protection before this second-delivery replay gate; its report must retain that outstanding requirement.

### Routing replay evidence (D29)

The replay gate compares the production selection paths, not a replacement model
of the breaker. Record the exact source, configuration, toolchain and adapter
fingerprints for both routers, the input hash, the initial-state hash, and a
totally ordered interval of at least 24 hours. Document gaps, dropped events,
caps and warm-up. Include canonical session/alias state, routing policy, account
health/quota evidence, capacity/admission events and the injected decision time.
Persist only allowlisted metadata; never prompts, bodies or credentials.

Compare committed moves, returns within 30 minutes, and comparable estimated
re-read tokens. The default margin is zero for each counter. Any intentional
increase must be named and justified before running the comparison. Report
measured costs separately from estimates and show known/unknown counts for both
routers. A candidate cannot pass by dropping cost coverage.
Also report dispatch, hold and refusal deltas: silently refusing work must not
pass merely because it produces fewer moves. Identify intended guard holds
before comparison and keep their allowance separate from token-cost margins.

A recorded response from account A is not an observed response from a
counterfactual account B. Unsupported branch-dependent usage or outcomes remain
unknown. Missing initial state, decision inputs or comparable costs produces
**insufficient evidence**, never a pass or a zero-cost assumption. A move-only
ledger playback, synthetic fixture or manifest declaration alone does not prove
the real-trace gate. Record these narrower results separately.

Include the reported 2026-10-22 12:01 account-return sequence as a regression.
Preserve which facts were observed and which were supplied synthetically; an
enriched fixture is not a reconstruction of missing historical inputs. Run the
same adapter twice to verify determinism, and compare identical router revisions
as an identity control before accepting a candidate comparison.

## Plan (re-planned 2026-10-23 after the root false-dependency audit)

**Ordering rule.** A node is ready when the inputs listed for it exist. Only two shared resources are serialized:
- **S1, the implementation worktree:** one branch checked out and one cargo process at a time. The Codex main owns it.
- **S2, the live install:** one swap at a time. The design session owns it.

Nothing else orders work. Among ready nodes, the Codex main picks by importance:
1. user-blocking bugs;
2. money and safety, and nodes that start a long wait;
3. user requests;
4. small fixes;
5. structure.

Long waits and work that needs no build run as background lanes (W) with one completion signal. Minor items are not parallelized; usage limits apply. User-set safety and authorization rules are unchanged, and R2 Part B stays held.

| Node | Lane | Inputs (real data only) | Start | Pass signal |
|---|---|---|---|---|
| N1 R16.2 one-line forecasts and provider summary | S1 | none (in progress) | `impl/r16.2`, brief R16-readme-assets.md D24 amendments 4-5 | focused gate and real-path capture (D27) |
| N2 R16.3 provider charts and pace line | S1 | none | `impl/r16.3` from dev, brief section R16.3 | focused gate, real-path capture, regressions for differing resets and a paused account |
| N6 R14.2 recorder, minimum delivery | S1 | none | `impl/r14.2` (keep db6baac), brief R14 plus D29 amendment 4 | schema, off switch, default off, 7-day bound, tests; then S2 install and enable; the 24 h capture starts at install |
| N6t R14 typed rebind and static write check | S1 | none | `impl/r14-rebind` | compile-time choke point, static check failing on a planted write, existing tests |
| N6r R14 differential replay gate | S1 | N6 installed plus 24 h captured export | `impl/r14-replay` | candidate vs installed replay report on the real export, margin 0 |
| N5m R13 C measurement: Codex cache survival by idle gap; client hold tolerance against a fake upstream | W | none | background `codex-qb exec` worker in `.sandbox/r13-measure/`; no cargo in S1 | measurement table in the R13 report |
| N5a R13 E HTTP wait and reset-resume for non-urgent sessions at 5h limits | S1 | none (bound is a setting; N5m only tunes its default) | `impl/r13-wait` | hold then serve on the same account after reset; bound exceeded falls back; cancellation releases |
| N5b R13 E WS/SSE cancellation and keepalive during a hold | S1 | none | `impl/r13-ws-hold` | cancelled held WS/SSE releases its slot and binding; no orphaned upstream |
| N5c R13 E urgent policy (`on_5h_limit`, `--urgent`, CLI and dashboard toggle; 7d always moves) | S1 | none | `impl/r13-urgent` | policy tests, CLI tests, real-path capture |
| N5d R13 E native-client resume after reset (Codex `/goal resume` only for a live goal; Claude continue) | S1 | none | `impl/r13-native-resume` | synthetic native test, exactly-once resume |
| N5i R13 C+E integration and defaults | S1 | N5a-d merged, N5m table | `impl/r13-ce` | full gate; then S2 install |
| N3 R9.5 move progress, interruption safety, readable preview | S1 | none | `impl/r9.5`, brief R9.5-move-progress.md | golden ko/en output, Ctrl+C tests per step, finish/rollback after a kill |
| N4 R12.11 canary version timeout | S1 | none | `impl/r12.11` (proposal kept) | slow-version, timeout-to-cache and unparseable regressions |
| N7 Part F pooled runway: calibration and 24 h backtest | W, then S1 | none for the backtest; the UI after N1 | backtest worker in `.sandbox/r13f-backtest/` | backtest error at 1 h and 3 h against a stated bound |
| N8 R15 compiled admin contract | S1 | none | `impl/r15`, brief R15-contract.md | contract round trips, static guard, property tests |
| N9 README final and v0.2.0 release | S2 and design | N1 and N2 installed | design session | public export scan clean, release assets built from the export |
| Held | R2 Part B (retire-cpa) | user decision | — | — |

**Current S1 order** (importance; change it only when importance changes): N1, N2, N6, N5a, N5b, N5c, N5d, N3, N4, N6t, N5i (when its inputs exist), N7 UI, N8, N6r (when its inputs exist).

**Start now in W:** N5m and the N7 backtest.

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

Report standard 2026-10-22 (`C:/_projects/research/methods/reporting/devlog-and-interim-report.md` §3 item 1-1). `.sandbox/scripts/status_head.py` renders these tables, plus the done / in progress / remaining count from the `GOAL.md` milestone table, right under every report head. Metric rows are written only when the delivery state can change: a gate pass, a merge or an install (flat results included). Unit progress belongs in the report body, not here, and a unit whose only output is new records is not a unit (GOAL rule). Every metric row has all four cells; side measurements go to the support table. Never edit old rows. When a metric's definition or denominator changes, start a new 계열 name.

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
| 2026-10-22 08:34 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | 복구54/54 병렬4로 통과, native 미완료; 미출시 |
| 2026-10-22 08:49 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 native 제목 보조 요청 형식 확인; 첫 종료 확인 실패, 미출시 |
| 2026-10-22 09:01 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 1/2 | R11 관문 통과·self-merge; 실제 설치 전이므로 출시 수 유지 |
| 2026-10-22 09:21 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | 설계 세션 R11 0106c0e 설치·계정 지정 live 통과 확인 |
| 2026-10-22 09:38 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R11.1 focused 관문 완료, 실제 별칭 적용은 설계 세션 |
| 2026-10-22 09:51 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 B+B2+D 초기 회귀12·실제 HWND 아이콘1통과, 관문 미완료 |
| 2026-10-22 10:02 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 첫 배포 B+B2+D 관문 완료; 설치 전이므로 변화 없음 |
| 2026-10-22 10:23 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 A 초기 코어25·CLI1 통과, 고빈도 표본·차트 보완 중; 미출시 |
| 2026-10-22 10:46 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 A 회귀 보완·원자 커밋87ef2bc, 설치 수 변화 없음 |
| 2026-10-22 10:53 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 A 집중 관문 완료, 출시 수 변화 없음 |
| 2026-10-22 11:09 | 2/2 | D24 장기창 보완32·로컬 native 기본4건 통과; C+E 기능은 미출시 |
| 2026-10-22 11:37 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 C 실측·정책 기반 단위; C+E 미출시, 출시 수 변화 없음 |
| 2026-10-22 11:52 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13 C 대기·정책 기반9건 추가 검증; C+E 미출시, 출시 수 변화 없음 |
| 2026-10-22 12:17 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13.A1 HTTP 예측 누락 수정; 출시 수 변화 없음 |
| 2026-10-22 12:19 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R13.A1 집중 관문22 통과·코드2b83ed3; 출시 수 변화 없음 |
| 2026-10-22 12:41 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | A1 설치 수용; C+E 기반 회귀74+3+3·예산8·Claude인자11 통과, 실행 통합 미출시 |
| 2026-10-22 13:13 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | C+E 긴급 등록 단위 커밋; 전체 wait/move 실행은 미출시, R14 보호 장치 시작 |
| 2026-10-22 13:57 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 보호17·집계20·원장9 통과; 실제 HTTP1통과·1실패 수정 중, 미출시 |
| 2026-10-22 14:43 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 미출시; 스트림23·HTTP11·strict lint·새 HWND 통과, 전체 관문 대기 |
| 2026-10-22 15:30 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 Sol P1 소스 증거 누락 수정1eb26ce; 코드 동결, 실행 관문 미완료 |
| 2026-10-22 15:35 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | 공통 의존성 빌드 성공; R14 수정본 검증과 2차 재생 관문 기준 보완, 미출시 |
| 2026-10-22 15:48 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 오래된 관측 경합 보완 중; 재생 서비스 거절 우회 합성 회귀 추가, 미출시 |
| 2026-10-22 15:50 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 관측 순서 보완c5a8bec 커밋·실행 관문 시작; 미출시 |
| 2026-10-22 16:00 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 c5a8bec 집중34·기본/전체 기능 lint 통과; 전체 관문 진행, 미출시 |
| 2026-10-22 16:10 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 전체 기본 코어681통과·8실패 확인; 원인 독립 대조, 미출시 |
| 2026-10-22 16:19 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 기본 전체2292통과·13실패·17제외; 병합 alias 결함·fixture 보완 집중 실행, 미출시 |
| 2026-10-22 16:27 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 보완 집중693통과·원자 커밋4개; 최종 WS/matrix 진행, 미출시 |
| 2026-10-22 16:31 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 집중697통과·기본 strict lint 통과; 최종 matrix 진행, 미출시 |
| 2026-10-22 17:28 | 새 경로 출시 (분모 2: R12 이미지, R11 실행별 계정) | 2/2 | R14 1차 전체 관문·실제 차단 확인 완료; 설치 전이므로 출시 수 유지 |
| 2026-10-23 01:08 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 0/3 | 이전 새 경로 계열은 2/2로 종료. R14.1 보호 관문 통과·self-merge 전달; 설치 전이므로 0/3 |
| 2026-10-23 01:12 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | 설계 세션이 dev249a14c 설치 수용. takeover 1,717회 중 연결 거절0, 일반·이미지 exec 통과, 실제 dashboard 재독 토큰 지표 확인 |
| 2026-10-23 02:21 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16 집중438통과·실제 core/AdminClient/tray 3HWND·20CLI PNG 통과, 코드19b8d22. 영어 UI는 이 계열 분모 밖이며 설치는 미실행 |
| 2026-10-23 02:52 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16 설치 수용006449a 및 R16.1 집중405·실제 경로 통과3737fe2. 영어 폭 보완은 분모 밖 |
| 2026-10-23 03:48 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R9.4 압축 세션 집중 관문 통과: 압축38·신규7·DB31·재개14(중복 포함), strict lint·빌드 통과. 분모 밖 복구이며 설치는 설계 세션 |
| 2026-10-23 04:26 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.2 집중428·실제8HWND 수용, 분모 밖 표시 개선; 실제 설치 전 |
| 2026-10-23 04:34 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.2 설치 수용 ddf8d87; takeover 거절0/1,959·실제 dashboard 확인. 이 표시 개선은 분모 밖 |
| 2026-10-23 05:19 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.3 집중464·실제10HWND 수용·설치 인계; 제공자 차트는 분모 밖. 다음 N6 |
| 2026-10-23 05:26 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.3 설치 수용41cb129·takeover 거절0/1,889; 잔여량 집계 R16.4 보완 |
| 2026-10-23 06:00 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.4 집중 관문·실제 경로 검수 완료; 분모 밖 표시 수정, 다음 N6 |
| 2026-10-23 06:12 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | N6 최소 기록기 관문 완료·설치 인계; 분모 밖 준비 기능, 다음 N5a |
| 2026-10-23 06:31 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | N6 최소 기록기 설치·128 MiB로 활성화; 실제24시간 입력은 10/24 06:31 이후 설계 세션 export |
| 2026-10-23 06:52 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.5 잔여량 이력 관문 완료·설치 인계; 출시 분모 밖 표시 보완 |
| 2026-10-23 06:58 | 남은 출시 (분모 3: R14 보호 설치, R13 C+E 대기·이동·재개 실행, R13 F 제공자 통합 예측) | 1/3 | R16.5 설치 수용; recorder e01 단일 epoch 수집10/23 06:58~10/24 06:58, 다음 설치는 수집 뒤 |
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
| 2026-10-22 08:24 | R11 실제 HWND 변경 상태 | 4/4 캡처·직접 시각 검수 통과 | 4/4 | detail/activity × light/dark; 실제1320×960 HDC, 별도WIC는 미집계 |
| 2026-10-22 08:24 | R11 전체 기능 lint·빌드·읽기 전용 launch 점검 | 모두 exit0 | exit0 | independent-final; 실제journal 쓰기 없음 |
| 2026-10-22 08:24 | R11 native 네 번째 시도 | 0/5, 첫 case 중단 | 5/5 | 계정 제목 확인; typed Daybreak request access missing, cleanup stopped·retained없음 |
| 2026-10-22 08:34 | R11 원본 소스 복구 전체 target (동시4) | 54통과·0실패 | 54/54 | 20초 기한·assertions 유지; 입력 d6737f49…, 고병렬 기한초과는 별도 보존 |
| 2026-10-22 08:34 | R11 임시 단계 추적 실행 | 첫54/54, 둘째53/54 | 원인 구간 확인 | 둘째 실패는 preview 약9초·targetrows17초 후 종료; 단일 정체 증거 없음, 소스 복원 확인 |
| 2026-10-22 08:49 | R11 native 5·6·7·8차 | 1/5·1/5·1/5·1/5 완료 뒤 실패 | 5/5·종료·보안 검사 통과 | 5·6차 차단 CONNECT 분류, 7차 제목 schema, 8차 종료EOF 확인 실패; 모두 잔류home0 |
| 2026-10-22 08:49 | R11 native 하네스 순수 경계 | 26+10 통과 | 전 검사 통과 | 관측한 title min1/max36 구분; 모든 CONNECT403, 실제 native 수용과 별도 |
| 2026-10-22 09:01 | R11 native 종료 진단 | A1 종료0/reader완료, B 종료0/reader미완료 | 프로세스 종료와 snapshot 정리 확인 | ConPTY EOF 지연 확인; 같은6초 OS process handle 대기로 보완, pure16/16 |
| 2026-10-22 09:01 | R11 native 9차 | 5/5 통과 | 5/5 | 제목·typed Daybreak·C8·strict이동·종료0·정리 통과, native 자체 디스크 캐시 생성 없음 |
| 2026-10-22 09:01 | R11 외부 캐시 잔존 첫 실행 | 3/5 후 concurrentA 목록 불일치 | 5/5 | 오래된 관측과 배치mark 영향 가능, 제품 원인 미확정; 원본 보존 |
| 2026-10-22 09:01 | R11 실제 사용량 조회 추가 첫 실행 | cold시작0/5, admin거부 | 5/5 | HTTP 상태 미보존으로 원인 미확정; cold 중복조회 제외·다음 응답상태 보존 |
| 2026-10-22 09:01 | R11 외부 캐시 잔존 최종 native | 5/5 통과·제목5·종료0·snapshot5 | 5/5 | 실제 admin조회200 ×4, seeded 외부캐시; native 캐시 hit 주장은 안 함 |
| 2026-10-22 09:01 | R11 최종 기본 workspace strict clippy | exit0 | exit0 | 최신 고정코드, 명령은workspace; runner기록digest범위core는 전체코드별도확인으로 보완 |
| 2026-10-22 09:01 | R11 최종 하네스 순수·정적 경계 | 26+10+16 통과, 독립 정적 closeout | 전 검사 통과 | runtime5/5와 별도; source hash62eacf, 모든CONNECT403 |
| 2026-10-22 09:21 | R11.1 실제 wire 미리보기 | 수정 전 0/2, 수정 후 2/2 | 2/2 | plain·dedicated, rh 계획 포함, 실제 apply 없음·관측 파일 불변 |
| 2026-10-22 09:28 | R11.1 합성 적용 첫 회귀 | 미리보기 성공 뒤 적용 1건 실패 | 별칭 적용·복구 통과 | alias-extension의 두 번째 해시 검사 발견·수정 |
| 2026-10-22 09:28 | R11.1 확장 회귀 | 4통과·1실패 | 5/5 | legacy Qi가 오래된 공유 해시를 복사하는 경로 발견; 신규 Qi 기록만 현재 원본 해시로 검증 |
| 2026-10-22 09:33 | R11.1 수정 전·후 회귀 | red 2통과·3실패, green 5통과 | green 5/5 | 유효한 red 실행; 별칭 적용·중단 복구·legacy Qi·교체 거부·진단 비공개 |
| 2026-10-22 09:33 | R11.1 기존 cutover 첫 실행 | 138통과·3실패·1제외 | 전체 경계 통과 | 3건은 stage 외곽 추가로 원인 assertion 위치 변경; 원래 원인·불변 검사는 유지 |
| 2026-10-22 09:38 | R11.1 최종 focused 관문 | 회귀5·cutover141통과·1제외, strict lint·fmt·build exit0 | 전 검사 통과 | source90b397e5…; 실제 preview2/2·launch 점검exit0·관측파일 불변 |
| 2026-10-22 09:49 | R13 B+B2+D 초기 focused 회귀 | lib9·bin3 통과 | scoped 관문 완료 | 실제 HWND·PE 검증은 별도 진행, 전체 관문 아님 |
| 2026-10-22 10:02 | R13 B+B2+D 최종 집중 관문 | tray673통과·19제외, 이력1·core PE4·수동 HWND2 통과, strict lint·fmt·build·launch exit0 | 첫 배포 관문 통과 | input13972ef1…; 실제 차트12·개요2·아이콘6 및 shell crop 확인 |
| 2026-10-22 10:23 | R13 A 초기 집중 단위 | 코어25/25·CLI1/1 통과 | A 최종 관문 | 후속 표본 요약·reset-only·cached 예측 보완 전 입력, 최종 통과 주장 아님 |
| 2026-10-22 10:46 | R13 A 후속 집중 보완 | 코어35·트레이6 통과 | A 최종 관문 | countdown jitter·고정 헤더 실제 overflow 검증; CLI·lint·HWND 미완료 |
| 2026-10-22 10:53 | R13 A 최종 집중 관문 | core35·tray6·CLI511 통과, 기존tray673 재사용·실제 HWND8 확인, strict lint·fmt·build·launch exit0 | A 표시용 배포 수용 | inputdd6d853c…; 최초core623은 이전입력이며 latest full matrix로 주장하지 않음 |
| 2026-10-22 11:09 | R13 C 로컬 native 무지연 control | Codex HTTP·Claude SSE 4/4 통과, 소유 job 종료·홈 삭제 4/4 | 지연 허용 표 | 기본 동작만 확인; timeout 최댓값 미측정 |
| 2026-10-22 11:09 | D24 amendment3 장기창 표시 | 집중32/32 통과 | C+E 배포에 포함 | 코드1f75213; 24h 경계·30d 평균/최근·일시간 표시 |
| 2026-10-22 11:37 | R13 C HTTP 로컬 native 지연 | 20/20 통과 | hold 허용 표 | 0/5/30/60/120초, 두 클라이언트·preheaders/firstevent; 전부 정리 |
| 2026-10-22 11:37 | R13 C WS/SSE 확장 무지연 | 7/7 통과 | 실제 WS 증거·heartbeat 기반 | 101·실제 response.create·completed 확인; 긴 대기 증명 아님 |
| 2026-10-22 11:37 | R13 C WS 지연 경계 | upgrade 약15초 재시도·firsttext 60초 통과/300초 종료 | keepalive 영향 별도 실측 | native 기본값; long sweep 중단 근거 보존, 제품 실패로 분류하지 않음 |
| 2026-10-22 11:37 | R13 C 실제 Codex 캐시 선행 검사 | 2회 불완전·idle probe 0건 | 같은 계정 첫 요청의1/5/15/30/60분 표 | 계정 선호가 실제 계정과 달라 거부; bound 증가 근거 없음 |
| 2026-10-22 11:37 | R13 C+E 저장·관리·CLI 기반 | core46/46·CLI3/3·strict lint exit0 | 실행 정책까지 연결 | 7d9e60a; 대기/전환 의미 아직 미연결 |
| 2026-10-22 11:52 | R13 C+E 실제 코어 긴급 관리 API | 3/3 통과 | 실행 정책 통합 | strict/anchor/시간 보존·무인증 거부·미관측 거부;0a2ca78 |
| 2026-10-22 11:52 | R13 C 순수 cache bound·표식 검출 | 6/6 통과 | 실측 뒤 실행에 연결 | 287c0ca·테스트 전용 등록; unknown/압축/중복키는5분 기본값 |
| 2026-10-22 11:52 | R13 C native Codex timeout 설정 실험 | HTTP·WS 각6회가 약2초에서 종료 | 변수 효과 확인 | provider stream_idle_timeout_ms=2000; 무지연 성공,5초 지연 종료; native_failed는 성공으로 세지 않음 |
| 2026-10-22 11:52 | R13 C 실제 캐시 세 번째 선행 검사 | idle probe0건·native/ledger 사용량 불일치 | 요청별 정확한 카운터 | native 누적 후보는 기존 관측값 차분과 ledger가 유일하게 일치할 때만 허용하도록 보완 |
| 2026-10-22 11:52 | R13 C 캐시 하네스 자체 회귀 | 51/51 통과 | 실제 측정과 구분 | 누적/요청별·모호성·첫요청·음수·warm단계/TTL 분리; 제품 수용 아님 |
| 2026-10-22 12:17 | R13.A1 실제 HTTP DTO 예측 전달 | 수정 전0/1 → 수정 후1/1, 이전 응답 호환1/1 | 개요·사용량의 예측 텍스트·projection | 실제 AdminClient와 두 직렬화 경로, 설치 수용은 별도 |
| 2026-10-22 12:19 | R13.A1 집중 전달 관문 | 22/22·strict lint·fmt·제품 build exit0 | HTTP 경계 누락 수정 전달 | inputs unchanged=true; 기존 전체 관문 반복하지 않음, 설치 화면 수용 별도 |
| 2026-10-22 12:41 | R13 C+E 호환·원자 편집 기반 | 코어74·CLI3·실제HTTP3 통과, core+CLI testkit all-target strict lint exit0 | 실행 정책까지 통합 | legacy fence·동시 urgency 보존; testkit 누락 명령1건은 미실행 실패로 별도 기록 |
| 2026-10-22 12:41 | R13 C+E 대기 예산·Claude 인자 계획 | 예산8/8·인자11/11, strict lint exit0 | 실제 요청과 native 의도 연결 | 둘 다 테스트 전용 등록; 동작 활성화 아님 |
| 2026-10-22 12:41 | R13 C Codex 6h 설정·600초 실측 | HTTP·WS 각0/600초, 4/4 통과·재시도0·정리4/4 | 5시간 대기·초기 upgrade 별도 | 실제 600초; 5시간 증명 아님, extended timeout은 합성 홈에만 설정 |
| 2026-10-22 12:41 | R13 C 기본 keepalive 실측 | WS ping60초 통과·300초 종료; HTTP comment Codex300초 종료·Claude600초 통과; typed ping 두 클라이언트600초 통과 | 전송별 안전한 대기 | 18경우·모든 소유 job 종료/home 삭제, 중복 glob 결과 제외 |
| 2026-10-22 12:41 | R13 C 실제 캐시 네 번째 측정 | 1/5분 input20908·cached20736, aux선행으로 unknown;15분 native실패·30/60분 미측정 | 유효한 첫 요청 캐시 수명 | 60초 bound 유지; 다섯 번째 무작정 반복 없음 |
| 2026-10-22 12:41 | R13 C 5시간 WS 하네스 첫 실행 | 무지연1통과·18000초 worker 거부1, cleanup완료 | 실제 장기 대기 | 상속 worker의600초 허용범위 수정·실제 admission6회귀 통과 후 별도 백그라운드 재실행 |
| 2026-10-22 13:13 | R13 C+E 긴급 등록 집중 단위 | core96·실제HTTP9·CLI14 통과, strict lint·fmt·diff exit0 | 단위 닫기 | 입력 변화0; 81536cf·f39e504, C+E 전체 관문 아님 |
| 2026-10-22 13:13 | R13 C Claude UUID 합성 native 증명 | 1/1 통과, 8.19초·정리 완료 | C8 정확한 세션 식별자 | 대문자 UUID가 실제 요청 metadata에 그대로 전달됨; core/UI 경로 증명 아님 |
| 2026-10-22 13:13 | R14 이동 기록 사전 조사 | 6.1978일·실제 이동53·10분 최대9·30분 내 복귀15 | D29 기본값 정당화·실행 차단 | 본문 없는 allowlist; 7일 전체 관측이나 권위 재생 입력이라고 주장하지 않음 |
| 2026-10-22 13:57 | R14 실행 보호 집중 단위 | guard/selection17·reducer20·ledger9 통과; 실제 HTTP1/2 | 강제 거절 typed429·WS·실제 화면 관문 | 입력 해시 불변; HTTP 비용 전달은 통과, 결제 오류 분기 우선순위 수정 뒤 집중 재검증 중 |
| 2026-10-22 14:04 | R14 실제 HTTP·트레이 집중 회귀 | HTTP2/2·트레이3/3 통과; 실제WS0/2 | WS 전송순서·재시도 집계·실제 화면 | WS fixture 식별자와 실제 전송 전 claim·재시도 계승 지적 보완 중; full gate 아님 |
| 2026-10-22 14:23 | R14 전송·실패 claim 집중 회귀 | 원장12·스트림20·실제HTTP7·실제WS4·트레이3 통과; strict lint1건 | 실제 HWND·full gate | 입력 변화0; 논리 동등한 불리언 단순화 뒤 원장·lint·화면 실행 중 |
| 2026-10-22 14:33 | R14 실제 HWND 비용 전달 | 개요·사용량180토큰, 측정1/미측정0; 인증·기록보존 통과 | 최종 SSE 경계·full gate | 실제 core/AdminClient/tray; PNG3개 직접 확인, 초기 하네스 import 오류 보존 |
| 2026-10-22 14:43 | R14 최종 SSE 경계 집중 단위 | 스트림23·실제HTTP11, strict lint·fmt·diff·build·새 실제 HWND exit0 | 실제 차단 알림·전체 matrix | inputs 변화0; 9ea0466와 동일 소스, 성공 경계·oversized/malformed unknown·바이트 보존 |
| 2026-10-22 14:46 | R14 실제 코어 전체 차단·알림 | MoveBreaker 실제 balloon·대상 전송0, 소유 PID/fixture 정리 | 최종 matrix·clear 후 비용 보존 재확인 | 별도 clear 보존은 기존 real-path로 증명; 추가 하네스 assertion 순서 보완 후 재실행 예정 |
| 2026-10-22 15:30 | R14 재시도 증거 독립 검수 | P1 1건 확인·수정; rustfmt·diff 통과 | 기본 예산 그대로 quota4·payment3 회귀 | 코드1eb26ce; 성공 테스트 수는 미주장 |
| 2026-10-22 15:30 | R14 2차 준비 합성 검사 | 재생 커널34·변경 표면13 통과 | 실제24시간 재생·컴파일 소유권 검사 | ignored prototype, 제품 수용과 별도 |
| 2026-10-22 15:35 | R14 공통 의존성 재빌드 | qb-testkit offline build exit0, 259.8초 | 수정본 제품 회귀·전체 관문 | 공통 의존성만, 제품 수용 합계와 별도 |
| 2026-10-22 15:35 | R14 D29 재생 관문 명세 | item11·coverage/unknown/서비스 유지 기준·diff 점검 완료 | 실제24시간 production router 재생 | 실행 통과 주장은 없음; 1차/2차 전달 구분 유지 |
| 2026-10-22 15:48 | R14 재생 서비스 유지 비교 | v2 합성47 통과 (기존34+추가13) | 실제24시간 production router 재생 | 무승인 deny-all/hold 통과 금지·건별 사전 허용 검증; 실제 trace 수용 아님 |
| 2026-10-22 15:48 | R14 증거 관측 순서 독립 검수 | pre-arrival Cooling 경합1건 확인·보완 중 | stale/equal 거절·fresh 허용·제품 회귀 | 이전 compile 중단 기록 보존; 관측 시점은 의미 증거와 별도 |
| 2026-10-22 15:50 | R14 관측 순서 수정본 동결 | c5a8bec·rustfmt/diff 통과 | 과거/동시각 거절·새 관측 허용·실제 경로 | 실제 commit Instant와 source/target 관측 분리; 테스트 전용 우회 없음 |
| 2026-10-22 16:00 | R14 관측 순서 집중 단위 | lib21·quota4 WS1·payment3 WS1·실제HTTP11, fmt·양쪽 strict lint 통과 | 전체 matrix·새 실제 차단/clear 보존 | c5a8bec; 기본 예산 그대로, 독립 Sol source closeout 완료; gate 전체 통과 아님 |
| 2026-10-22 16:10 | R14 전체 기본 코어 첫 결과 | 681통과·8실패·0제외 | fixture/제품 원인 분리 후 재검증 | command는 no-fail-fast로 나머지 workspace 실행 중; 원본 실패 진단 보존, 기본 예산 완화 없음 |
| 2026-10-22 16:19 | R14 기본 전체 첫 관문 | 2292통과·13실패·17제외, exit101·1225.11초 | 보완 후 집중 회귀·최종 관문 | source c5a8bec 바이너리; 변경 입력4개 별도 보존, 수정본 통과로 재사용하지 않음 |
| 2026-10-22 16:19 | R14 임시 실행 병합 원인 | unused source ID를 먼저 생성해 기존 canonical과 AliasConflict | 실제 동시 진입 한 binding·표준 집계 보존 | 없는 source명만 기존 target ID에 연결; 양쪽 기존 ID 충돌은 계속 거절, 회귀 추가·실행 중 |
| 2026-10-22 16:27 | R14 보완 집중 실행 | 코어690·실제 동시 진입HTTP1·CLI2 통과, 입력 변화0 | WS2건·strict lint·최종 matrix·새 실제 차단 | 원자 커밋2610f6f/a82a348/2c67a94/fd45f82; 원래 실패 보존, 기본 예산 변경 없음 |
| 2026-10-22 16:27 | R14 보완 독립 Sol closeout | alias 제품 수정·WS fixture delta에서 추가 확정 결함 없음 | WS 실제 실행·최종 관문 | 정적 범위 검수; 실행 수용과 분리, full gate 통과 미주장 |
| 2026-10-22 16:31 | R14 실제 WS 보완·기본 예산 회귀 | unknown403·drain·기본quota4·payment3 각1통과, 기본 all-target strict lint 통과 | 전체 기능 lint·양쪽 workspace·standalone·새 실제 경로 | 합산 집중697; 전체 관문 완료 아님, 기본 guard/전송/cancel 조건 보존 |
| 2026-10-22 17:28 | R14 1차 최종 실행 관문 | 기본2306·전체기능2671·단독코어802 통과, 실패0; 양쪽 strict lint·fmt·build·read-only check 통과 | 2차 proof/static/실제24시간 replay | 소스fd45f82, 명령별 fingerprint·제외 수 보존; 전체 D29 완료 아님 |
| 2026-10-22 17:28 | R14 실제 경로 최종 차단 | actual core/AdminClient/tray 4PNG, MoveBreaker 알림·대상 추가 전송0·clear 뒤180토큰 보존 | 설치 후 live 수용은 설계 세션 | harness target 정리 실수로 첫 캡처 실행 전 실패; 재빌드+캡처만 재실행, 원본 실패 보존 |
| 2026-10-22 17:28 | R14 2차 준비 검사 | 변경면 inventory 합성18통과; carrier/owner/회계 제안 독립 검수 | 제품 적용·실제 경로·음성 대조·구조 관문 | ignored 준비물이며 제품 실행 수용 수에 합산하지 않음 |
| 2026-10-23 01:08 | R14.1 보호만 분리한 집중 관문 | guard21·R1419·실제HTTP12·실제WS123 통과, strict core all-target testkit lint·fmt·diff exit0; 소스8c3a974 | 설치·live 확인은 설계 세션 | 최초HTTP8통과/4실패는 공유SERIAL잠금30초 대기; 실행스레드1에서12통과, 제품 기한·내부 경합 유지. Sol 소스 PASS, 수집기 제외 |
| 2026-10-23 01:12 | R14.1 설치 수용 (설계 세션 측정) | takeover 1,717회 중 연결 거절0; codex-qb exec·images exec 통과; 실제 dashboard 재독 토큰 지표 표시 | 보호 설치 | dev249a14c; .sandbox/reports/R14.1-evidence/live-dashboard.png, 관측 시작10/23 01:12·관측된 이동 없음 |
| 2026-10-23 02:21 | R16 한국어 보존·영문·snapshot 집중 검사 | core6·tray274+127·통합29·CLI2 통과, strict all-target lint·fmt·testkit build exit0 | brief 집중 관문 | R16-evidence/delivery-fingerprint.json에 명령별 입력·재사용 범위 |
| 2026-10-23 02:21 | R16 실제 경로·오프라인 렌더 | 실제 core/AdminClient/tray 3HWND 및 export, 최종 CLI20 PNG 통과 | D27·영문 light/dark 각5뷰 | 합성 loopback, fixture 정리 완료. 코드19b8d22, 실제 설치 미실행 |
| 2026-10-23 02:26 | R16 설치 수용 (설계 세션 측정) | dev006449a·takeover 거절0/1,811; 실제 debug snapshot·render-snapshot 통과 | README용 영문 화면·도구 | R16.1로 영어 폭 보완, 설치는 설계 세션 실행 |
| 2026-10-23 02:52 | R16.1 영어 고정 문구·한국어 보존 | 집중405통과, strict lint·fmt·build exit0; 한국어 전후 PNG4개 바이트 동일 | 요청1024×640·1320×900, scale2·두 테마 | 3737fe2; R16.1-evidence/delivery-fingerprint.json, 수정 전 실패·수정 후 통과 |
| 2026-10-23 02:52 | R16.1 D27 실제 경로 | spawned core·AdminClient·tray 3HWND 및 snapshot CLI5뷰, 종료·fixture 정리 통과 | 실제 전달·렌더 경로 수용 | R16.1-evidence/real-path-final/completion.json, 실제 설치·실프로필 변경 없음 |
| 2026-10-23 04:26 | R16.2 집중 검증 | 고유428통과·strict clippy 3crate·fmt·build exit0 | 한 줄 예측·상세 보존·경고 경계·기존 배치 회귀 | fingerprint에 명령·exit·입력SHA 보존; full matrix 재실행 주장 아님 |
| 2026-10-23 04:26 | R16.2 실제·합성 렌더 | KO/EN 실제8HWND·합성120PNG | D27 실제 core/AdminClient/tray 및 두 크기·테마 | 직접 시각 검수 수용; fixture·자식 프로세스 정리 |
| 2026-10-23 05:19 | R16.3 집중 검증 | 고유464통과·strict clippy 3crate·fmt·build exit0 | 집중 관문·기존 R16.1/R16.2 회귀 | delivery-fingerprint.json에 명령별 입력SHA·exit·합계; 전체 workspace 재실행 미주장 |
| 2026-10-23 05:19 | R16.3 실제·합성 렌더 | KO/EN 실제10HWND·합성8PNG 직접 확인 | D27 실제 core/AdminClient/tray·두 크기·테마 | fixture 정리·소스/바이너리 parity; 최초 실패와 exhausted baseline red 보존 |
| 2026-10-23 06:00 | R16.4 잔여량 집중 검증 | 453통과·strict tray all-target lint·fmt·testkit build exit0 | 현재 snapshot/소진0/paused 제외/이력 갭·기존 영어 배치 | R16.4-evidence/delivery-fingerprint.json에 명령·exit·입력SHA; 전체 matrix 미주장 |
| 2026-10-23 06:00 | R16.4 실제 경로 | KO/EN core→AdminClient→tray 10HWND 확인 | D27·snapshot 실제 전달 | fixture 정리·소스/바이너리 parity; visual-review.json |
| 2026-10-23 06:12 | N6 최소 기록기 집중 관문 | replay45·config20·실제 HTTP1·12세션 cohort1·takeover3 검사, strict lint·fmt exit0 | schema/off/7일 bounds·기본 동작 보존 | minimum-delivery-fingerprint.json의 중복 제거 합계·명령별 입력SHA·exit; 전체 replay 수용 아님 |
| 2026-10-23 06:31 | N6 최소 설치 | epoch e00·첫 segment 363 KB, 연결 timeout1/1,779·refusal0 | 설계 세션 실제 takeover probe·ledger 오류0 | listener admission과 writer join 순서는 N6t/N6r 후속 점검 |
| 2026-10-23 06:52 | R16.5 집중 관문 | 고유466통과·strict tray all-target clippy·fmt·testkit build exit0 | 희소 이력·소진0·리셋100·명시적 unknown 갭·한영 문구 | R16.5-evidence/delivery-fingerprint.json; baseline red 예상 실패·정확한 소스 복원·독립 소스 PASS |
| 2026-10-23 06:52 | R16.5 실제 경로·렌더 | 실제 KO/EN10HWND·동일 export2PNG·합성 전체이력4PNG 확인 | core 정상 이력복구→실제 AdminClient→tray·production renderer | 실제 Claude19시간 선·문구 확인, fixture 삭제·소스/바이너리 parity |
| 2026-10-23 06:58 | R16.5 실제 설치 | dev19353ac·연결 거절0/1,920 | 설계 세션 실제 takeover probe | recorder e01 단일 epoch로 24시간 창 갱신 |
| 2026-10-23 07:00 | N5a 실제 HTTP 취소 정리 | 실제 TCP close 검사1·strict core/testkit all-target clippy exit0 | preheader 대기 후 소켓 양방향 종료·초기화 후 upstream0건 | 1초 이내 lifecycle·본문 예산 반환, n5a-tcp-close-fixture/n5a-tcp-lint 증거; 전체 N5a 완료 아님 |
| 2026-10-23 07:27 | N5a 실제 관측 근거·저장소 무효화 | registry25·실제 Store/runtime10·Store 경계9, 합계44통과·exit0 | 전송 전 origin·공유 순서·부분 응답·실패/종료 시 권한 취소 | 7cc9d0f 단위; 조기 실패3경로 독립 지적 보완·source recheck PASS. dev18d93dc 병합 후 집중 검사 진행, 대기 실행 수용 아님 |
| 2026-10-23 08:07 | N5a HTTP429 근거 완료 | N5a72·판정18·실제 TCP1, 총91통과; strict core/testkit all-target lint exit0 | 헤더 즉시 적용·본문 완료 후 최종 근거·새 보고 무효화 | ce351c2; 이전 코드 실제 조기 mint 대조 exit101·정확 복원 exit0. 실제 대기/재개 승인 아님 |
<!-- support:end -->

<!-- gaps:begin -->
| 남은 차이 | 지금 | 넘어야 할 값 | 다음 행동 |
|---|---|---|---|
| 기록기 N6t/N6r | 최소 기록기 설치·수집 중, 초기 cut/전체 owner/실제 replay 미수용 | 완전성 증명·24시간 차등 재생 | 10/24 06:58 이후 e01 단일 epoch export 검증; takeover listener/writer 순서 점검 |
| 대기·재개 N5a-d | HTTP429 근거72·판정18·TCP 종료1 통과, 최종 대기 허가 미연결 | HTTP/WS 취소·초기화 재개·긴급 이동 | 본문 네이티브 전이 합성·실제 대기 승인·무효화 알림과 HTTP 재개 연결 |
| 통합 예측 N7 | 검증 가능한 24시간 입력 부족 | 명시한 오차 범위의 backtest | body-free 입력·훈련/후속 관측 검증, 풀 예측 표시는 보류 |
<!-- gaps:end -->

R11 Windows process/crash focused verification uses `cargo test -p quota-baton --all-features --test r9_crash --no-fail-fast -- --test-threads=4`. It still runs cases concurrently and preserves every in-case race, kill checkpoint, 20-second command deadline and five-second reap deadline. Default high-concurrency deadline failures and diagnostic traces remain cited; the bounded run is not a claim that the default-load timing issue was fixed. No product timing or ownership workaround was added.
