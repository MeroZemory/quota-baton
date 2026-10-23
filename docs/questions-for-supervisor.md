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


## R8: 월간 등 비표준 길이 창의 gate·정렬

비차단 질문이다. C9의 `43200m`(30일)·양수 초 단위 창은 D20 근거 갱신·배정량 해제·poll 추적에서 처리한다. 기존 C7 gate와 초기화 정렬은 5h/7d 및 primary/secondary 별칭 규칙을 유지했다. 월간 창에 주간 90/98% 기준을 새로 적용하거나 정렬 순서를 발명하지 않았다. 기존 기준을 명시적으로 유지할지, 비표준 길이 창의 gate·정렬 슬롯을 별도로 정할지 관문에서 확인한다. 실프로필은 조회하지 않았다.

## R9 Addendum 3 feasibility (gate question, 2026-10-21)

Independent Sol source/native investigation: `.sandbox/research/R9-panel-feasibility.md`. Native Codex 0.163 falls back to rollout filenames when its thread row is absent and recreates the row. Thus keeping source S as a fork-ancestor link (G6) while removing its row does not make raw `codex` in the old home fail closed (G7); it can resume S and become a second row owner (G1). Process-held Codex locks also disappear when the importer is killed, so exposed links in two homes are writable before journal recovery (G3). Both have synthetic native counterexamples; no real home was touched.

Implementation continues on typed confirmation, inventory, current-state return, map journaling and tests. Before merge, please rule whether G1/G3/G7 cover only qb-mediated launches/recovery, or whether raw native guarantees require additional upstream/launcher authority (outside currently approved means). Recommendation: do not claim raw native guarantees; retain raw counterexamples as evidence and explicitly scope ownership enforcement, or approve a stronger mechanism before merge. This is queued until the milestone gate, while independent implementation remains.


## R10: 폭에 들어가지 않는 세션 부하 기준 칸 수

비차단 관문 질문입니다. 대시보드 v2 §3.3은 기준 T개만큼 칸을 그리도록 하지만 C7의 유효 기준은 i64::MAX까지입니다. 유한한 창에서 그 수를 모두 표시할 수 없습니다. 구현은 기본 5칸과 폭에 들어가는 21칸을 정확히 표시합니다. 들어가지 않는 경우에는 칸 수를 폭으로 제한하고 줄임표, 전체 N/T, 생략 수 접근성 설명을 유지합니다. 기준 수만큼 반복·할당하지 않습니다.

추천: 이 한정된 표현을 극단값의 공식 예외로 승인합니다. 설정 상한을 새로 줄이거나 나머지 필드를 밀어내는 변경은 하지 않았습니다. literal T 규칙과의 편차 및 회귀 근거는 `.sandbox/reports/R10-evidence/pip-fallback.md`와 독립 검수 `R10-final-requirements-sol.md` D1에 기록했습니다. 전체 관문 보고에서 한 번만 확인하며 다른 구현·검증은 계속합니다.


## R12.10: Windows loopback bind가 커널에서 반환하지 않음

관문 시점의 외부 환경 질문입니다. 렌더 수정의 실제 창 성능 조건, release 라이브러리220건, UIA 비교2건, 실제 Host1건과 strict clippy는 통과했습니다. 최종 qb-tray 테스트 중 기존 HTTP 검사6건이 Windows socket bind에서 멈췄습니다. 실행 소유권을 확인한 뒤 해당 cargo/test 프로세스만 종료했습니다.

별도 `python -I -S` 프로세스에서 qb를 불러오지 않고 `socket().bind((127.0.0.1,0))`만 실행해도8초 안에 반환하지 않습니다. 테스트 native stack은 std::net::TcpListener::bind → ws2_32!bind → mswsock → NtDeviceIoControlFile이며, 다른 테스트들은 mio 초기화 Once를 기다립니다. 보고서 보조 Python도 전역 site hook의 IPv6 지원 확인 bind에서 멈췄고, site를 건너뛰면 보고서 생성은 정상입니다. TCP 상태 집계는 Established110, TimeWait160, Bound260으로 기록했지만 원인은 단정하지 않습니다.

OS/보안/네트워크 계층의 담당 확인과 점검을 감독 세션에서 맡아 주실 수 있나요? 실제 서비스·설정·계정은 변경하지 않았습니다. 시스템 재시작이나 보안/네트워크 설정 변경은 하지 않습니다. 근거는 `.sandbox/reports/R12.10-evidence/gate-stall/`입니다. 이 외부 조건을 기다리는 동안 네트워크가 필요 없는 R11 통합을 계속합니다. 관문은 통과로 표시하거나 dev로 병합하지 않았습니다.


### R12.10 OS bind 판정과 관문 종결

감독 세션은 OS System 로그로 원인을 확인했습니다. 보고된 타임라인: 04:38:53 nProtect Online Security (PFS), INCA nossvc 설치 및 TKFWFV/TKFWVT/TkIdsVt 로드; 04:39:04 TKFsFtM; 04:40:19 Hyper-V VmSwitch의 WSL/Default Switch 재구성; 04:50–04:51 trustlet 시작/종료. 약04:49 bind 중단은 이 구간에 있었으며 QB·포트 고갈 결함이 아닌 일시적 보안 소프트웨어 설치 영향으로 판정됐습니다. 이후 새 이름 exe를 포함한 loopback bind는1–3ms였습니다. 제품 우회 코드는 추가하지 않았습니다.

감독 지시에 따라 멈췄던 HTTP6건을 각각 한 번만 재실행했고 모두 exit0/1 passed였습니다. 이 판정으로 관문을 인정합니다. 원본 aggregate는 interrupted로 보존합니다(라이브러리222통과; 완료된 전체 합계는 주장하지 않음). fmt·strict clippy·정상 제품 build exit0. 근거: `R12.10-evidence/gate-stall/retry-six/completion.json`, `normal-build.json`, `final-fingerprint.json`.


## R13 A gate: non-blocking forecast policy

D24 explicitly chooses average-first for 7d and recent-first for5h. Current other explicit window lengths (including30d Free) use recent with average fallback. Independent source review recommends considering average-first for all >1d windows. No current normative violation or gate blocker; keep existing implementation pending a later policy choice.


## R14 second delivery: authoritative 24-hour replay input

Non-blocking question for the runtime-delivery gate; no unit message is sent.

The available 6.1978-day move inventory is not a complete decision tape. It does not contain a coherent initial router state, every candidate/owner observation, or branch-dependent lifecycle and accounting facts. The synthetic replay kernel and source review cannot recover that missing history or satisfy D29's real 24-hour gate.

Recommendation: implement a bounded, opt-in metadata recorder with explicit owner DTOs, pseudonymous identities, initial-state reconciliation and loss manifests. The design session enables it after reviewing the schema and operator controls, and supplies the sanitized export. The main never reads the live profile. The proposed input contract is in `.sandbox/r14-prep/replay-capture-inputs.md` in the implementation worktree.

At the gate, please confirm whether an existing complete sanitized tape is available. Otherwise, please settle the installation/capture step needed before the final structural/replay acceptance. The requested runtime-first delivery can ship independently; owner separation, proof producers and synthetic/static checks continue while capture is arranged. No waiver of the 24-hour gate or acceptance of a move-only replay is requested.

## R13 N5d: native continuation boundaries (collect for the gate)

Codex integration continues independently; these are not reasons to stop it. The physical and initial full-proof HTTP producers have actual scoped owner tests. Their final publication does not authorize a native command by itself.

Two remaining compatibility questions need a gate decision if further evidence does not close them:

- Existing interactive Claude TUI: the pinned public TypeScript/Python interfaces reviewed so far do not establish a non-user command that resumes the same owned, quota-stopped TUI. A new user prompt, guessed keystroke or process restart would change the acceptance boundary. Recommendation: preserve the no-activation behavior for this unsupported path, label it explicitly, and decide whether a separately specified owned-client mechanism is acceptable. Evidence: `R13-evidence/n5d-core-owner/claude-primary-continuation.md` in the main repository report directory. This is a bounded evidence limit, not a claim of impossibility.
- Received Codex WS quota errors: supported client source has wrapped errors with quota headers, but the inspected real-source fixtures do not prove a complete five-hour/long-window report plus unambiguous physical attempt identity. Separate unlabelled rate-limit and rejection frames cannot acquire that identity through timing, adjacency or a merged snapshot. Recommendation: decline registration from those incomplete frames; decide whether to obtain a sanitized complete wire fixture or specify an independently qualified full-poll producer for that path. Source research: implementation `.sandbox/r13-part-c/n5d-native/ws-producer-shape-review.md`. A cloned report origin is single-use and cannot mint successive stream reports.

These questions will be sent once with the milestone gate batch. No unsupported continuation, alternate transport or weaker proof is being enabled while the managed Codex parent is integrated.
