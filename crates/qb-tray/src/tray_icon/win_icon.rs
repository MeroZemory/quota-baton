//! Minimal Win32 ABI isolated here; no extra windows crate feature required.
//! Keep this module Windows-only. All handles are private, uniquely owned.
use super::pixels::{self, Badge};
use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::ptr;
use std::rc::Rc;

type Handle = *mut c_void;

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

#[repr(C)]
struct IconInfo {
    icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    mask: Handle,
    color: Handle,
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateDIBSection(
        dc: Handle,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: Handle,
        offset: u32,
    ) -> Handle;
    fn CreateBitmap(
        width: i32,
        height: i32,
        planes: u32,
        bit_count: u32,
        bits: *const c_void,
    ) -> Handle;
    fn DeleteObject(object: Handle) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateIconIndirect(info: *const IconInfo) -> Handle;
    fn DestroyIcon(icon: Handle) -> i32;
}

struct Bitmap(Handle);
impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: non-null private bitmap, never selected into any DC.
        unsafe {
            DeleteObject(self.0);
        }
    }
}

/// Owned generated HICON; deliberately neither Clone nor Send/Sync.
/// Retain on UI thread until successful NIM_MODIFY/NIM_DELETE releases its use.
pub struct NotifyIcon {
    handle: Handle,
    _ui_thread: PhantomData<Rc<()>>,
}

impl NotifyIcon {
    pub fn new(size: u32, dark: bool, badge: Badge) -> io::Result<Self> {
        let pixels = pixels::render(size, dark, badge).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected 16, 20, 24 or 32 pixels",
            )
        })?;
        let info = BitmapInfo {
            header: BitmapInfoHeader {
                size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                width: size as i32,
                height: -(size as i32), // top-down BGRA
                planes: 1,
                bit_count: 32,
                compression: 0, // BI_RGB
                size_image: pixels.bgra.len() as u32,
                x_pels_per_meter: 0,
                y_pels_per_meter: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };
        debug_assert_eq!(pixels.size, size);
        let mut bits = ptr::null_mut();
        // SAFETY: valid C-layout info/output pointer; no mapped section or DC.
        let color =
            unsafe { CreateDIBSection(ptr::null_mut(), &info, 0, &mut bits, ptr::null_mut(), 0) };
        if color.is_null() {
            return Err(io::Error::last_os_error());
        }
        let color = Bitmap(color);
        if bits.is_null() {
            return Err(io::Error::other(
                "CreateDIBSection returned no pixel buffer",
            ));
        }
        // SAFETY: DIB allocated exactly width*height*4 bytes; source is separate.
        unsafe {
            ptr::copy_nonoverlapping(pixels.bgra.as_ptr(), bits.cast::<u8>(), pixels.bgra.len());
        }
        // Explicit zero AND mask: on supported Windows 11, the 32-bit alpha
        // channel supplies transparency. Word-align every 1bpp scanline.
        let stride = size.div_ceil(16) as usize * 2;
        let mask_bits = vec![0_u8; stride * size as usize];
        // SAFETY: buffer covers every scanline; CreateBitmap copies the bits.
        let mask =
            unsafe { CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr().cast()) };
        if mask.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mask = Bitmap(mask);
        let info = IconInfo {
            icon: 1,
            x_hotspot: 0,
            y_hotspot: 0,
            mask: mask.0,
            color: color.0,
        };
        // SAFETY: both bitmaps live through this call; API copies them.
        let handle = unsafe { CreateIconIndirect(&info) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle,
            _ui_thread: PhantomData,
        })
        // Both temporary bitmaps deleted on success and every early-return path.
    }

    /// Borrow only. For windows-rs use HICON(icon.as_raw()); never DestroyIcon it.
    pub fn as_raw(&self) -> *mut c_void {
        self.handle
    }
}

impl Drop for NotifyIcon {
    fn drop(&mut self) {
        // SAFETY: uniquely owned non-shared HICON from CreateIconIndirect.
        unsafe {
            DestroyIcon(self.handle);
        }
    }
}
