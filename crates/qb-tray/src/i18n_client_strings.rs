//! Client presentation copy only; protocol codes and account metadata stay unchanged.
pub(crate) const STRINGS: &[(&str, &str)] = &[
    ("로그인 요청이 올바르지 않습니다", "Invalid sign-in request"),
    (
        "로그인 응답을 확인할 수 없습니다",
        "Could not confirm the sign-in response",
    ),
    ("먼저 다시 로그인해 주세요.", "Sign in again first."),
    ("계정을 먼저 켜 주세요.", "Enable this account first."),
    (
        "계정 상태를 먼저 확인해 주세요.",
        "Check the account status first.",
    ),
    ("리셋권 사용 중…", "Using reset credit…"),
    ("기록된 만료 {0}", "Recorded expiry {0}"),
    ("리셋권 · 확인 전", "Reset credits · not observed"),
    ("리셋권을 새로고침해 주세요.", "Refresh reset credits."),
    ("리셋권 · 지원 안 함", "Reset credits · unsupported"),
    ("만료 없음", "Never expires"),
    ("만료 확인 전", "Expiry unknown"),
    (
        "리셋권 · 오래된 정보 · 제공자 보고 {0}",
        "Reset credits · stale · provider reported {0}",
    ),
    ("리셋권 0개", "Reset credits: 0"),
    (
        "리셋권 {0}개 · 가장 빠른 만료 {1}",
        "Reset credits: {0} · earliest expiry {1}",
    ),
    (
        "리셋권 · 제공자 보고 {0} · 상세·만료 확인 필요",
        "Reset credits · provider reported {0} · details/expiry unverified",
    ),
    (
        "수량·자격·만료를 확인하려면 새로고침하세요.",
        "Refresh to verify count, eligibility and expiry.",
    ),
    ("확인 시각 {0}", "Observed {0}"),
    (
        "만료가 가장 빠른 리셋권 1개를 사용합니다.",
        "Use one reset credit with the earliest verified expiry.",
    ),
    (
        "리셋권 정보가 변경되었거나 만료되었습니다. 새 미리보기를 열어 주세요.",
        "Reset-credit information changed or expired. Open a new preview.",
    ),
    ("사용 가능한 리셋권이 없습니다.", "No usable reset credits."),
    (
        "리셋권 상세 정보가 불완전하여 가장 빠른 만료를 확인할 수 없습니다.",
        "Reset-credit details are incomplete; earliest expiry cannot be verified.",
    ),
    (
        "리셋권 사용 자격을 확인하지 못했습니다.",
        "Reset-credit eligibility could not be verified.",
    ),
    (
        "리셋권 만료를 확인하지 못했습니다.",
        "Reset-credit expiry could not be verified.",
    ),
    (
        "리셋권 정보가 올바르지 않습니다. 새로고침하세요.",
        "Reset-credit information is invalid. Refresh it.",
    ),
    (
        "사용 결과를 확인하지 못했습니다. 상태를 다시 확인해 주세요.",
        "Could not confirm the outcome. Refresh the status before using another credit.",
    ),
    (
        "리셋권을 확인하지 못했습니다. 새로고침하세요.",
        "Could not check reset credits. Refresh the status.",
    ),
    (
        "리셋권 응답이 너무 큽니다",
        "Reset-credit response is too large",
    ),
    ("계정 번호가 올바르지 않습니다", "Invalid account ID"),
    (
        "리셋권을 사용했습니다. 사용량·리셋권 정보는 새로고침이 필요합니다.",
        "Reset credit used. Usage and reset-credit information need refreshing.",
    ),
    ("리셋권 1개를 사용했습니다.", "Used one reset credit."),
    (
        "이 리셋권은 이미 사용되었습니다.",
        "This reset credit was already used.",
    ),
    (
        "초기화할 Codex 한도가 없습니다.",
        "No eligible Codex limits to reset.",
    ),
    ("리셋권 확인 중…", "Checking reset credits…"),
    ("리셋권 새로고침", "Refresh reset credits"),
    ("리셋권 사용…", "Use reset credit…"),
    ("리셋권 1개 사용", "Use one reset credit"),
    ("1개 사용", "Use one"),
    ("리셋권 · 계정 메뉴…", "Reset credits · account menu…"),
    (
        "{0}\n{1}\n만료 {2}\n영향: 해당되는 Codex 한도\n만료가 가장 빠른 리셋권 1개를 사용합니다.\n확인 유효 시각 {3}",
        "{0}\n{1}\nExpiry {2}\nEffect: eligible Codex limits\nUse one reset credit with the earliest verified expiry.\nConfirmation valid until {3}",
    ),
    ("세션 번호가 올바르지 않습니다", "Invalid session ID"),
    (
        "대시보드 세션 응답이 요청과 다릅니다",
        "Session response does not match the requested session",
    ),
    (
        "기록 조회 기간이 올바르지 않습니다",
        "Invalid activity range",
    ),
    (
        "대시보드 응답 형식이 올바르지 않습니다",
        "Invalid dashboard response",
    ),
    (
        "대시보드 응답이 너무 큽니다",
        "Dashboard response is too large",
    ),
    (
        "계정 메뉴에서 다시 로그인하세요",
        "Sign in again from the account menu",
    ),
    (
        "갱신 응답을 확인할 수 없습니다",
        "Could not verify refresh response",
    ),
    (
        "CPA가 갱신하는 계정입니다",
        "This account is refreshed by the integration",
    ),
    (
        "드레인 해제 후 다시 시도하세요",
        "Turn off drain and try again",
    ),
    (
        "이전 갱신 결과가 불확실합니다 · CLI의 --force를 확인하세요",
        "Previous refresh result is uncertain · Check CLI --force",
    ),
    ("잠시 후 다시 갱신해 주세요", "Try refreshing again later"),
    ("계정을 찾을 수 없습니다", "Account not found"),
    ("잠시 후 다시 시도해 주세요", "Try again shortly"),
    ("요청을 처리하지 못했습니다", "Request failed"),
    (
        "코어 응답을 읽지 못했습니다",
        "Could not read core response",
    ),
    (
        "코어 health가 false입니다 ({})",
        "Core health is false ({})",
    ),
    (
        "제공자: {}\n이메일: {}\n요금제: {}\n제공자 계정 ID: {}\n원본 경로: {}\n마지막 갱신: {}\n파일 수정 시각: {}\n원래 앱 실행 중: {}{}",
        "Provider: {}\nEmail: {}\nPlan: {}\nProvider account ID: {}\nSource path: {}\nLast refresh: {}\nFile modified: {}\nOriginal app running: {}{}",
    ),
    (
        "예 · 먼저 종료하고 다시 검사하세요",
        "Yes · Close it and check again",
    ),
    ("감지되지 않음 (최선의 확인)", "Not detected (best effort)"),
    (
        "\n기존 계정: {} · 다음 단계에서 교체 확인이 필요합니다",
        "\nExisting account: {} · Confirm replacement in the next step",
    ),
    (
        "가져오면 quota-baton이 이 계정의 토큰을 관리합니다. 원래 앱({})은 로그아웃되며, 다시 쓰려면 그 앱에서 새로 로그인해야 합니다. 원래 앱이 켜져 있으면 먼저 종료하세요.",
        "Importing lets quota-baton manage this account's tokens. The original app ({}) will be signed out. To use it again, sign in there. Close the original app before continuing.",
    ),
];
