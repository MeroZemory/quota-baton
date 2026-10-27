// Validate the native login-start trailer separately from the exact account table.
pub(super) fn status_without_platform_trailer(status: &str) -> &str {
    let (table, trailer) = status
        .strip_suffix('\n')
        .unwrap()
        .rsplit_once('\n')
        .unwrap();
    let state = trailer.strip_prefix("로그인 시 자동 시작: ").unwrap();
    assert!(
        ["등록됨", "등록 안 됨"].iter().any(|expected| {
            state == *expected
                || state
                    .strip_prefix(*expected)
                    .is_some_and(|details| details.starts_with(" (") && details.ends_with(')'))
        }),
        "unexpected login-start status: {trailer:?}"
    );
    assert!(!trailer.chars().any(char::is_control));
    &status[..table.len() + 1]
}
