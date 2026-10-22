//! Client presentation copy only; protocol codes and account metadata stay unchanged.
pub(crate) const STRINGS: &[(&str, &str)] = &[
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
