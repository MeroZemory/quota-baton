#[cfg(windows)]
#[path = "m7b/windows_cases.rs"]
mod windows_cases;

use qb_core::guard::{MemoryGuard, MemoryGuardTransition};

#[tokio::test]
async fn memory_guard_test_hook_has_hysteresis() {
    let guard = MemoryGuard::new(50 * 1024 * 1024);
    assert_eq!(
        guard.force_sample(50 * 1024 * 1024 + 1).transition,
        MemoryGuardTransition::On
    );
    assert!(guard.is_guarded());
    assert_eq!(
        guard.force_sample(45 * 1024 * 1024 - 1).transition,
        MemoryGuardTransition::Off
    );
    assert!(!guard.is_guarded());
}
