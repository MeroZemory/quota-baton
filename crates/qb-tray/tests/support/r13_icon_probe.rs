//! Borrow HICONs; own only GetIconInfo bitmap copies and capture DCs/bitmaps.
use anyhow::{Result, ensure};
use std::{ffi::c_void, fs::OpenOptions, io::Write, mem::size_of, path::Path, ptr};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{BITMAPINFO, BITMAPINFOHEADER},
    UI::WindowsAndMessaging::{GetWindowRect, GetWindowThreadProcessId},
};

type Handle = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct IconInfo {
    icon: i32,
    x: u32,
    y: u32,
    mask: Handle,
    color: Handle,
}

#[repr(C)]
#[derive(Default)]
struct BitmapInfo {
    kind: i32,
    width: i32,
    height: i32,
    stride: i32,
    planes: u16,
    depth: u16,
    bits: Handle,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetIconInfo(icon: Handle, info: *mut IconInfo) -> i32;
    fn GetGuiResources(process: Handle, flags: u32) -> u32;
    fn GetDC(hwnd: Handle) -> Handle;
    fn ReleaseDC(hwnd: Handle, dc: Handle) -> i32;
    fn PrintWindow(hwnd: Handle, dc: Handle, flags: u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn GetLastError() -> u32;
    fn SetLastError(error: u32);
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetObjectW(object: Handle, length: i32, output: *mut c_void) -> i32;
    fn GetDIBits(
        dc: Handle,
        bitmap: Handle,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        info: *mut BITMAPINFO,
        usage: u32,
    ) -> i32;
    fn CreateCompatibleDC(dc: Handle) -> Handle;
    fn CreateCompatibleBitmap(dc: Handle, width: i32, height: i32) -> Handle;
    fn SelectObject(dc: Handle, object: Handle) -> Handle;
    fn DeleteObject(object: Handle) -> i32;
    fn DeleteDC(dc: Handle) -> i32;
}

struct Bitmap(Handle);
impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

struct MemoryDc(Handle);
impl Drop for MemoryDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteDC(self.0) };
        }
    }
}

struct WindowDc(HWND, Handle);
impl Drop for WindowDc {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { ReleaseDC(self.0.0, self.1) };
        }
    }
}

fn read_bitmap(dc: Handle, bitmap: Handle, width: i32, height: i32) -> Result<Vec<u8>> {
    ensure!((1..=4096).contains(&width) && (1..=4096).contains(&height));
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0; width as usize * height as usize * 4];
    ensure!(
        unsafe {
            GetDIBits(
                dc,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr().cast(),
                &mut info,
                0,
            )
        } == height,
        "GetDIBits must return every top-down BGRA row"
    );
    Ok(pixels)
}

/// GetIconInfo duplicates its bitmaps. Never destroy the borrowed HICON itself.
pub fn icon(icon: Handle) -> Result<(u32, Vec<u8>)> {
    ensure!(!icon.is_null(), "missing HICON");
    let mut info = IconInfo::default();
    ensure!(
        unsafe { GetIconInfo(icon, &mut info) } != 0,
        "invalid HICON"
    );
    let _mask = Bitmap(info.mask);
    let color = Bitmap(info.color);
    ensure!(
        info.icon != 0 && !color.0.is_null(),
        "expected color application icon"
    );
    let mut dimensions = BitmapInfo::default();
    ensure!(
        unsafe {
            GetObjectW(
                color.0,
                size_of::<BitmapInfo>() as i32,
                ptr::from_mut(&mut dimensions).cast(),
            )
        } == size_of::<BitmapInfo>() as i32,
        "GetObjectW color bitmap"
    );
    ensure!(
        dimensions.width == dimensions.height
            && [16, 20, 24, 32].contains(&dimensions.width)
            && dimensions.planes == 1
            && dimensions.depth == 32,
        "unexpected HICON bitmap dimensions/format"
    );
    let dc = MemoryDc(unsafe { CreateCompatibleDC(ptr::null_mut()) });
    ensure!(!dc.0.is_null(), "create icon decoder memory DC");
    let pixels = read_bitmap(dc.0, color.0, dimensions.width, dimensions.height)?;
    Ok((dimensions.width as u32, pixels))
}

/// Process-wide [GDI, USER] counts, sampled after all decoder copies are dropped.
pub fn resources() -> [u32; 2] {
    [0, 1].map(|kind| unsafe {
        SetLastError(0);
        let count = GetGuiResources(GetCurrentProcess(), kind);
        assert!(count != 0 || GetLastError() == 0, "GetGuiResources failed");
        count
    })
}

/// Full-window PrintWindow only, including nonclient titlebar. No screen fallback.
pub fn capture(hwnd: HWND, path: &Path) -> Result<[i32; 2]> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    ensure!(
        pid == std::process::id(),
        "capture must belong to this test process"
    );
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }?;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    ensure!((1..=4096).contains(&width) && (1..=4096).contains(&height));
    let window = WindowDc(hwnd, unsafe { GetDC(hwnd.0) });
    ensure!(!window.1.is_null());
    let dc = MemoryDc(unsafe { CreateCompatibleDC(window.1) });
    ensure!(!dc.0.is_null());
    let bitmap = Bitmap(unsafe { CreateCompatibleBitmap(window.1, width, height) });
    ensure!(!bitmap.0.is_null());
    let old = unsafe { SelectObject(dc.0, bitmap.0) };
    ensure!(!old.is_null() && old as isize != -1);
    let painted = unsafe { PrintWindow(hwnd.0, dc.0, 0) };
    unsafe { SelectObject(dc.0, old) };
    ensure!(
        painted != 0,
        "PrintWindow failed; no desktop capture fallback"
    );
    let pixels = read_bitmap(dc.0, bitmap.0, width, height)?;
    let mut bmp = Vec::with_capacity(54 + pixels.len());
    bmp.extend(b"BM");
    bmp.extend((54 + pixels.len() as u32).to_le_bytes());
    bmp.extend([0; 4]);
    bmp.extend(54_u32.to_le_bytes());
    bmp.extend(40_u32.to_le_bytes());
    bmp.extend(width.to_le_bytes());
    bmp.extend((-height).to_le_bytes());
    bmp.extend(1_u16.to_le_bytes());
    bmp.extend(32_u16.to_le_bytes());
    bmp.extend(0_u32.to_le_bytes());
    bmp.extend((pixels.len() as u32).to_le_bytes());
    bmp.extend([0; 16]);
    bmp.extend(pixels);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(&bmp)?;
    Ok([width, height])
}
