use super::LoginMethod;

// Release builds never trust local authorization pages or environment overrides.
pub(super) fn allowed(_: &str, _: LoginMethod, _: &str) -> bool {
    false
}
