//! Native shell additions to the shared Korean/English catalog.
pub(crate) const STRINGS: &[(&str, &str)] = &[
    (
        "연결을 확인하며 기존 로그인 상태를 다시 확인하고 있습니다. 새 요청은 만들지 않습니다.",
        "Checking the connection and the existing sign-in request. No new request will be created.",
    ),
    (
        "로그인 시간이 만료되었습니다. 다시 시도하거나 로그인 방법을 변경해 주세요.",
        "Sign-in expired. Try again or change the sign-in method.",
    ),
    ("Codex 계정 추가", "Add Codex account"),
    (
        "다른 계정의 로그인이 완료되었습니다. 선택한 계정의 재로그인은 확인되지 않았습니다.",
        "Another account signed in. Re-login of the selected account was not confirmed.",
    ),
    (
        "로그인은 완료되었지만 선택한 계정의 결과는 확인하지 못했습니다. 계정 목록을 확인해 주세요.",
        "Sign-in completed, but the selected account's result could not be confirmed. Check the account list.",
    ),
    ("로그인 완료 계정: {0}", "Signed-in account: {0}"),
    ("웹 브라우저로 로그인", "Browser sign-in"),
    (
        "이 PC의 브라우저에서 로그인합니다.",
        "Sign in using this PC’s browser.",
    ),
    ("디바이스 코드로 로그인", "Device code sign-in"),
    (
        "표시된 코드를 브라우저에서 입력합니다. 다른 기기에서도 가능합니다.",
        "Enter the displayed code in a browser. You can use another device.",
    ),
    ("로그인 요청 중…", "Starting sign-in…"),
    (
        "이전 로그인 종료를 확인하는 중…",
        "Confirming the previous sign-in has ended…",
    ),
    ("로그인이 완료되었습니다.", "Sign-in completed."),
    (
        "로그인 요청 결과를 확인하지 못했습니다. 새 요청을 만들 수 없습니다. 코어 연결과 기존 로그인 요청이 종료되었는지 확인해 주세요.",
        "Could not confirm the sign-in request. A new request is disabled. Check the core connection and resolve the existing sign-in request.",
    ),
    (
        "브라우저에서 코드를 입력해 주세요.",
        "Enter the code in your browser.",
    ),
    (
        "브라우저에서 로그인을 완료해 주세요.",
        "Complete sign-in in your browser.",
    ),
    ("남은 시간 {0}초", "Time remaining: {0}s"),
    ("코드 복사됨", "Code copied"),
    ("코드 복사", "Copy code"),
    ("인증 페이지 열기", "Open sign-in page"),
    ("브라우저 다시 열기", "Reopen browser"),
    (
        "브라우저를 열지 못했습니다. 주소를 복사하거나 다시 열어 주세요.",
        "Could not open the browser. Copy the address or open it again.",
    ),
    (
        "로그인 시간이 만료되었습니다. 새 코드를 명시적으로 요청해 주세요.",
        "Sign-in expired. Request a new code to continue.",
    ),
    ("로그인이 취소되었습니다.", "Sign-in cancelled."),
    (
        "로그인 요청을 처리하지 못했습니다. 디바이스 로그인이 허용되어 있는지 Security 설정이나 워크스페이스 관리자에게 확인하세요.",
        "Could not complete sign-in. For device sign-in, check Security settings or ask your workspace administrator whether device sign-in is allowed.",
    ),
    (
        "로그인 요청이 너무 많습니다. 잠시 후 직접 다시 시도하거나 웹 로그인을 선택해 주세요.",
        "Too many sign-in requests. Try again later or choose browser sign-in.",
    ),
    ("새 코드 요청", "Request new code"),
    ("로그인 방법 변경", "Change method"),
    ("선택한 계정: {0}", "Selected account: {0}"),
    ("남은 시간 {}", "Time remaining {}"),
    ("활성 {}{}", "Active {}{}"),
    ("quota-baton 대시보드", "quota-baton dashboard"),
    (
        "quota-baton · 코어에 연결할 수 없음",
        "quota-baton · Core unavailable",
    ),
    ("변경 중…", "Updating…"),
    ("로그인 시간이 만료되었습니다", "Sign-in expired"),
    ("로그인하지 못했습니다", "Could not sign in"),
    (
        "브라우저에서 로그인을 마치세요",
        "Finish signing in in your browser",
    ),
    ("{} 계정 추가", "Add {} account"),
    (" · 켜짐", " · On"),
    (" · 꺼짐", " · Off"),
    ("브라우저를 열지 못했습니다.", "Could not open the browser."),
    (
        "코어를 종료하지 못했습니다. 트레이를 유지합니다. 다시 시도하세요.",
        "Could not stop the core. The tray will stay open. Try again.",
    ),
    (
        "최근 1시간 최소 {} 토큰",
        "At least {} tokens in the last hour",
    ),
    (
        "트레이에서 해당 계정의 다시 로그인을 누르세요.",
        "Choose Sign in again for this account in the tray.",
    ),
    (
        "트레이에서 이 계정의 \"다시 로그인\"을 누르세요.",
        "Choose Sign in again for this account in the tray.",
    ),
    ("계정 한도 도달", "Account limit reached"),
    (
        "트레이에서 한도 초기화 시간을 확인하거나 꺼진 계정을 켜세요.",
        "Check the reset time in the tray or enable another account.",
    ),
    ("사용 가능한 계정 없음", "No available accounts"),
    (
        "해당 제공자의 계정 상태를 확인하고 계정을 추가하거나 켜세요.",
        "Check this provider's accounts, then add or enable an account.",
    ),
    (
        "해당 계정의 조직 접근 권한을 확인하세요.",
        "Check this account's organization access.",
    ),
    (
        "트레이에서 이 계정의 결제 상태를 확인하세요.",
        "Check this account's billing status in the tray.",
    ),
    ("코어 다시 시작", "Core restarted"),
    (
        "예기치 않게 종료된 코어를 다시 시작했습니다. 계정 상태를 확인하세요.",
        "The core restarted after an unexpected exit. Check account status.",
    ),
    ("반복 계정 이동 차단", "Repeated account moves blocked"),
    (
        "기존 계정에서 대기합니다. 대시보드에서 이동 기록을 확인하세요.",
        "Waiting on the current account. Check move history in the dashboard.",
    ),
    (
        "계정 이동 토큰 비용 증가",
        "Account move token cost increased",
    ),
    (
        "계정 이동으로 문맥을 다시 읽는 비용이 늘었습니다. 대시보드의 측정값과 미측정 건수를 확인하세요.",
        "Account moves increased context re-read costs. Check measured costs and unknown counts in the dashboard.",
    ),
    (
        "가져오는 중입니다. 원본 파일을 정리하고 토큰 갱신을 확인합니다.",
        "Importing the account and checking token refresh.",
    ),
    (
        "계정 정보를 확인하고 있습니다. 잠시 기다리세요.",
        "Checking account details. Please wait.",
    ),
    (
        "\n기존 계정을 교체합니다. 계정 ID, 연결 및 기록은 유지됩니다.",
        "\nThe existing account will be replaced. Its ID, bindings and history stay.",
    ),
    (
        "가져오기를 마쳤고 토큰 갱신을 확인했습니다.",
        "Account imported. Token refresh verified.",
    ),
    (
        "가져왔지만 토큰 갱신을 확인하지 못했습니다. 계정 상태를 확인하세요.",
        "Account imported. Check its status because token refresh could not be verified.",
    ),
    (
        "가져오기를 확인하지 못했습니다. 계정 목록과 원본 폴더를 확인하세요. 앱 실행, 최근 파일 변경 또는 기존 계정 충돌이면 다시 검사하세요. 자동 재시도하지 않습니다.",
        "Could not verify the import. Check the accounts and source folder, then inspect again. No automatic retry.",
    ),
    (
        "가져올 계정의 홈 폴더 선택",
        "Choose the account home folder",
    ),
    ("차트", "Chart"),
];
