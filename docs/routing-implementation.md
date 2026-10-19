# 계정 전환 보완 구현 기록

사용자 지시(2026-10-20): 설계 보완안을 원자적으로 커밋하고 구현, codex-ax Sol max 독립 검수, 테스트까지 진행한다. 기존 실프로필·설치·이전 프록시 종료 보류는 유지한다.

기준: `b70dade` 설계 보완, 그 이전 작업은 `1f260cb`까지 dev에 커밋되어 있다. 기존 작업 트리는 clean이었다. 별도 worktree는 만들지 않는다.

## 구현 단위

1. [ ] 영속 루트 정책(strict/recover/rebalance), 프로젝트 기본값/상속, 관리 API·CLI. 기본값은 현행 rebalance/60초를 유지한다. 추가 크레딧 권한은 바꾸지 않는다.
2. [ ] 그룹 체류·재방문/이동 예산, 대상 신뢰·예약·실행 시 재검사, unknown 비용·idle pin 등 관측 정확성.
3. [ ] 요청 전체 deadline과 dispatch 권한 재검사, reset 근거·epoch·취소, rate 대기 만료 군집 방지.
4. [ ] bounded 계정/window 소비 예측 shadow. 미지원 window·불확실·reset/부하 변경은 fallback. 실제 배정 변경은 기본적으로 끈다.
5. [ ] HTTP·WS·재시작·takeover·적대적 왕복/동시 배정 합성 회귀. UI/CLI와 문서의 설정 의미 일치.
6. [ ] 독립 `codex-ax` gpt-6.1-sol max 실제 구현 검수와 수정. 마지막 전체 gate를 한 번 실행하고 fingerprint/report 기록.

수치가 미결정인 새 정책은 현재 기본값을 유지하거나 명시적 opt-in으로 제공한다. 예측의 효과를 측정 없이 확정하지 않는다. 단위별 commit, focused 테스트; 마지막 gate에서 전체 matrix를 순차 실행한다. Cargo는 default target, incremental=0, debug profile override 없음, 8GiB cap을 지킨다.

이 문서는 현재 진행 목록이다. 완료하지 않은 단위를 완료로 보고하지 않는다. 설계 세션 <pane>에는 보내지 않는다.
