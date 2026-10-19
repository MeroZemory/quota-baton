# quota-baton 사용자 안내서

quota-baton은 Claude Code와 Codex CLI가 사용할 로컬 프록시입니다. 사용자의 계정을 여러 개 등록할 수 있습니다. 코어는 요청을 전달하고, 트레이는 상태와 계정 작업을 보여 줍니다.

자세한 규칙은 [계약서](contract.md)를 따릅니다. 계정과 토큰은 [C5 계정 상태](contract.md#c5-account-state)와 [C10 OAuth](contract.md#c10-oauth-parameters)에 설명되어 있습니다.

## 설치와 첫 실행

Windows에서 릴리스 바이너리를 준비합니다. 소스에서 빌드하려면 Rust 1.93 환경에서 다음을 실행합니다.

```powershell
cargo build --release --workspace
```

`quota-baton.exe`는 코어와 CLI입니다. `quota-baton-tray.exe`는 트레이 프로그램입니다.

처음 실행할 때는 코어를 시작합니다.

```powershell
quota-baton.exe serve
```

기본 데이터 폴더는 `%LOCALAPPDATA%\quota-baton`입니다. 개발 또는 별도 환경에서는 `QB_HOME`으로 폴더를 지정할 수 있습니다. 설정은 그 폴더의 `config.toml`에서 읽습니다.

코어가 실행되면 상태를 확인합니다.

```powershell
quota-baton.exe status
```

코어는 데이터 포트와 관리 포트를 따로 사용합니다. 관리 API에는 별도의 관리 토큰이 필요합니다. 토큰과 계정 파일을 직접 편집하지 마세요.

## 트레이 사용법

트레이를 별도로 시작합니다.

```powershell
quota-baton-tray.exe
```

트레이 아이콘은 다음 상태를 보여 줍니다.

- 정상: 코어와 계정이 사용할 수 있습니다.
- 주의: 재로그인이 필요하거나 모든 계정이 소진되었습니다.
- 오류: 코어에 연결할 수 없습니다.

아이콘을 왼쪽 또는 오른쪽 클릭하면 플라이아웃이 열립니다. 열린 상태에서 다시 클릭하면 닫힙니다. 목록이 길면 휠이나 Page Up·Page Down 키로 스크롤합니다. 위쪽 요약과 아래쪽 버튼은 고정됩니다.

기본은 밝은 테마입니다. `config.toml`의 `[tray]`에서 `theme = "dark"`로 어둡게, `theme = "system"`으로 시스템 설정을 따르게 바꿀 수 있습니다. 다음에 플라이아웃을 열면 적용됩니다.

위쪽 요약은 Claude와 Codex를 지금 쓸 수 있는지 알려 줍니다. 계정마다 남은 양이 가장 적은 창을 막대 하나로 보여 줍니다. `주간 38% 남음`은 주간 한도의 38%를 더 쓸 수 있다는 뜻입니다. 다른 창과 정확한 잔액은 툴팁이나 행 메뉴에서 봅니다.

파란 칩은 사용 중, 빨간 칩은 다시 로그인 같은 조치 필요, 주황 칩은 크레딧 사용 중입니다. 회색은 기다림이나 일시정지입니다. 준비된 계정에는 칩이 없습니다. 꺼진 계정은 접어 두지만 제공자가 막히면 펼쳐서 `켜기`를 보여 줍니다.

계정 행 어디를 눌러도 메뉴가 열립니다. 행 안의 버튼은 바로 그 작업을 합니다. 메뉴에서는 다음 작업을 할 수 있습니다.

- 다시 로그인
- 일시정지 또는 재개
- 끄기 또는 켜기
- 새 대화 받지 않기 설정을 켜거나 끄기
- Codex 유료 크레딧 사용 허용 설정을 켜거나 끄기
- 사용량 새로고침
- 계정 삭제

Codex 크레딧 사용을 켤 때마다 잔액과 함께 확인창을 띄웁니다. `크레딧 사용`을 눌러야 켜집니다. 취소나 Esc는 바꾸지 않습니다. 끄기는 바로 적용됩니다. 메뉴의 체크 항목을 바꿔도 메뉴는 유지됩니다.

알림은 계정 이름과 다음 처리 방식을 알려 줍니다. 다른 계정이 이어받는지, 크레딧으로 계속 처리하는지, 모두 소진됐는지를 구분합니다. 클릭하면 관련 행이 보이며 메뉴는 닫혀 있습니다. 같은 대상의 같은 알림은 30분 동안 반복하지 않습니다. 새 대화 급증 알림은 클라이언트가 반복 실행 중인지 확인하라는 뜻입니다.

아래쪽에는 계정 추가, 설정, 종료가 있습니다. 설정 메뉴에서 설정 파일이나 로그 폴더를 열고 버전을 봅니다. 종료를 누르면 트레이만 종료할지 코어도 함께 종료할지 선택합니다. Tab과 방향키로 이동하고 Enter로 고릅니다. Esc는 메뉴를 먼저 닫고, 메뉴가 없으면 플라이아웃을 닫습니다.

연결된 CPA 계정은 재로그인 메뉴가 없습니다. CPA가 토큰을 갱신하면 quota-baton이 최신 토큰을 다시 읽습니다.

플라이아웃의 계정 추가는 제공자별 로그인으로 시작합니다. 로그인 주소가 열리면 브라우저에서 인증을 끝냅니다. 로그인 세션은 정해진 시간이 지나면 만료됩니다. 만료되면 로그인을 다시 시작합니다.

트레이를 종료해도 코어는 계속 실행됩니다. 종료 메뉴에서 트레이만 종료하거나 트레이와 프록시를 함께 종료할 수 있습니다. 코어를 종료하면 진행 중인 요청의 drain 절차가 적용됩니다.

## 대시보드

트레이 아이콘을 두 번 누르거나 설정 메뉴의 `대시보드 열기`를 누릅니다. 이미 열려 있으면 그 창으로 이동합니다. 대시보드를 닫아도 트레이와 코어는 계속 실행됩니다. 창의 위치와 크기는 다음 실행에도 유지됩니다.

`세션`에서는 계정별 대화와 진행 중인 요청을 봅니다. 제공자와 시간 범위를 고르거나 폴더 이름으로 찾을 수 있습니다. 세션을 누르면 최근 요청과 계정 이동을 봅니다. 자세히 보기의 원본 ID 앞 8글자로 CLI의 `/status`와 대조할 수 있습니다. 그 밖의 화면에는 짧은 해시만 표시됩니다.

`기록`에서는 최근 7일의 한도, 계정 이동과 갱신 기록을 봅니다. `사용량`에서는 오늘 남은 양과 초기화 시각을 봅니다. 관측하지 못한 구간은 빈칸입니다. `—`는 확인 전이라는 뜻입니다. 코어를 다시 시작하면 메모리에만 있던 요청 수나 폴더 이름은 사라질 수 있습니다.

이력이 많으면 접근성 표에는 대표 시점만 표시할 수 있습니다. 이때 `일부 기록`이라고 알려 줍니다. 차트의 관측값은 표의 행 수에 맞춰 버리지 않습니다.

계정 변경은 대시보드에서 하지 않습니다. 다시 로그인이나 계정 추가 버튼을 누르면 트레이의 해당 작업으로 이동합니다. 연결이 끊기면 마지막 화면을 흐리게 보여 주고 다시 연결합니다.

폴더 이름은 마지막 이름만 메모리에 둡니다. 대화 내용은 읽지 않습니다. Claude 폴더를 찾을 때도 세션 파일의 이름만 확인합니다. 폴더 이름과 원본 ID의 앞부분은 로그나 기록 파일에 저장하지 않습니다. 기록과 창 설정의 위치는 [C12](contract.md#c12-persistence-formats), 관리 API는 [C11](contract.md#c11-admin-api)에 설명되어 있습니다.

Claude의 세션 파일 이름 조회를 끄려면 `config.toml`에 `[dashboard]`와 `resolve_claude_project = false`를 설정한 뒤 코어를 다시 시작합니다. 런처 라벨이 없는 세션은 짧은 세션 이름으로 표시됩니다.

## 계정 추가

### CPA 계정을 연결하는 방법

CPA의 인증 파일을 linked mode로 가져옵니다.

```powershell
quota-baton.exe accounts import cpa --dir <CPA-auths-folder> --dry-run
quota-baton.exe accounts import cpa --dir <CPA-auths-folder>
quota-baton.exe accounts import cpa --dir <CPA-auths-folder> --activate codex:<email>
quota-baton.exe accounts list
```

먼저 `--dry-run`으로 결과를 확인합니다. 가져온 계정은 기본적으로 비활성 상태입니다. `--activate provider:email`을 지정한 계정만 사용 상태로 바꿉니다.

linked account의 토큰 갱신은 CPA가 담당합니다. quota-baton은 CPA 파일에서 같은 계정의 최신 토큰을 읽습니다. CPA 파일이 갱신되지 않으면 계정은 `link_stale` 상태가 될 수 있습니다. 더 오래된 토큰을 다시 쓰지는 않습니다.

코어가 실행 중이면 import CLI는 인증된 관리 경로를 사용합니다. 코어가 멈춰 있으면 CLI가 저장소 lease를 직접 잡습니다.

### quota-baton이 직접 로그인하는 방법

소유 계정을 직접 로그인합니다.

```powershell
quota-baton.exe accounts login claude
quota-baton.exe accounts login codex
```

브라우저를 직접 열고 싶지 않으면 `--no-browser`를 사용합니다. 이미 linked account인 같은 계정을 직접 소유하도록 바꾸려면 명시적으로 `--replace-linked`를 지정합니다.

```powershell
quota-baton.exe accounts login claude --replace-linked
quota-baton.exe accounts login codex --replace-linked
```

소유 계정의 refresh token은 quota-baton이 관리합니다. 토큰 오류가 terminal 상태가 되면 트레이에서 다시 로그인합니다.

### CPA 연결 계정을 직접 갱신하도록 바꾸는 방법

`accounts list`에서 계정 ID를 확인합니다. 아래의 `<계정-ID>`를 그 값으로 바꿉니다.

```powershell
quota-baton.exe accounts adopt <계정-ID> --from cpa-file
quota-baton.exe accounts adopt <계정-ID> --from store
```

`cpa-file`은 같은 계정의 CPA 파일을 우선 읽습니다. 파일이 없으면 저장된 토큰을 사용합니다. `store`는 quota-baton에 저장된 토큰을 사용합니다. 백업을 지정하려면 `--from <백업-파일-또는-폴더>`를 씁니다. 오래된 토큰으로 되돌리지 않습니다.

CPA가 실행 중이고 해당 계정 파일을 읽을 수 있으면 이전을 거부합니다. 이전해도 계정 ID와 대화 연결은 유지됩니다. 이후에는 quota-baton이 직접 갱신합니다. 트레이의 계정 메뉴에서 `직접 갱신` 또는 `CPA 갱신`, 만료 시각과 최근 갱신 시각을 볼 수 있습니다.

직접 소유하거나 이전한 계정은 일시정지·비활성 상태에서도 토큰을 갱신합니다. 드레인 중이거나 재로그인이 필요한 계정은 자동 갱신하지 않습니다.

### 다른 앱의 계정을 가져오는 방법

원래 앱을 먼저 종료합니다. 트레이의 계정 추가 메뉴에서 `다른 앱에서 계정 가져오기`를 선택합니다. 제공자와 원래 앱의 홈 폴더를 고른 뒤 계정 정보와 경고를 확인합니다. 이미 등록된 계정이면 교체를 별도로 확인합니다. 같은 사용자의 기존 계정 ID와 대화 연결은 유지됩니다.

CLI에서는 확인부터 실행합니다. 다음 경로는 예시입니다.

```powershell
quota-baton.exe accounts takeover codex C:\example\codex-home
quota-baton.exe accounts takeover claude C:\example\claude-home
```

확인만 하면 종료 코드는 2입니다. 경고를 읽고 동의하면 같은 명령에 `--yes`를 붙입니다. 기존 계정 교체에는 `--replace`도 필요합니다. 실행 중인 코어가 모든 읽기와 저장을 담당합니다. API 키 로그인은 가져올 수 없습니다.

가져오면 quota-baton이 토큰을 갱신합니다. 원본 자격 파일은 날짜를 붙여 이름을 바꾸고, 토큰 갱신을 한 번 확인합니다. 원래 앱을 다시 쓰려면 그 앱에서 새로 로그인해야 합니다. 원본 파일이 최근 120초 안에 바뀌었거나 원래 앱이 감지되면 진행하지 않습니다. Claude의 사용자 정보는 원본 access token으로 OAuth 프로필을 조회해 확인합니다.

`--keep-source`는 원본 파일을 남깁니다. 원래 앱이 같은 토큰을 다시 갱신하면 양쪽 계정이 모두 풀릴 수 있습니다. 이 옵션에는 더 강한 경고가 표시됩니다. 응답이 끊긴 뒤 같은 요청을 다시 보내도 완료 기록을 재사용합니다. 앞선 갱신 결과를 알 수 없으면 자동으로 재전송하지 않으며 다시 로그인이 필요할 수 있습니다.

자세한 조건은 [C11 관리 API](contract.md#c11-admin-api)와 [D18 계정 가져오기](decisions.md#d18-take-over-an-account-from-another-apps-home-with-a-warning--accepted-user-direction-2026-10-19)를 따릅니다.

### 지금 토큰을 갱신하는 방법

만료가 가깝거나 갱신 확인이 필요하면 트레이 메뉴에 **토큰 지금 갱신**이 보입니다. 다음 명령도 쓸 수 있습니다. **사용량 새로고침**은 토큰 갱신과 다른 작업입니다. CPA 연결 계정은 CPA가 토큰을 갱신합니다.

```powershell
quota-baton.exe accounts refresh <계정-ID>
```

이전 갱신 결과가 불확실하면 일반 수동 갱신을 거부할 수 있습니다. 상태를 확인한 뒤 `--force`로 한 번 시도할 수 있습니다. 이미 무효화된 갱신 토큰이면 다시 로그인해야 합니다.

```powershell
quota-baton.exe accounts refresh <계정-ID> --force
```

코어가 실행 중이면 계정 이전과 수동 갱신 CLI는 관리 API를 사용합니다. 코어가 없으면 CLI가 계정 잠금을 직접 잡습니다. 자세한 조건은 [C11 관리 API](contract.md#c11-admin-api)와 [D17 갱신 소유권](decisions.md#d17)을 따릅니다.

## 속도 제한과 세션 분산

429에는 두 종류가 있습니다.

- 사용량 한도 429: 해당 사용량 창이 끝나거나 다른 계정으로 요청을 넘길 수 있습니다.
- 요청 속도 429: 계정이 너무 빠르게 호출되었다는 뜻입니다. quota-baton은 같은 계정에서 잠시 기다리고 요청 속도를 낮춥니다.

요청 속도 429만으로 대화 세션을 다른 계정으로 옮기지 않습니다. 계정 전환 후 캐시 유지가 보장되지 않고 문맥 재전송 비용이 커질 수 있기 때문입니다. 오래 지속되는 제한으로 한 계정에 세션이 과도하게 몰리고 다른 계정에 여유가 있으면, 다음 요청 경계에서 제한된 수의 세션을 다른 계정으로 분산할 수 있습니다.

새 세션은 사용량과 활성 세션 수를 함께 고려해 배치합니다. 이미 연결된 세션은 일반적인 사용량 변화만으로 이동하지 않습니다. 이 정책의 자세한 규칙은 [C6.3 속도 대기와 계정별 pacing](contract.md#c63-rate-hold-and-per-account-pacing-d14)과 [C7.1 세션 분산](contract.md#c71-session-spreading-and-overload-rebalance-d15)에 있습니다.

## 프로젝트·세션별 계정 유지

코어가 실행 중일 때 `quota-baton routing list`로 정책과 루트 세션 식별자를 확인합니다. 여기의 긴 식별자는 대시보드의 짧은 표시 ID와 다릅니다.

```powershell
quota-baton routing session <식별자> --mode strict --reset-wait-secs 0
quota-baton routing session <식별자> --mode recover
quota-baton routing session <식별자> --mode rebalance
quota-baton routing project "작업 프로젝트" --mode strict
quota-baton routing session <식별자> --project <프로젝트-ID> --inherit
```

`strict`는 현재 계정을 유지합니다. 그 계정을 사용할 수 없으면 다른 계정으로 옮기지 않고 멈춥니다. 캐시 적중이나 작업 성공을 보장하는 설정은 아닙니다. `recover`는 계정을 사용할 수 없을 때만 전환합니다. `rebalance`는 기존처럼 조건에 맞는 혼잡 분산도 허용합니다. 기본값은 `rebalance`입니다. 자식 에이전트는 루트의 정책을 따릅니다.

프로젝트는 이름과 별개의 ID로 등록됩니다. 같은 이름을 써도 다른 프로젝트입니다. 세션 연결은 명시적으로 지정하며 폴더 이름으로 추측하지 않습니다. 정책과 고정 계정은 재시작 뒤에도 유지됩니다. 프로젝트 기본값을 바꿔도 이미 연결한 세션을 조용히 바꾸지 않습니다. `--inherit`는 명시적 세션 설정을 해제하고 저장된 프로젝트 기본값을 따릅니다.

대기 시간 0은 초기화를 기다리지 않는다는 뜻입니다. 기본 대기 한도는 60초입니다. 추가 크레딧 사용 권한은 이 설정과 별개입니다. 크레딧을 금지하는 99% 예방 차단은 제공자의 추가 청구를 0으로 강제하는 금융 상한이 아닙니다.

## 클라이언트 설정 전환

전환 전에는 항상 dry-run을 실행합니다.

```powershell
quota-baton.exe wire
```

결과를 확인한 뒤 실제 변경을 승인할 때만 다음을 실행합니다.

```powershell
quota-baton.exe wire --apply
```

활성 연결 계정이 남아 있으면 CPA가 토큰을 갱신해야 합니다. 연결 결과와 `status`에 필요한 계정 수가 표시됩니다. 이때 `wire --disable-cpa-tasks`는 거절됩니다. 계정을 직접 갱신으로 이전하거나 직접 로그인으로 전환한 뒤 다시 확인하세요.

Codex 기본 홈이 ChatGPT 직접 로그인이거나 CPA 주소를 쓰지 않으면 `wire`는 별도 홈 `~/.codex-acct/qb`와 `codex-qb` 명령을 만듭니다. 기본 홈의 설정과 인증은 그대로 둡니다. 새 홈은 기본 설정을 복사하고 quota-baton 주소와 클라이언트 키를 넣습니다. 공용 agents·skills·hooks 등은 연결해서 사용합니다. 로그는 새 홈에 남깁니다.

```powershell
codex-qb
claude-qb
```

`codex-qb`로 시작한 세션만 quota-baton을 거칩니다. 기존 `codex`·`codex-ib`·`codex-mz`·`codex-cpa`는 바뀌지 않습니다.

Claude는 기본적으로 `claude-qb` 명령을 만듭니다. 클라이언트 키와 연결 주소는 사용자 전용 권한의 `~/.claude-qb/settings.json`에 둡니다. `claude-qb`는 이 설정을 지정해 실행합니다. 기존 `~/.claude/settings.json`과 `claude` 명령은 바뀌지 않습니다. Claude 설정 폴더와 대화 기록은 공유하므로 두 명령에서 이어 열 수 있습니다. 먼저 일반 `claude` 명령이 PATH에 설치되어 있어야 합니다.

두 실행기는 창이나 탭 제목을 `codex-qb · 현재 폴더` 또는 `claude-qb · 현재 폴더`로 표시합니다. Codex 전용 설정의 `[tui] terminal_title = []`와 Claude 전용 설정의 `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1`이 이 제목을 유지합니다.

이미 연결한 환경도 `wire`로 미리 본 뒤 `wire --apply`로 제목 설정을 갱신할 수 있습니다. 바뀔 전용 파일만 표시하고 교체합니다. 전용 설정의 다른 항목과 원래 되돌리기용 백업은 보존합니다.

직접 수정한 파일은 기본적으로 거절합니다. 내용을 확인한 뒤 `wire --apply --force`를 쓸 수 있습니다. 설정의 다른 항목은 유지하지만 실행기 본문은 다시 만듭니다. 연결 주소나 인증 값이 기존 연결 계획과 다르면 갱신을 중단합니다. 다른 파일로 교체된 경로는 `--force`도 덮어쓰지 않습니다.

기본 홈이 API key 방식으로 CPA에 연결된 예전 구성이라면, `wire`는 기존 방식대로 기본 홈의 연결 주소와 인증을 바꿉니다. 관련 모델 카탈로그와 CPA provider 주소도 바꿉니다. 원래 값은 journal에 보호해 둡니다.

모드를 직접 고를 수도 있습니다. 각 명령은 먼저 미리보기만 합니다.

```powershell
quota-baton.exe wire --codex-mode dedicated
quota-baton.exe wire --codex-mode default
quota-baton.exe wire --codex-mode none
quota-baton.exe wire --claude-mode dedicated
quota-baton.exe wire --claude-mode default
quota-baton.exe wire --claude-mode none
```

각 provider의 `dedicated`는 별도 연결 설정을, `default`는 기존 기본 설정을 바꿉니다. `none`은 해당 provider를 연결하지 않습니다. 두 모드를 따로 고를 수 있습니다. Claude에서 `default`를 명시하면 기존 `settings.json`의 `ANTHROPIC_BASE_URL`과 인증 값을 바꾸고 나머지 설정은 유지합니다.

기존 `.codex-acct` 홈은 덮어쓰지 않습니다. herdr 창에서 `codex-qb` 세션을 재시작 후 이어 열려면 그 세션 ID를 `session-homes.json`에 `~/.codex-acct/qb`로 등록해야 합니다. 이 파일은 자동으로 수정하지 않습니다.

전환을 되돌릴 때도 먼저 미리 봅니다.

```powershell
quota-baton.exe unwire
quota-baton.exe unwire --apply
```

별도 홈의 세션 기록(`~/.codex-acct/qb/sessions`)은 되돌린 뒤에도 남습니다. `unwire`는 직접 만든 파일과 연결만 변경 여부를 확인해 제거합니다.

사용자가 수정한 파일은 자동으로 덮어쓰지 않습니다. 변경 내용을 확인하고 force가 필요한 경우에만 별도 승인을 사용합니다. 전환은 [C11 관리 API](contract.md#c11-admin-api)와 [C12 저장 형식](contract.md#c12-persistence-formats)에 맞춰 journal과 함께 처리됩니다.

## 문제 해결

### 401

인증 토큰 문제입니다. 소유 계정은 먼저 자동 refresh를 시도합니다. 새 토큰으로도 401이면 트레이에서 다시 로그인합니다. linked account는 CPA 파일을 다시 읽습니다. 최신 토큰이 없으면 `link_stale`로 표시됩니다.

### 403

해당 모델 또는 조직에서 사용이 거부되었다는 뜻일 수 있습니다. quota-baton은 다른 계정을 시도할 수 있습니다. 같은 모델에 대한 권한 문제는 그 모델에 한해 계정을 잠시 제외합니다.

### 429

사용량 한도라면 리셋 시각을 기다리거나 다른 계정으로 넘깁니다. 요청 속도 한도라면 같은 계정에서 rate hold와 pacing을 적용합니다. 요청 속도 429만으로 세션을 옮기지는 않습니다.

### 로그와 ledger

다음 폴더를 확인합니다.

- 로그: `%LOCALAPPDATA%\quota-baton\logs\`
- 요청 metadata ledger: `%LOCALAPPDATA%\quota-baton\ledger\YYYY-MM-DD.jsonl`
- 사용량 상태: `%LOCALAPPDATA%\quota-baton\state\quota.json`
- 세션 연결 상태: `%LOCALAPPDATA%\quota-baton\state\bindings.json`

로그와 ledger에는 요청 본문, 토큰, 키를 기록하지 않습니다. 파일을 다른 사람에게 보낼 때도 개인 경로와 계정 식별자를 먼저 확인합니다.

### 코어 교체와 takeover

새 코어를 실행할 때 `--takeover`를 사용합니다.

```powershell
quota-baton.exe serve --takeover
```

새 코어가 실행 중인 코어에 drain을 요청합니다. 기존 코어는 새 refresh와 새 저장을 멈추고, binding을 flush한 뒤 credential lease를 넘깁니다. 새 코어는 계정과 binding을 읽고 포트를 이어받습니다. 진행 중인 HTTP stream은 끝까지 처리합니다. WebSocket은 현재 응답 뒤 정상적으로 닫힙니다. 기존 코어가 살아 있을 때 강제로 종료하지 마세요.

takeover와 종료 규칙은 [C6 계정 상태와 결과 처리](contract.md#c6-upstream-outcome-mapping)와 [아키텍처 14장 수명주기](architecture.md#14-lifecycle-and-upgrades)에 설명되어 있습니다.

## 자세한 규칙

- 요청과 응답의 원문 보존: [C3 헤더](contract.md#c3-header-rules), [C4 본문 불변성](contract.md#c4-body-invariance)
- 계정 선택과 세션 고정: [C7 선택 정책](contract.md#c7-selection-policy)
- 사용량 polling: [C9 사용량 신호](contract.md#c9-usage-signals)
- 트레이 설계: [아키텍처 10장](architecture.md#10-tray)
- 파일 위치와 로그: [아키텍처 12장](architecture.md#12-persistence-layout-localappdataquota-baton)

## 전환 대기와 관측용 예측

초기화 대기, 인증 갱신, 연결과 재시도에는 하나의 전송 전 시간 한도가 적용됩니다. `routing.request_timeout_secs`의 기본값은 300초입니다. WebSocket 연결은 별도로 `routing.ws_upgrade_hold_secs`(기본 10초) 안에 끝냅니다. 이미 시작한 정상 응답 스트림을 이 시간 때문에 잘라내지는 않습니다.

속도 제한의 대기 한도가 끝났다고 요청을 상류에 강제로 보내지 않습니다. 받은 429가 있으면 그대로 돌려줍니다. 전송 전부터 기다리던 요청은 로컬 429를 받습니다. 열린 WebSocket의 미전송 요청은 1013 종료로 다시 요청할 수 있게 합니다. 이 동작만으로 계정의 구독 사용량이 소진됐다고 표시하지 않습니다.

`routing.forecast_shadow = true`는 소비 속도의 관측용 비교를 켭니다. 기본값은 꺼짐입니다. 결과는 `routing list`의 `forecast_shadow`에서 확인합니다. 이 결과는 계정을 배정하거나 전환하지 않습니다. 관측이 부족하거나 오래됐으면 예측하지 않습니다. 표시된 상·하한은 보정 전 시나리오이며 성공 확률이나 비용 상한이 아닙니다. 90% 배정 기준은 아직 유지됩니다.

프로젝트 연결만 추가하면 기존의 명시적 세션 정책을 유지합니다. 그 정책을 해제하려는 경우에만 `--inherit`를 함께 사용합니다. 이동 허용은 추가 유료 크레딧 사용 허용과 별개입니다.
