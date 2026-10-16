//! Manual Windows acceptance harness for M6c.
//!
//! It is ignored by default because it starts two real GUI processes. Set
//! `M6C_CORE_BIN`, `M6C_TRAY_BIN`, and `M6C_HOME` to synthetic test fixtures
//! before running it. No real profile is ever selected by this test.

#![cfg(windows)]

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{
            BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY,
            SelectObject,
        },
        Storage::Xps::PrintWindow,
        UI::WindowsAndMessaging::{
            FindWindowW, GetWindowRect, PostMessageW, WM_LBUTTONUP, WM_RBUTTONUP,
        },
    },
    core::w,
};

fn window() -> Option<HWND> {
    unsafe { FindWindowW(None, w!("quota-baton")).ok() }
}

fn click(hwnd: HWND, x: i16, y: i16, message: u32) {
    let packed = ((y as u16 as u32) << 16) | x as u16 as u32;
    unsafe {
        let _ = PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(packed as isize));
    }
}

fn capture(hwnd: HWND, path: &std::path::Path) -> anyhow::Result<()> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect)?;
    }
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let screen = unsafe { GetDC(None) };
    anyhow::ensure!(!screen.0.is_null(), "GetDC failed");
    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    anyhow::ensure!(
        !memory.0.is_null() && !bitmap.0.is_null(),
        "bitmap allocation failed"
    );
    unsafe {
        let _old = SelectObject(memory, bitmap.into());
        anyhow::ensure!(
            PrintWindow(hwnd, memory, Default::default()).as_bool()
                || BitBlt(
                    memory,
                    0,
                    0,
                    width,
                    height,
                    Some(screen),
                    rect.left,
                    rect.top,
                    SRCCOPY
                )
                .is_ok(),
            "window capture failed"
        );
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
            ) == height,
            "GetDIBits failed"
        );
        let image_size = pixels.len() as u32;
        let file_size = 14 + 40 + image_size;
        let mut bytes = Vec::with_capacity(file_size as usize);
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&file_size.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&(54u32).to_le_bytes());
        bytes.extend_from_slice(&(40u32).to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&(-height).to_le_bytes());
        bytes.extend_from_slice(&(1u16).to_le_bytes());
        bytes.extend_from_slice(&(32u16).to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&image_size.to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&pixels);
        fs::write(path, bytes)?;
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        let _ = ReleaseDC(None, screen);
    }
    Ok(())
}

struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
#[ignore = "starts a real Windows core and tray; run explicitly with synthetic M6C_* variables"]
fn m6c_live_spawned_core_clicks_and_captures() -> anyhow::Result<()> {
    let core_bin = std::env::var_os("M6C_CORE_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("M6C_CORE_BIN is required"))?;
    let tray_bin = std::env::var_os("M6C_TRAY_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("M6C_TRAY_BIN is required"))?;
    let home = std::env::var_os("M6C_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("M6C_HOME is required"))?;
    let evidence =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/reports/M6c-evidence");
    fs::create_dir_all(&evidence)?;
    let core = Command::new(core_bin)
        .env("QB_HOME", &home)
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let tray = Command::new(tray_bin)
        .env("QB_HOME", &home)
        .args([
            "--no-supervise",
            "--open-flyout-live",
            "--now",
            "2030-01-01T00:00:00Z",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let _children = Children(vec![core, tray]);
    let hwnd = (0..100)
        .find_map(|_| {
            let found = window();
            if found.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            found
        })
        .ok_or_else(|| anyhow::anyhow!("flyout window not found"))?;
    capture(hwnd, &evidence.join("list.bmp"))?;
    click(hwnd, 330, 145, WM_RBUTTONUP);
    thread::sleep(Duration::from_millis(100));
    capture(hwnd, &evidence.join("row-menu.bmp"))?;
    click(hwnd, 220, 180, WM_LBUTTONUP);
    thread::sleep(Duration::from_millis(100));
    capture(hwnd, &evidence.join("login-or-action.bmp"))?;
    Ok(())
}
