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

아이콘을 왼쪽 또는 오른쪽 클릭하면 플라이아웃이 열립니다. 플라이아웃에는 코어 상태, 버전, 제공자별 계정, 요금제, 상태 칩, 5시간 및 주간 사용량 막대, 다음 리셋 시각이 표시됩니다.

계정의 상태 칩은 사용 중, 준비됨, 제한 중, 재로그인 필요, 일시정지, 비활성화 등을 나타냅니다. 계정 행의 메뉴에서 다음 작업을 할 수 있습니다.

- 다시 로그인
- 일시정지 또는 재개
- 비활성화 또는 활성화
- 새 대화 받지 않기 설정을 켜거나 끄기
- Codex 유료 크레딧 사용 허용 설정을 켜거나 끄기
- 사용량 새로고침
- 계정 삭제

플라이아웃 아래쪽 메뉴에는 계정 추가, 설정 폴더 열기, 로그 열기, 종료가 있습니다. 종료를 누르면 트레이만 종료할지 코어도 함께 종료할지 선택합니다.

연결된 CPA 계정은 재로그인 메뉴가 없습니다. CPA가 토큰을 갱신하면 quota-baton이 최신 토큰을 다시 읽습니다.

플라이아웃의 계정 추가는 제공자별 로그인으로 시작합니다. 로그인 주소가 열리면 브라우저에서 인증을 끝냅니다. 로그인 세션은 정해진 시간이 지나면 만료됩니다. 만료되면 로그인을 다시 시작합니다.

트레이를 종료해도 코어는 계속 실행됩니다. 종료 메뉴에서 트레이만 종료하거나 트레이와 프록시를 함께 종료할 수 있습니다. 코어를 종료하면 진행 중인 요청의 drain 절차가 적용됩니다.

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

## 속도 제한과 세션 분산

429에는 두 종류가 있습니다.

- 사용량 한도 429: 해당 사용량 창이 끝나거나 다른 계정으로 요청을 넘길 수 있습니다.
- 요청 속도 429: 계정이 너무 빠르게 호출되었다는 뜻입니다. quota-baton은 같은 계정에서 잠시 기다리고 요청 속도를 낮춥니다.

요청 속도 429만으로 대화 세션을 다른 계정으로 옮기지 않습니다. 대화 캐시를 잃고 전체 문맥을 다시 보내게 되기 때문입니다. 오래 지속되는 제한으로 한 계정에 세션이 과도하게 몰리고 다른 계정에 여유가 있으면, 다음 요청 경계에서 제한된 수의 세션을 다른 계정으로 분산할 수 있습니다.

새 세션은 사용량과 활성 세션 수를 함께 고려해 배치합니다. 이미 연결된 세션은 일반적인 사용량 변화만으로 이동하지 않습니다. 이 정책의 자세한 규칙은 [C6.3 속도 대기와 계정별 pacing](contract.md#c63-rate-hold-and-per-account-pacing-d14)과 [C7.1 세션 분산](contract.md#c71-session-spreading-and-overload-rebalance-d15)에 있습니다.

## 클라이언트 설정 전환

전환 전에는 항상 dry-run을 실행합니다.

```powershell
quota-baton.exe wire
```

결과를 확인한 뒤 실제 변경을 승인할 때만 다음을 실행합니다.

```powershell
quota-baton.exe wire --apply
```

전환은 다음 항목을 바꿉니다.

- Codex 기본 홈의 `openai_base_url`을 quota-baton으로 바꿉니다.
- Codex의 관련 모델 카탈로그 주소와 CPA provider 연결 주소를 quota-baton 주소로 바꿉니다.
- Claude 설정의 `ANTHROPIC_BASE_URL`과 인증 값을 quota-baton에 맞춥니다.
- 기존 provider id와 그 밖의 설정은 유지합니다.
- 원래 값은 되돌리기 위한 journal에 보호해 둡니다.

`.codex-acct` 홈은 herdr shim의 예외 대상입니다. 이 홈은 전환으로 덮어쓰지 않습니다. `session-homes.json`에 남은 ChatGPT 세션 홈은 dry-run 경고를 확인한 뒤 별도로 처리합니다.

전환을 되돌릴 때도 먼저 미리 봅니다.

```powershell
quota-baton.exe unwire
quota-baton.exe unwire --apply
```

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
