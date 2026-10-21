# Milestones: unit and gate checks

This file names the verification for each unit and each row. The row list and the order live in `GOAL.md`; this file does not repeat them. Commands run in `C:/_projects/SampleUser/quota-baton-impl`, one after another, in the default target with `CARGO_INCREMENTAL=0`.

## Status (single source for every report head)

<!-- status:begin -->
현재 마일스톤: R11 (실행별 계정 지정과 계정별 모델 목록, codex-rh 같은 계정 별칭)
목표: codex-qb를 실행할 때 쓸 계정을 지정할 수 있게 하고, 계정별 별칭 명령을 제공한다
핵심 지표: 새 경로 출시 1/2 (R12 이미지 출시 완료, R11 실행별 계정 진행 중) · 2026-10-22 01:04 KST
사용자 대기: 없음 (R2 Part B만 사용자 결정 대기)
갱신: 2026-10-22 01:04 KST (R12.9 dev c899b6a 설치 완료)
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
| Backlog | further dashboard and tray polish | — | not before A1 and A2 ship | — |

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
<!-- metric:end -->

<!-- support:begin -->
| 측정 시각 (KST) | 보조 지표 | 값 | 목표 | 비고 |
|---|---|---|---|---|
| 2026-10-22 01시대 (분 미기록) | 대시보드 유휴 CPU (코어 하나 기준, 창 열림) | 60.7 % | 2 % 미만 | 최소화 시 1.4 %; R12.10 전 측정 |
<!-- support:end -->

<!-- gaps:begin -->
| 남은 차이 | 지금 | 넘어야 할 값 | 다음 행동 |
|---|---|---|---|
| R11 실행별 계정 | HTTP 통합 저장, WS 경계 2건과 출시 관문 남음 | 관문 통과와 설치 | R12.10 뒤 WS 경계 2건 마무리 후 관문 |
| 대시보드 지연 (R12.10) | 유휴 CPU 60.7 %, 장면당 약 300 ms | 유휴 2 % 미만, 조작 p95 16 ms 미만 | 장면 단계별 시간 측정 후 다시 그리기 조건 제한 |
| R13 소진 예측·아이콘·차트·캐시 보류 | 설계 완료, 구현 0/5 부분 | 5/5 부분 병합과 설치 | R11과 R12.10 뒤 시작 |
<!-- gaps:end -->
