//! One native DPI seam. Release/default always query the OS, without environment access.
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::GetDpiForWindow;

#[cfg(all(debug_assertions, feature = "testkit"))]
#[path = "dpi/testkit.rs"]
mod implementation;
#[cfg(not(all(debug_assertions, feature = "testkit")))]
#[path = "dpi/native.rs"]
mod implementation;

#[cfg(all(debug_assertions, feature = "testkit"))]
pub(super) use implementation::proof;
pub(super) use implementation::{effective, initialize, record_target};

/// Actual monitor/window DPI remains independently observable in synthetic scaled runs.
pub(super) fn actual(hwnd: HWND) -> u32 {
    unsafe { GetDpiForWindow(hwnd) }
}
