//! Capture the actual HWND's current client frame with a same-process GDI DC.
//! Keep HDC creation and Direct2D BindDC in the same process.
use std::path::Path;
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    },
    UI::WindowsAndMessaging::{GetClientRect, SendMessageW, WM_PRINTCLIENT},
};

pub fn save(hwnd: HWND, path: &Path) -> anyhow::Result<()> {
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect)?;
        let (width, height) = (rect.right, rect.bottom);
        anyhow::ensure!(width > 0 && height > 0 && width <= 4096 && height <= 4096);
        let dc = GetDC(Some(hwnd));
        anyhow::ensure!(!dc.is_invalid(), "owned HWND GetDC failed");
        let memory = CreateCompatibleDC(Some(dc));
        let bitmap = CreateCompatibleBitmap(dc, width, height);
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(!memory.is_invalid() && !bitmap.is_invalid());
            let old = SelectObject(memory, bitmap.into());
            anyhow::ensure!(!old.is_invalid());
            let painted = SendMessageW(
                hwnd,
                WM_PRINTCLIENT,
                Some(WPARAM(memory.0 as usize)),
                Some(LPARAM(4)), // PRF_CLIENT
            );
            SelectObject(memory, old);
            anyhow::ensure!(painted.0 == 0, "actual HWND client painting failed");
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            anyhow::ensure!(
                GetDIBits(
                    memory,
                    bitmap,
                    0,
                    height as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS
                ) == height
            );
            std::fs::write(path, pixels)?;
            Ok(())
        })();
        if !bitmap.is_invalid() {
            let _ = DeleteObject(bitmap.into());
        }
        if !memory.is_invalid() {
            let _ = DeleteDC(memory);
        }
        ReleaseDC(Some(hwnd), dc);
        result
    }
}
