//! Strict production ancestor policy: mount flags cannot authorize broad modes.
use super::trusted_ancestor_policy;

#[test]
fn root_owned_0755_ancestor_is_accepted() {
    assert!(trusted_ancestor_policy(true, 0, 1000, 0o755));
}
#[test]
fn effective_uid_owned_0755_ancestor_is_accepted() {
    assert!(trusted_ancestor_policy(true, 1000, 1000, 0o755));
}
#[test]
fn root_or_effective_uid_owned_0777_is_refused_without_mount_exception() {
    assert!(!trusted_ancestor_policy(true, 0, 1000, 0o777));
    assert!(!trusted_ancestor_policy(true, 1000, 1000, 0o777));
}
#[test]
fn group_or_world_writable_ancestor_is_refused() {
    assert!(!trusted_ancestor_policy(true, 0, 1000, 0o775));
    assert!(!trusted_ancestor_policy(true, 0, 1000, 0o757));
}
#[test]
fn foreign_owner_is_refused_even_with_private_mode() {
    assert!(!trusted_ancestor_policy(true, 2000, 1000, 0o700));
}
#[test]
fn non_directory_is_refused_even_with_trusted_uid_and_mode() {
    assert!(!trusted_ancestor_policy(false, 0, 1000, 0o755));
}
#[test]
fn sticky_writable_tmp_ancestor_is_refused() {
    assert!(!trusted_ancestor_policy(true, 0, 1000, 0o1777));
}
#[test]
fn private_or_readonly_permission_bits_on_trusted_directory_are_accepted() {
    assert!(trusted_ancestor_policy(true, 1000, 1000, 0o700));
    assert!(trusted_ancestor_policy(true, 1000, 1000, 0o500));
}
