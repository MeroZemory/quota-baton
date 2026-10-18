//! Geometry only. Call disk operations from the tray worker, never the window thread.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub maximized: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiFile {
    schema: u8,
    dashboard: Geometry,
}

impl Geometry {
    pub fn valid(self) -> bool {
        (48..=768).contains(&self.dpi)
            && (100..=32_768).contains(&self.width)
            && (100..=32_768).contains(&self.height)
            && self.left.unsigned_abs() <= 1_000_000
            && self.top.unsigned_abs() <= 1_000_000
    }

    /// Returns a physical window rectangle on the chosen monitor. Restore saved
    /// DIPs at its current DPI; an unplugged monitor never strands the caption.
    pub fn restore(self, work: [i32; 4], dpi: u32) -> Option<[i32; 4]> {
        if !self.valid() || !(48..=768).contains(&dpi) {
            return None;
        }
        let ww = i64::from(work[2]) - i64::from(work[0]);
        let wh = i64::from(work[3]) - i64::from(work[1]);
        if ww < 100 || wh < 100 {
            return None;
        }
        let scale = |v: u32| i64::from(v) * i64::from(dpi) / i64::from(self.dpi);
        let cap_w = ww * 9 / 10;
        let cap_h = wh * 9 / 10;
        let min_w = (1024 * i64::from(dpi) / 96).min(cap_w);
        let min_h = (640 * i64::from(dpi) / 96).min(cap_h);
        let width = scale(self.width).clamp(min_w, cap_w);
        let height = scale(self.height).clamp(min_h, cap_h);
        let left = i64::from(self.left).clamp(i64::from(work[0]), i64::from(work[2]) - width);
        let top = i64::from(self.top).clamp(i64::from(work[1]), i64::from(work[3]) - height);
        Some([
            left as i32,
            top as i32,
            (left + width) as i32,
            (top + height) as i32,
        ])
    }
}

fn safe_dir(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.file_attributes() & 0x400 == 0)
}

pub fn load(home: &Path) -> Option<Geometry> {
    use std::os::windows::fs::MetadataExt;
    if !safe_dir(home) || !safe_dir(&home.join("state")) {
        return None;
    }
    let path = home.join("state/ui.json");
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.file_attributes() & 0x400 != 0 || meta.len() > 4096 {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let file: UiFile = serde_json::from_slice(&bytes).ok()?;
    (file.schema == 1 && file.dashboard.valid()).then_some(file.dashboard)
}

pub fn save(home: &Path, geometry: Geometry) -> std::io::Result<()> {
    use std::{
        os::windows::ffi::OsStrExt,
        sync::atomic::{AtomicU64, Ordering},
    };
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if !geometry.valid() || !safe_dir(home) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let state = home.join("state");
    if !state.exists() {
        fs::create_dir(&state)?;
    }
    if !safe_dir(&state) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let path = state.join("ui.json");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let temp = state.join(format!(
        ".ui-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec(&UiFile {
        schema: 1,
        dashboard: geometry,
    })?;
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        let src: Vec<_> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let dst: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(src.as_ptr()),
                PCWSTR(dst.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(std::io::Error::other)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Geometry {
        Geometry {
            left: 4000,
            top: -2000,
            width: 1320,
            height: 960,
            dpi: 96,
            maximized: false,
        }
    }
    #[test]
    fn disconnected_monitor_and_dpi_restore_stay_inside_work_area() {
        for dpi in [96, 120, 144, 192] {
            let r = sample().restore([-1920, 0, 0, 1080], dpi).unwrap();
            assert!(r[0] >= -1920 && r[1] >= 0 && r[2] <= 0 && r[3] <= 1080);
            assert!(r[2] - r[0] <= 1728 && r[3] - r[1] <= 972);
        }
        assert!(sample().restore([0, 0, 80, 90], 96).is_none());
    }
    #[test]
    fn persisted_schema_has_geometry_only_and_rejects_display_data() {
        let value = serde_json::to_value(UiFile {
            schema: 1,
            dashboard: sample(),
        })
        .unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(value["dashboard"].as_object().unwrap().len(), 6);
        let mut injected = value;
        injected["dashboard"]["session_label"] = "must-not-survive".into();
        assert!(serde_json::from_value::<UiFile>(injected).is_err());
    }
}
