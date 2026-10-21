// Included inside `mapped::tests`, so the unsafe file/view helpers stay private.
// Every filesystem fixture lives under this worktree's ignored .sandbox.
use super::*;
use crate::sessions_guard::{HeaderOpen, header_directory, open_header};
use std::{
    fs::{self, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::Path,
};
use windows::Win32::{
    Storage::FileSystem::FILE_SHARE_WRITE,
    System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    },
};

const PAGE_BOUNDARY: usize = 64 * 1024;

fn fixture() -> tempfile::TempDir {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-plain-header-tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(fs::canonicalize(root).unwrap()).unwrap()
}

fn owner(root: &Path, path: &Path) -> HeaderOpen {
    let directory = header_directory(root, path.parent().unwrap()).unwrap();
    open_header(&directory, path).unwrap()
}

fn writer(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .open(path)
}

// Guard native memory without ever making a Rust slice of the source view.
// The 64 KiB boundary also satisfies Windows page alignment. Only the native
// byte loader may touch the trailing PAGE_NOACCESS region.
struct GuardedMemory {
    base: NonNull<u8>,
    readable: usize,
    total: usize,
}

impl GuardedMemory {
    fn new(readable: usize) -> Self {
        assert!(readable > 0 && readable % PAGE_BOUNDARY == 0);
        let total = readable + PAGE_BOUNDARY;
        // SAFETY: request private committed memory; no Rust reference aliases it.
        let pointer =
            unsafe { VirtualAlloc(None, total, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let memory = Self {
            base: NonNull::new(pointer.cast()).expect("synthetic native allocation"),
            readable,
            total,
        };
        // SAFETY: readable bytes are committed writable memory owned by memory.
        unsafe { std::ptr::write_bytes(memory.base.as_ptr(), b'x', readable) };
        let mut old = PAGE_PROTECTION_FLAGS::default();
        // SAFETY: the trailing region belongs to this allocation; its aligned
        // boundary leaves the preceding readable region intact.
        unsafe {
            VirtualProtect(
                memory.base.as_ptr().add(readable).cast(),
                PAGE_BOUNDARY,
                PAGE_NOACCESS,
                &mut old,
            )
        }
        .expect("synthetic guard protection");
        memory
    }

    fn put(&mut self, offset: usize, bytes: &[u8]) {
        assert!(offset <= self.readable && bytes.len() <= self.readable - offset);
        // SAFETY: private writable memory, disjoint from the supplied owned bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.base.as_ptr().add(offset),
                bytes.len(),
            )
        };
    }

    fn scan(&self, offset: usize, bound: usize) -> Result<Vec<u8>, ScanFailure> {
        assert!(offset <= self.total && bound <= self.total - offset);
        // SAFETY: allocation stays live for the entire indicated extent, even
        // where protected. C SEH alone reads the potentially inaccessible bytes.
        unsafe { scan_view(self.base.as_ptr().add(offset).cast(), bound) }
    }
}

impl Drop for GuardedMemory {
    fn drop(&mut self) {
        // SAFETY: base is the unique original allocation and is released once.
        let _ = unsafe { VirtualFree(self.base.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}

#[test]
fn native_newline_first_needs_no_next_byte() {
    let mut memory = GuardedMemory::new(PAGE_BOUNDARY);
    memory.put(PAGE_BOUNDARY - 1, b"\n");
    assert_eq!(memory.scan(PAGE_BOUNDARY - 1, 2).unwrap(), b"\n");
}

#[test]
fn native_header_at_guard_boundary_does_not_read_body() {
    let mut memory = GuardedMemory::new(PAGE_BOUNDARY);
    memory.put(PAGE_BOUNDARY - 3, b"{}\n");
    // The first body byte is PAGE_NOACCESS, within the supplied view bound.
    assert_eq!(memory.scan(PAGE_BOUNDARY - 3, 4).unwrap(), b"{}\n");
}

#[test]
fn native_required_inaccessible_byte_is_a_controlled_fault() {
    let memory = GuardedMemory::new(PAGE_BOUNDARY);
    assert_eq!(
        memory.scan(PAGE_BOUNDARY - 1, 2),
        Err(ScanFailure::MappedRead)
    );
    // Also fault after several successfully published output chunks. No
    // partially copied header may escape as a successful result.
    assert_eq!(memory.scan(0, memory.total), Err(ScanFailure::MappedRead));
}

#[test]
fn native_newline_at_last_byte_of_maximum_window_is_accepted() {
    let mut memory = GuardedMemory::new(MAX_HEADER_LIMIT);
    memory.put(MAX_HEADER_LIMIT - 1, b"\n");
    let bytes = memory.scan(0, MAX_HEADER_LIMIT).unwrap();
    assert_eq!(bytes.len(), MAX_HEADER_LIMIT);
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert!(
        bytes[..MAX_HEADER_LIMIT - 1]
            .iter()
            .all(|byte| *byte == b'x')
    );
}

#[test]
fn native_limit_excludes_newline_one_byte_beyond_it() {
    let mut memory = GuardedMemory::new(PAGE_BOUNDARY);
    memory.put(32, b"\n");
    assert_eq!(memory.scan(0, 32), Err(ScanFailure::NoNewline));
    assert_eq!(memory.scan(0, 33).unwrap().len(), 33);
}

#[test]
fn native_no_newline_and_empty_window_fail_closed() {
    let memory = GuardedMemory::new(PAGE_BOUNDARY);
    for bound in [0, 1, INITIAL_OUTPUT, PAGE_BOUNDARY] {
        assert_eq!(memory.scan(0, bound), Err(ScanFailure::NoNewline));
    }
}

#[test]
fn native_short_prefix_uses_small_owned_output_and_ignores_invalid_utf8_body() {
    let mut memory = GuardedMemory::new(MAX_HEADER_LIMIT);
    memory.put(0, b"{}\n\xff\xfe");
    let bytes = memory.scan(0, MAX_HEADER_LIMIT).unwrap();
    assert_eq!(bytes, b"{}\n");
    assert!(std::str::from_utf8(&bytes).is_ok());
    assert!(bytes.capacity() <= INITIAL_OUTPUT);
}

#[test]
fn public_reader_rejects_zero_and_excessive_limits() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    fs::write(&path, b"{}\n").unwrap();
    let opened = owner(fixture.path(), &path);
    for limit in [0, MAX_HEADER_LIMIT + 1] {
        assert_eq!(
            super::super::read(&opened, limit).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
    }
}

#[test]
fn original_reader_rewinds_and_stops_at_newline_before_invalid_body() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    fs::write(&path, b"{}\n\xff\xfe").unwrap();
    let opened = owner(fixture.path(), &path);
    let mut cursor = opened.file();
    cursor.seek(SeekFrom::End(0)).unwrap();
    let (bytes, metadata) = original_reader(opened.file(), MAX_HEADER_LIMIT).unwrap();
    assert_eq!(bytes, b"{}\n");
    assert_eq!(metadata.len(), 5);
    assert_eq!(cursor.stream_position().unwrap(), 3);
}

#[test]
fn original_reader_accepts_exact_limit_and_rejects_missing_newline() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    fs::write(&path, b"abcd\n").unwrap();
    let opened = owner(fixture.path(), &path);
    assert_eq!(
        original_reader(opened.file(), 4).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(original_reader(opened.file(), 5).unwrap().0, b"abcd\n");
    drop(opened);
    fs::write(&path, b"").unwrap();
    let opened = owner(fixture.path(), &path);
    assert_eq!(
        original_reader(opened.file(), 5).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn mapped_reader_uses_opened_identity_after_path_replacement() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    let retired = fixture.path().join("retired.jsonl");
    let original_bytes = b"{\"original\":true}\n\xff\xfe";
    fs::write(&path, original_bytes).unwrap();
    let opened = owner(fixture.path(), &path);
    let original_id = identity(&information(opened.file()).unwrap());
    fs::rename(&path, &retired).unwrap();
    fs::write(&path, b"{\"replacement\":true}\n").unwrap();
    let replacement = owner(fixture.path(), &path);
    assert_ne!(
        original_id,
        identity(&information(replacement.file()).unwrap())
    );
    let (bytes, metadata) = super::super::read(&opened, MAX_HEADER_LIMIT).unwrap();
    assert_eq!(bytes, b"{\"original\":true}\n");
    assert_eq!(metadata.len(), original_bytes.len() as u64);
    assert_eq!(identity(&information(opened.file()).unwrap()), original_id);
}

#[test]
fn existing_writer_forces_fallback_on_same_original_handle() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    let retired = fixture.path().join("retired.jsonl");
    fs::write(&path, b"original\n\xff").unwrap();
    let opened = owner(fixture.path(), &path);
    let held_writer = writer(&path).unwrap();
    assert!(reserve_read(opened.file()).is_err());
    fs::rename(&path, &retired).unwrap();
    fs::write(&path, b"replacement\n").unwrap();
    let mut cursor = opened.file();
    cursor.seek(SeekFrom::End(0)).unwrap();
    let (bytes, metadata) = super::super::read(&opened, MAX_HEADER_LIMIT).unwrap();
    assert_eq!(bytes, b"original\n");
    assert_eq!(metadata.len(), 10);
    assert_eq!(cursor.stream_position().unwrap(), 9);
    drop(held_writer);
}

#[test]
fn reservation_blocks_truncating_writer_until_view_and_section_are_released() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    fs::write(&path, b"header\nbody").unwrap();
    let opened = owner(fixture.path(), &path);
    let reserved = reserve_read(opened.file()).unwrap();
    let view = View::new(&reserved, 11).unwrap();
    // A normal truncating writer must be refused while the temporary reservation
    // and mapping live. This does not model a pre-existing writable section.
    let denied = OpenOptions::new()
        .write(true)
        .truncate(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .open(&path)
        .unwrap_err();
    assert_eq!(denied.raw_os_error(), Some(32)); // ERROR_SHARING_VIOLATION
    // SAFETY: the retained view and file cover the entire bounded source.
    assert_eq!(
        unsafe { scan_view(view.base.as_ptr(), view.bound) }.unwrap(),
        b"header\n"
    );
    drop(view);
    assert!(writer(&path).is_err()); // The reservation still denies writing.
    drop(reserved);
    let truncator = writer(&path).unwrap();
    truncator.set_len(0).unwrap(); // Unmap + section close really preceded this.
    drop(truncator);
    assert_eq!(
        super::super::read(&opened, MAX_HEADER_LIMIT)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData,
    );
}

#[test]
fn public_reader_releases_reservation_on_success_and_missing_newline() {
    let fixture = fixture();
    let path = fixture.path().join("source.jsonl");
    fs::write(&path, b"header\nbody").unwrap();
    let opened = owner(fixture.path(), &path);
    let (bytes, _) = super::super::read(&opened, MAX_HEADER_LIMIT).unwrap();
    assert_eq!(bytes, b"header\n");
    let truncator = writer(&path).unwrap();
    truncator.set_len(3).unwrap();
    drop(truncator);
    assert_eq!(
        super::super::read(&opened, MAX_HEADER_LIMIT)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData,
    );
    let truncator = writer(&path).unwrap();
    truncator.set_len(0).unwrap();
}
