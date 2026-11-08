//! Isolated real-HWND scaling only. No display, registry, monitor or process settings.
use std::{
    cell::{Cell, RefCell},
    sync::OnceLock,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, RECT},
        Graphics::Direct2D::ID2D1HwndRenderTarget,
        UI::WindowsAndMessaging::{GetClientRect, SetPropW},
    },
    core::w,
};

const INVALID: &str = "invalid isolated tray DPI fixture configuration";
static OVERRIDE: OnceLock<Option<u32>> = OnceLock::new();
thread_local! {
    static PROOF: RefCell<Option<(isize, Proof)>> = const { RefCell::new(None) };
    static SEQUENCE: Cell<usize> = const { Cell::new(0) };
}

/// An immutable copy from one actual HWND/target observation, not requested DPI alone.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct Proof {
    pub sequence: usize,
    pub os_dpi: u32,
    pub effective_dpi: u32,
    pub target_dpi_x: f32,
    pub target_dpi_y: f32,
    pub client_pixels: [u32; 2],
    pub target_pixels: [u32; 2],
}

fn validate(
    marker: Option<&str>,
    base: Option<&str>,
    no_browser: bool,
    no_clipboard: bool,
    prepared_fixture: bool,
) -> Result<Option<u32>, &'static str> {
    let Some(marker) = marker else {
        return Ok(None);
    };
    let dpi = match marker {
        "144" => 144,
        "192" => 192,
        _ => return Err(INVALID),
    };
    if !no_browser || !no_clipboard || prepared_fixture {
        return Err(INVALID);
    }
    let value = base.ok_or(INVALID)?;
    if value.bytes().any(|b| b.is_ascii_control() || b == b'\\') {
        return Err(INVALID);
    }
    let url = reqwest::Url::parse(value).map_err(|_| INVALID)?;
    let ip: std::net::IpAddr = url
        .host_str()
        .ok_or(INVALID)?
        .trim_matches(['[', ']'])
        .parse()
        .map_err(|_| INVALID)?;
    if url.scheme() != "http"
        || !ip.is_loopback()
        || !url
            .port()
            .is_some_and(|port| port != 0 && port != 10100 && port != 10101)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(INVALID);
    }
    Ok(Some(dpi))
}

pub(crate) fn initialize(prepared_fixture: bool) -> anyhow::Result<()> {
    let marker = std::env::var_os("QB_TRAY_TEST_DPI");
    let selected = if let Some(marker) = marker {
        let marker = marker.to_str().ok_or_else(|| anyhow::anyhow!(INVALID))?;
        validate(
            Some(marker),
            std::env::var("QB_TEST_LOGIN_BASE").ok().as_deref(),
            std::env::var("QB_TRAY_TEST_NO_BROWSER").as_deref() == Ok("1"),
            std::env::var("QB_TRAY_TEST_NO_CLIPBOARD").as_deref() == Ok("1"),
            prepared_fixture,
        )
        .map_err(|_| anyhow::anyhow!(INVALID))?
    } else {
        None
    };
    if let Some(previous) = OVERRIDE.get() {
        anyhow::ensure!(*previous == selected, INVALID);
    } else {
        OVERRIDE
            .set(selected)
            .map_err(|_| anyhow::anyhow!(INVALID))?;
    }
    Ok(())
}

pub(crate) fn effective(hwnd: HWND) -> u32 {
    OVERRIDE
        .get()
        .copied()
        .flatten()
        .unwrap_or_else(|| super::actual(hwnd).max(96))
}

pub(crate) fn proof(hwnd: HWND) -> Option<Proof> {
    PROOF.with(|slot| {
        (*slot.borrow())
            .filter(|(owner, _)| *owner == hwnd.0 as isize)
            .map(|(_, proof)| proof)
    })
}

pub(crate) fn record_target(hwnd: HWND, target: &ID2D1HwndRenderTarget) {
    // Default debug+testkit runs remain uninstrumented unless explicitly requested.
    if OVERRIDE.get().copied().flatten().is_none() {
        return;
    }
    PROOF.with(|slot| *slot.borrow_mut() = None);
    let mut client = RECT::default();
    let mut x = 0.;
    let mut y = 0.;
    let target_pixels;
    unsafe {
        if GetClientRect(hwnd, &mut client).is_err() {
            return;
        }
        target.GetDpi(&mut x, &mut y);
        target_pixels = target.GetPixelSize();
    }
    let sequence = SEQUENCE.with(|counter| {
        let next = counter.get().checked_add(2)?;
        counter.set(next);
        Some(next)
    });
    let Some(sequence) = sequence else {
        return;
    };
    let observation = Proof {
        sequence,
        os_dpi: super::actual(hwnd),
        effective_dpi: effective(hwnd),
        target_dpi_x: x,
        target_dpi_y: y,
        client_pixels: [
            (client.right - client.left).max(0) as u32,
            (client.bottom - client.top).max(0) as u32,
        ],
        target_pixels: [target_pixels.width, target_pixels.height],
    };
    let pairs = [
        (w!("QuotaBaton.TestDpi.Os"), observation.os_dpi as usize),
        (
            w!("QuotaBaton.TestDpi.Effective"),
            observation.effective_dpi as usize,
        ),
        (
            w!("QuotaBaton.TestDpi.TargetX1000"),
            (x * 1000.).round() as usize,
        ),
        (
            w!("QuotaBaton.TestDpi.TargetY1000"),
            (y * 1000.).round() as usize,
        ),
        (
            w!("QuotaBaton.TestDpi.ClientWidth"),
            observation.client_pixels[0] as usize,
        ),
        (
            w!("QuotaBaton.TestDpi.ClientHeight"),
            observation.client_pixels[1] as usize,
        ),
        (
            w!("QuotaBaton.TestDpi.TargetWidth"),
            observation.target_pixels[0] as usize,
        ),
        (
            w!("QuotaBaton.TestDpi.TargetHeight"),
            observation.target_pixels[1] as usize,
        ),
    ];
    unsafe {
        // Reader takes sequence before/after properties: accept equal nonzero even values.
        if SetPropW(
            hwnd,
            w!("QuotaBaton.TestDpi.Sequence"),
            Some(HANDLE((sequence - 1) as *mut std::ffi::c_void)),
        )
        .is_err()
        {
            return;
        }
        for (name, value) in pairs {
            if SetPropW(hwnd, name, Some(HANDLE(value as *mut std::ffi::c_void))).is_err() {
                PROOF.with(|slot| *slot.borrow_mut() = None);
                return;
            }
        }
        if SetPropW(
            hwnd,
            w!("QuotaBaton.TestDpi.Sequence"),
            Some(HANDLE(sequence as *mut std::ffi::c_void)),
        )
        .is_err()
        {
            return;
        }
    }
    PROOF.with(|slot| *slot.borrow_mut() = Some((hwnd.0 as isize, observation)));
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
