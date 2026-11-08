//! Ordinary path: preserve the incumbent per-window OS query and clamp exactly.
use windows::Win32::{Foundation::HWND, Graphics::Direct2D::ID2D1HwndRenderTarget};

pub(crate) fn initialize(_prepared_fixture: bool) -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn effective(hwnd: HWND) -> u32 {
    super::actual(hwnd).max(96)
}
pub(crate) fn record_target(_hwnd: HWND, _target: &ID2D1HwndRenderTarget) {}
