# 계정 전환 보완 구현 기록

사용자 지시(2026-10-20): 설계 보완안을 원자적으로 커밋하고 구현, codex-ax Sol max 독립 검수, 테스트까지 진행한다. 기존 실프로필·설치·이전 프록시 종료 보류는 유지한다.

기준: `b70dade` 설계 보완, 그 이전 작업은 `1f260cb`까지 dev에 커밋되어 있다. 기존 작업 트리는 clean이었다. 별도 worktree는 만들지 않는다.

## 구현 단위

1. [x] 영속 루트 정책(strict/recover/rebalance), 프로젝트 기본값/상속, 관리 API·CLI. 기본값은 현행 rebalance/60초를 유지한다. 추가 크레딧 권한은 바꾸지 않는다.
2. [x] 그룹 체류·재방문/이동 예산, 대상 신뢰·예약·실행 시 재검사, unknown 비용·idle pin 등 관측 정확성.
3. [x] 요청 전체 deadline과 dispatch 권한 재검사, reset 근거·epoch·취소, rate 대기 만료 군집 방지.
4. [x] bounded 계정/window 소비 예측 shadow. 미지원 window·불확실·reset/부하 변경은 fallback. 실제 배정 변경은 기본적으로 끈다.
5. [x] HTTP·WS·재시작·takeover·적대적 왕복/동시 배정 합성 회귀. UI/CLI와 문서의 설정 의미 일치.
6. [ ] 독립 `codex-ax` gpt-6.1-sol max 실제 구현 검수와 수정. 마지막 전체 gate를 한 번 실행하고 fingerprint/report 기록.

수치가 미결정인 새 정책은 현재 기본값을 유지하거나 명시적 opt-in으로 제공한다. 예측의 효과를 측정 없이 확정하지 않는다. 단위별 commit, focused 테스트; 마지막 gate에서 전체 matrix를 순차 실행한다. Cargo는 default target, incremental=0, debug profile override 없음, 8GiB cap을 지킨다.

이 문서는 현재 진행 목록이다. 완료하지 않은 단위를 완료로 보고하지 않는다. 설계 세션 <pane>에는 보내지 않는다.

## 단위 검증 기록

- `bcaca4f`: 영속 정책·프로젝트·관리 API/CLI 기반. Sol max 독립 검수에서 P1 4건/P2 2건 발견.
- 후속 단위: 현재 전역 상속, 필수 스키마/링크·읽기 cap, 읽기 스냅샷, 시작 전 anchor 보존, 명시적 inherit, 전송 직전 권한 재검사와 왕복/예약 보호를 수정. core lib 334/334, routing HTTP 4/4, testkit all-targets strict clippy 통과. 실제 fake upstream Authorization/account header와 A 대기→B 이동→strict(B) 경합을 확인했다. 최종 독립 재검수/전체 gate는 남아 있다.

- 두 번째 Sol max 검수(`ebf5aae`)의 이전 형식 Codex anchor, 전체 그룹 예산 누락, 실행 시 비용 재검사, 이차 반복, Unix final-link 열기 지적을 보완했다. schema 2는 schema 1의 보수적 alias를 유지한다. 캐시/왕복 보호는 루트·대상·provider 예산과 실제 권한 검사를 함께 적용한다.
- 전체 전송 전 HTTP 시간 예산(기본 300초), WS 연결 전체 시간 한도, 중단 가능한 초기화 대기, 대기 만료 시 강제 전송 금지, 권고 시간 floor, 모델 문맥을 보존한 열린 WS 재전송 검사를 추가했다. 원래 강제 전송을 기대한 합성 회귀는 새 계약의 무전송 조건을 검증한다. HTTP 29/29, WS 76/76 scoped 통과. 마지막 전체 gate 결과가 이 단위 최종 증거다.
- 소비 예측은 `routing.forecast_shadow=false`가 기본이며 자동 배정에는 연결하지 않는다. 제한된 숫자 관측·상하 시나리오·별도 120초 실측 coverage만 표시한다. 실제 소진/절감/확률 보장은 아직 하지 않는다. 90/98% 배정 기준도 바꾸지 않는다.

- 실제 두 코어 프로세스 takeover 회귀 `routing_legacy_strict_anchor_survives_two_process_takeover` 1/1 통과. 기존 형식 strict A 유지, 새 코어의 실제 Authorization/ChatGPT-Account-Id 확인, A 중지 후 다른 계정으로 전달되지 않음을 확인했다.
- 최대 20,000 root의 정책 조회 검사와 Windows 생성/교체/로드의 protected owner ACE 검사를 추가했다. ACL 출력은 BOM 없는 UTF-16도 처리한다. Unix no-follow 코드는 Windows에서 실행 검증했다고 주장하지 않는다.
- 마지막 testkit all-targets strict clippy(qb-core, quota-baton, qb-testkit) 통과. 최종 commit의 전체 matrix와 Sol max 독립 검수/수정 결과는 관문 보고서에 별도로 남긴다.

- 세 번째 Sol max 검수(`ba6348a`)의 WS 대형/미분류 전송, 프로젝트 anchor 원자성, 실패 루트 비용 맵 상한, 복원/자식 비용, HTTP 일반 재시도와 rate 예산 분리, WS 별도 ingress deadline, legacy 프로젝트 편집, 늦은 drain 검사, 로컬 rate 오류/Retry-After, strict 결제·권한 409 우선순위를 보완했다. shadow 계산은 마지막 관측부터 reset까지의 소비를 포함한다.
- 기존 전체 관문은 1,223 passed / 15 failed / 12 ignored로 실패했다. 새 무강제전송·영속 anchor 계약과 맞지 않는 fixture는 실제 요청·자격 증명을 확인하도록 수정했다. 분산 예약의 정렬 불일치와 C5 헤더 소진의 rate 오분류는 실제 코드 결함으로 수정했다. 실패 결과를 통과로 집계하지 않는다. 최종 수정 commit에서 관문과 독립 재검수를 다시 실행한다.

- 초기화 시각이 지난 shadow 진단은 `reset_elapsed` fallback으로 전환한다. 관측이 아직 fresh여도 초기화 이전 잔량을 여유 용량처럼 표시하지 않는다. 합성 회귀의 수정 전 실패와 수정 후 통과를 증거에 보존한다.

- 네 번째 Sol max 검수(`014f732`)의 큰 WS 출력 공개 후 재전송, 순서가 다른 큰 create의 admission/모델 우회, A→B→A receipt, HTTP continuation 비용, legacy 이동 이력 중복, 기존 프로젝트의 빈 anchor 보완, 미전송 timeout의 release slot 소비를 수정했다. payload tree를 만들지 않는 bounded 메타데이터 파서, 살아 있는 배치 epoch receipt, 원장 집계와 전송 승인의 분리를 적용했다. 바인딩 파일도 링크/핸들·소유 권한·16MiB·40,000 항목 상한을 검사한 뒤 legacy anchor 입력으로 사용한다.

- 다섯 번째 Sol max 검수(`07742e6`)의 미분류 소형 JSON 출력, 대기 중 출력에 의한 예약 재전송 취소, 모델 미상 최종 WS 전송 검사, 손상된 strict legacy 복원 거절, writer/reader 상한 일치를 보완했다. 앞선 Option 참조 컴파일 오류는 `010b8b8`에서 수정했고 core 357/357와 rate→느린 auth 회귀 및 clippy로 확인했다. 최종 관문은 이후 코드 commit에 고정한다.
