//! Bounded first-record reads from an already anchored, validated open file.
//!
//! Only Windows MSVC uses a private mapping. Every mapped load is a volatile
//! byte load inside the native SEH helper; Rust never borrows the mapped bytes.
//! The view, section and temporary deny-write reservation end inside `read`.
//! This is not a snapshot against pre-existing writable mappings and does not
//! replace inventory freshness checks or the move/return reservations.

use std::{
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
};

const MAX_HEADER_LIMIT: usize = 4 * 1024 * 1024;
const INITIAL_OUTPUT: usize = 256;

fn invalid_header() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "header newline missing or over limit",
    )
}

/// Return only the first record, including its newline, and same-handle metadata.
/// `limit` includes the newline and must be in 1..=4 MiB. Parsing/UTF-8 validation
/// belong to the caller. The anchored owner must remain alive for this call.
pub(crate) fn read(
    owner: &crate::sessions_guard::HeaderOpen,
    limit: usize,
) -> io::Result<(Vec<u8>, Metadata)> {
    if limit == 0 || limit > MAX_HEADER_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid header limit",
        ));
    }
    #[cfg(all(windows, target_env = "msvc"))]
    {
        // SAFETY: HeaderOpen retains its validated leaf and ancestor pins. This
        // private helper never opens a pathname or lets a native view escape.
        unsafe { mapped::read_validated_file(owner.file(), limit) }
    }
    #[cfg(not(all(windows, target_env = "msvc")))]
    original_reader(owner.file(), limit)
}

fn original_reader(original: &File, limit: usize) -> io::Result<(Vec<u8>, Metadata)> {
    let metadata = original.metadata()?;
    let mut reader = original;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(INITIAL_OUTPUT.min(limit));
    // Capacity one is intentional: no file read can reach beyond the newline.
    // A newline outside `limit` is rejected without reading the extra byte.
    BufReader::with_capacity(1, reader.take(limit as u64)).read_until(b'\n', &mut bytes)?;
    if bytes.last() != Some(&b'\n') {
        return Err(invalid_header());
    }
    Ok((bytes, metadata))
}

#[cfg(all(windows, target_env = "msvc"))]
mod mapped {
    use super::*;
    use std::{
        ffi::c_void,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr::NonNull,
    };
    use windows::{
        Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
                FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
                GetFileInformationByHandle, ReOpenFile,
            },
            System::Memory::{
                CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
                PAGE_READONLY, UnmapViewOfFile,
            },
        },
        core::PCWSTR,
    };

    const FOUND: u32 = 0;
    const MORE: u32 = 1;
    const NO_NEWLINE: u32 = 2;
    const MAPPED_FAULT: u32 = 3;

    // This is our C ABI, not a hand-declared Windows API. Build.rs compiles the
    // matching header/implementation only for this target configuration.
    unsafe extern "C" {
        fn qb_plain_header_copy_chunk(
            view: *const c_void,
            view_len: usize,
            offset: usize,
            dst: *mut u8,
            dst_len: usize,
            written: *mut usize,
        ) -> u32;
    }

    fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: File owns the live handle; info is writable for the call.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(io::Error::other)?;
        if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header is not a regular file",
            ));
        }
        Ok(info)
    }

    fn identity(info: &BY_HANDLE_FILE_INFORMATION) -> (u32, u64) {
        (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    }

    fn reserve_read(original: &File) -> io::Result<File> {
        // SAFETY: reopen the validated object itself, never a pathname. Exclude
        // write sharing only for this read; preserve read/delete sharing.
        let handle = unsafe {
            ReOpenFile(
                HANDLE(original.as_raw_handle()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                FILE_FLAGS_AND_ATTRIBUTES(0),
            )
        }
        .map_err(io::Error::other)?;
        // SAFETY: successful ReOpenFile transfers a fresh, owned file handle.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }

    struct View<'file> {
        base: NonNull<c_void>,
        bound: usize,
        _section: OwnedHandle,
        _file: &'file File,
    }

    impl<'file> View<'file> {
        fn new(file: &'file File, bound: usize) -> io::Result<Self> {
            debug_assert!((1..=MAX_HEADER_LIMIT).contains(&bound));
            // SAFETY: read-only section of the live reserved handle, capped at
            // its observed length. A stale/shrunken length fails or faults in C.
            let handle = unsafe {
                CreateFileMappingW(
                    HANDLE(file.as_raw_handle()),
                    None,
                    PAGE_READONLY,
                    0,
                    bound as u32,
                    PCWSTR::null(),
                )
            }
            .map_err(io::Error::other)?;
            // SAFETY: successful creation transfers one owned section handle.
            let section = unsafe { OwnedHandle::from_raw_handle(handle.0) };
            // SAFETY: retain section and file until this private view is unmapped.
            let address = unsafe {
                MapViewOfFile(HANDLE(section.as_raw_handle()), FILE_MAP_READ, 0, 0, bound)
            };
            let base = NonNull::new(address.Value).ok_or_else(io::Error::last_os_error)?;
            Ok(Self {
                base,
                bound,
                _section: section,
                _file: file,
            })
        }
    }

    impl Drop for View<'_> {
        fn drop(&mut self) {
            // SAFETY: only this owner can unmap the view; the section and file
            // are still live. Closing the section follows this drop body.
            let _ = unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.base.as_ptr(),
                })
            };
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    enum ScanFailure {
        MappedRead,
        NoNewline,
        Allocation,
        Protocol,
    }

    /// SAFETY: `view` must stay allocated/mapped for `bound` bytes throughout
    /// this call. Pages may become inaccessible: only C SEH dereferences them.
    unsafe fn scan_view(view: *const c_void, bound: usize) -> Result<Vec<u8>, ScanFailure> {
        if bound == 0 {
            return Err(ScanFailure::NoNewline);
        }
        if bound > MAX_HEADER_LIMIT {
            return Err(ScanFailure::Protocol);
        }
        let mut output = Vec::<u8>::new();
        loop {
            if output.len() == output.capacity() {
                let target = if output.capacity() == 0 {
                    INITIAL_OUTPUT
                } else {
                    output.capacity().saturating_mul(2)
                }
                .min(bound);
                if target <= output.len() {
                    return Err(ScanFailure::Protocol);
                }
                output
                    .try_reserve_exact(target - output.len())
                    .map_err(|_| ScanFailure::Allocation)?;
            }
            let offset = output.len();
            let remaining = bound.checked_sub(offset).ok_or(ScanFailure::Protocol)?;
            let room = (output.capacity() - offset).min(remaining);
            if room == 0 {
                return Err(ScanFailure::Protocol);
            }
            let mut written = 0;
            // SAFETY: dst is separate Rust-owned spare capacity; the native
            // helper initializes exactly `written` bytes on FOUND/MORE only.
            let status = unsafe {
                qb_plain_header_copy_chunk(
                    view,
                    bound,
                    offset,
                    output.as_mut_ptr().add(offset),
                    room,
                    &mut written,
                )
            };
            match status {
                FOUND | MORE => {
                    if written == 0
                        || written > room
                        || (status == MORE && (written != room || written >= remaining))
                    {
                        return Err(ScanFailure::Protocol);
                    }
                    // SAFETY: the checked count describes initialized owned bytes.
                    unsafe { output.set_len(offset + written) };
                    if status == FOUND {
                        if output.last() != Some(&b'\n') {
                            return Err(ScanFailure::Protocol);
                        }
                        return Ok(output);
                    }
                }
                MAPPED_FAULT if written == 0 => return Err(ScanFailure::MappedRead),
                NO_NEWLINE if written == 0 => return Err(ScanFailure::NoNewline),
                _ => return Err(ScanFailure::Protocol),
            }
        }
    }

    /// SAFETY: original is the leaf of a retained HeaderOpen. Only synthetic
    /// unit fixtures may bypass that owner. No arbitrary-file public API exists.
    pub(super) unsafe fn read_validated_file(
        original: &File,
        limit: usize,
    ) -> io::Result<(Vec<u8>, Metadata)> {
        let first = information(original)?;
        let reserved = match reserve_read(original) {
            Ok(file) => file,
            Err(_) => return original_reader(original, limit),
        };
        if identity(&first) != identity(&information(&reserved)?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header handle identity changed",
            ));
        }
        let metadata = reserved.metadata()?;
        let bound = metadata.len().min(limit as u64) as usize;
        if bound == 0 {
            return Err(invalid_header());
        }
        let view = match View::new(&reserved, bound) {
            Ok(view) => view,
            Err(_) => {
                // Return-scope RAII keeps the reservation through fallback.
                return original_reader(original, limit);
            }
        };
        // SAFETY: view and reservation remain live; scan makes no Rust borrow.
        let scanned = unsafe { scan_view(view.base.as_ptr(), view.bound) };
        drop(view); // Unmap and close the section before any fallback or return.
        let result = match scanned {
            Ok(bytes) => Ok((bytes, metadata)),
            Err(ScanFailure::MappedRead) => original_reader(original, limit),
            Err(ScanFailure::NoNewline) => Err(invalid_header()),
            Err(ScanFailure::Allocation) => Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "header allocation failed",
            )),
            Err(ScanFailure::Protocol) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native header protocol failure",
            )),
        };
        drop(reserved); // No deny-write lease survives the read, even on error.
        result
    }

    #[cfg(test)]
    mod tests {
        include!("sessions_plain_header_tests.rs");
    }
}
