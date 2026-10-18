# 감독 결정 대기

## R2B: CPA를 멈추기 전 중단된 작업의 되돌리기

상태: 해결. 감독이 추천 예외를 승인했다 (2026-10-18). tasks만 완료되고 stop/adopt/guard 이전이며 원래 pidfile·exe·start·listener가 모두 일치하면 tasks만 복원한다. 불일치는 한국어로 거절한다. R2B crash matrix와 보고서에 반영한다.

R2 Part B는 모든 단계에서 복원을 지원하면서, 복원 시작 전에 10100이
비어 있어야 한다고 정한다. 그런데 retire는 예약 작업을 먼저 끈 뒤 CPA를
멈춘다. 작업을 끈 직후 중단되면 원래 CPA가 여전히 10100에서 실행 중이고,
토큰 인수와 파일 변경은 하나도 일어나지 않았다. 이 상태에서 포트 조건을
그대로 적용하면 예약 작업만 되돌리는 것도 거절된다.

추천: journal이 process stop·adopt·guard 이전임을 증명하고, 현재 프로세스가
원래 pidfile·실행 경로·시작 시각·listener와 모두 일치할 때는 **예약 작업만
되돌리는 경로**를 허용한다. 프로세스를 종료하거나 토큰 파일을 건드리지
않는다. 자격 증명 소유권을 되돌리는 나머지 복원은 10100이 비어 있어야 한다.

대안: 모든 복원에 포트 조건을 유지하고, 이 단계에서도 사용자가 검증된
이전 프록시를 먼저 직접 멈춰야 한다고 문서화한다.

근거: `.sandbox/briefs/R2-adopt-retire-cpa.md` Part B의 단계별 복구·실행 순서·
포트 조건, `.sandbox/reviews/D17-review.md` §5 항목 17·21·23·25.
실제 CPA에 작업할 권한을 요청하는 질문이 아니다.

## R2B: wiring이 이미 끈 CPA 작업으로 복원 시작

상태: 해결. 감독이 journaled 임시 enable → Run → 10100 listener 검증 → 기록된 disabled 상태 복귀를 승인했다 (2026-10-18). 중단 때도 기록된 상태로 복구한다. 실제 CPA와 설치 설정을 바꾸는 작업은 설계 세션 재개 전까지 보류한다.

D17 review MF5는 wiring.json이 CPA 작업을 소유하면 retire가 그 작업을 다시 끄거나 복원하지 말라고 한다. 같은 복원 절차는 CPA를 `schtasks /Run /TN cliproxyapi-proxy`로 시작하도록 한다. wiring이 해당 작업을 꺼 둔 상태에서는 Run이 실행되지 않는다. 원래 작업이 비활성이었던 경우도 같다.

추천: 시작 단계에 한정한 journaled 임시 enable → Run → listener 검증 → 원래 disabled로 복귀를 허용한다. wiring의 소유권과 최종 상태는 그대로 두며, 실패/중단 때도 임시 변경을 복구한다. CPA 실행에 필요한 시작 단계를 제외한 disable/restore는 wiring 소유 작업에 하지 않는다.

대안: 비활성 작업이면 모든 token/file 변경 전에 복원을 거절하고 사용자가 별도 활성화하도록 안내한다. 다만 retire journal이 full unwire를 막으므로 사용자에게 복구 절차를 명시해야 한다.

근거: `.sandbox/reviews/D17-review.md` MF4/MF5/SF5와 Part B의 task ownership/start 순서. [Microsoft Schtasks 설명](https://learn.microsoft.com/en-us/windows/win32/taskschd/schtasks)은 비활성 작업이 실행되지 않는다고 설명한다. 실제 작업을 조회하거나 실행하지 않았다.
