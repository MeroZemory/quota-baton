//! System app theme. Invoke from the tray worker, then publish the cached value.
use windows::{
    Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    core::w,
};

#[cfg(all(debug_assertions, feature = "testkit"))]
static TEST_THEME: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(all(debug_assertions, feature = "testkit"))]
pub fn set_test_dark(dark: bool) {
    TEST_THEME.store(
        if dark { 2 } else { 1 },
        std::sync::atomic::Ordering::Release,
    );
}

fn dark_value(value: Option<u32>) -> bool {
    value == Some(0)
}

pub fn system_dark() -> bool {
    #[cfg(all(debug_assertions, feature = "testkit"))]
    match TEST_THEME.load(std::sync::atomic::Ordering::Acquire) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    #[cfg(all(debug_assertions, feature = "testkit"))]
    if let Ok(value) = std::env::var("QB_TRAY_TEST_THEME") {
        match value.as_str() {
            "dark" => return true,
            "light" => return false,
            _ => {}
        }
    }
    let mut value = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    dark_value((result.is_ok() && size == 4).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_theme_uses_dark_only_for_explicit_zero() {
        assert!(dark_value(Some(0)));
        for value in [None, Some(1), Some(2)] {
            assert!(!dark_value(value));
        }
    }
}
