//! Shared build-time executable icon. No runtime/Win32 dependency is needed here.
use std::{env, fs, path::PathBuf, process::Command};

#[allow(dead_code)]
#[path = "../crates/qb-tray/src/tray_icon/pixels.rs"]
mod pixels;

const SIZES: [u32; 4] = [16, 20, 24, 32];

pub fn embed(binary: &str) {
    println!("cargo:rerun-if-changed=../../build-support/windows_icon.rs");
    println!("cargo:rerun-if-changed=../qb-tray/src/tray_icon/pixels.rs");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    assert_eq!(
        env::var("CARGO_CFG_TARGET_ENV").as_deref(),
        Ok("msvc"),
        "Windows executable icons currently require the MSVC target (.res linking)"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    fs::write(out.join("quota-baton.ico"), ico()).expect("write executable ICO");
    fs::write(
        out.join("quota-baton.rc"),
        "LANGUAGE 0, 0\n1 ICON \"quota-baton.ico\"\n",
    )
    .expect("write executable RC");

    let explicit = env::var_os("RC");
    let compilers = explicit
        .as_ref()
        .map(|path| vec![path.clone()])
        .unwrap_or_else(|| vec!["llvm-rc.exe".into(), "rc.exe".into()]);
    let mut compiled = false;
    for compiler in compilers {
        let result = Command::new(&compiler)
            .current_dir(&out)
            .args(["/FO", "quota-baton.res", "quota-baton.rc"])
            .output();
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                continue;
            }
            Err(error) => panic!("cannot run icon resource compiler {compiler:?}: {error}"),
            Ok(output) => {
                assert!(
                    output.status.success(),
                    "icon resource compiler {compiler:?} failed ({}):\n{}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                compiled = true;
                break;
            }
        }
    }
    assert!(
        compiled,
        "executable icon requires llvm-rc.exe or rc.exe on PATH; alternatively set RC to an SDK rc.exe executable path"
    );
    println!(
        "cargo:rustc-link-arg-bin={binary}={}",
        out.join("quota-baton.res").display()
    );
}

fn word(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}

fn dword(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn ico() -> Vec<u8> {
    let frames = SIZES.map(dib);
    let mut bytes = Vec::new();
    word(&mut bytes, 0);
    word(&mut bytes, 1);
    word(&mut bytes, SIZES.len() as u16);
    let mut offset = 6 + 16 * u32::try_from(SIZES.len()).unwrap();
    for (size, frame) in SIZES.into_iter().zip(&frames) {
        bytes.extend([size as u8, size as u8, 0, 0]);
        word(&mut bytes, 1);
        word(&mut bytes, 32);
        let length = u32::try_from(frame.len()).expect("ICO frame length");
        dword(&mut bytes, length);
        dword(&mut bytes, offset);
        offset = offset.checked_add(length).expect("ICO offset overflow");
    }
    for frame in frames {
        bytes.extend(frame);
    }
    bytes
}

fn dib(size: u32) -> Vec<u8> {
    let image = pixels::render(size, false, pixels::Badge::Normal).expect("supported icon size");
    let mask_stride = size.div_ceil(32) * 4;
    let mut bytes = Vec::new();
    dword(&mut bytes, 40);
    dword(&mut bytes, size);
    dword(&mut bytes, size * 2);
    word(&mut bytes, 1);
    word(&mut bytes, 32);
    dword(&mut bytes, 0); // BI_RGB
    dword(&mut bytes, size * size * 4);
    for _ in 0..4 {
        dword(&mut bytes, 0);
    }
    for y in (0..size).rev() {
        for x in 0..size {
            let start = ((y * size + x) * 4) as usize;
            let pixel = &image.bgra[start..start + 4];
            let alpha = u32::from(pixel[3]);
            for channel in &pixel[..3] {
                let straight = if alpha == 0 {
                    0
                } else {
                    ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8
                };
                bytes.push(straight);
            }
            bytes.push(pixel[3]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0_u8; mask_stride as usize];
        for x in 0..size {
            if image.bgra[((y * size + x) * 4 + 3) as usize] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        bytes.extend(row);
    }
    bytes
}
