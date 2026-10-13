# AGENTS.md: rules for agents working on quota-baton

The documentation map is in `docs/README.md`. The normative behaviour is in `docs/contract.md`.

## Build hygiene (C: has little free space; D: and E: are full)

- **One target per worktree.** Use the worktree's default `target/` only.
  - Do not create extra `CARGO_TARGET_DIR`s per feature set, per reviewer or per test run. Examples to avoid: `.sandbox/target-*`, `.sandbox/targets/*`.
  - Build the feature variants one after another in the same target.
- **No incremental caches.** `.cargo/config.toml` sets `incremental = false`. Do not override it.
- **Clean up at the end of a milestone.** Run `cargo clean` in the worktree after the milestone report is sent, unless the next milestone starts immediately in the same worktree.
- **Run a review build in the implementer's worktree only while the implementer is idle.** Use the default target, run `cargo test` / `cargo clippy` sequentially, and do not keep a second copy.
- Never move build output to D: or E:.
- If a cleanup is blocked, report the exact path and size instead of working around it.
- **Retention of run artifacts.** Verification copies, test binaries, temporary homes, and capture or serve logs under `.sandbox/` are deleted once their milestone is merged. Before deleting, record the result in the milestone report.
  - Keep only what a report or review cites: `.sandbox/reports/<M>.md` with its cited evidence files, `.sandbox/reviews`, `.sandbox/briefs` and `.sandbox/research`.
  - Never copy test executables or debug symbols (`*.exe`, `*.pdb`) into `.sandbox`. Cite their test names instead.
  - Body captures that contain conversation content are deleted as soon as their result is recorded.

## Working rules for the implementer main (supervisor common rules, 2026-10-13)

- 질문 경로(2026-10-11 사용자 지시): GOAL과 설계 문서로 정할 수 있는 것은 스스로 정한다. 그것으로 정할 수 없어 남은 작업 전체가 막히는 결정만 감독 세션(herdr 패널 <pane>)에 바로 묻는다: `herdr agent prompt <pane> "[메인 세션이 보낸 질문 · 사용자 입력 아님] <질문, 맥락, 선택지와 추천>"`. 막히지 않는 질문은 `docs/questions-for-supervisor.md`에 모아 두었다가 마일스톤 관문에서만 한 번에 보낸다(단위마다 보내지 않는다). 단위 결과, 진행 보고, 확인 요청, 인사는 보내지 않는다. 감독은 결과를 저장소와 기록에서 직접 본다. 답을 기다리는 동안 그 답에 기대지 않는 작업을 계속한다. 사용자 의견이 꼭 필요한 결정(돈, 계정, 공개·배포, 취향과 최종 승인)만 맥락과 선택지를 붙여 쉬운 문장으로 사용자에게 묻는다.
- 마일스톤 완료 보고는 마일스톤 관문에서 한 번만 보낸다(보고서는 `.sandbox/reports/<M>.md`).
- 기다림은 멈춤 조건이 아니다(2026-10-11 사용자). 사용자 판단이나 외부 근거가 필요한 일이 생기면 감독 세션(herdr 패널 <pane>)에 바로 보내고, 감독 세션은 맥락과 선택지를 붙여 쉬운 문장으로 사용자에게 묻는다. 답을 기다리는 동안 그 답이 필요 없는 나머지 작업(다음 마일스톤, 골격의 가장 큰 빈칸, 검증·정리)을 모두 끝낸다. 답이나 외부 근거로만 할 수 있는 일만 남았을 때만 멈추고, 무엇을 누구에게 물었는지와 재개 조건을 남긴다.
- 환경변수나 설정·인증 파일을 통째로 조회하거나 출력하지 않는다. 필요한 값은 이름으로 존재 여부만 확인하고 값은 출력하지 않는다.
- 사용자가 의도적으로 끈 Claude 세션은 다시 켜지 않는다(2026-10-12 사용자).
