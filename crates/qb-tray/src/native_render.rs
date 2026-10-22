//! PNG compatibility adapter. HWND and WIC both paint through v2_render.
use crate::{
    Row, v2, v2_geometry as g,
    v2_render::{LoginStatus, RenderOptions, Renderer},
};
use std::{collections::BTreeSet, path::Path, time::SystemTime};
use windows::{
    Win32::{
        Foundation::E_INVALIDARG,
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1CreateFactory, ID2D1Factory,
                ID2D1RenderTarget,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Imaging::{
                CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppPBGRA,
                IWICImagingFactory, WICBitmapCacheOnLoad, WICBitmapEncoderNoCache,
            },
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
    },
    core::{Error, PCWSTR, Result},
};
use windows_numerics::Matrix3x2;

fn input(rows: &[Row], now: SystemTime) -> Result<v2::View> {
    let json = if rows.is_empty() {
        "{\"accounts\":[]}"
    } else {
        rows[0]
            .view_input
            .as_deref()
            .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?
    };
    v2::project(json, now).map_err(|_| Error::from_hresult(E_INVALIDARG))
}
fn presentation(
    view: &v2::View,
    expand: bool,
    state: Option<&str>,
    core_ok: bool,
) -> g::Presentation {
    let mut p = g::Presentation {
        core_ok,
        ..Default::default()
    };
    if expand || state == Some("expanded") {
        for s in &view.sections {
            p.expanded.insert(s.provider.clone(), true);
        }
    }
    if let Some(provider) = state.and_then(|s| s.strip_prefix("expanded:")) {
        for s in &view.sections {
            p.expanded
                .insert(s.provider.clone(), s.provider == provider);
        }
    }
    if let Some(state) = state {
        if let Some(id) = state
            .strip_prefix("menu:")
            .and_then(|s| s.split(':').next())
        {
            p.overlay = g::Overlay::RowMenu(id.into());
            for s in &view.sections {
                if s.disabled_rows.iter().any(|r| r.raw.id == id) {
                    p.expanded.insert(s.provider.clone(), true);
                }
            }
        } else if let Some(id) = state.strip_prefix("confirm-delete:") {
            p.overlay = g::Overlay::DeleteConfirm(id.into());
        } else if let Some(id) = state.strip_prefix("confirm-credits:") {
            p.overlay = g::Overlay::CreditsConfirm(id.into());
        } else if let Some(provider) = state
            .strip_prefix("login:")
            .and_then(|s| s.split(':').next())
        {
            p.overlay = g::Overlay::Login(provider.into());
        } else {
            p.overlay = match state {
                "settings" => g::Overlay::Settings,
                "quit" => g::Overlay::Quit,
                "add" => g::Overlay::Add,
                _ => g::Overlay::None,
            };
        }
    }
    p
}
fn prepared(
    rows: &[Row],
    dark: bool,
    now: SystemTime,
    expand: bool,
    core_ok: bool,
    state: Option<&str>,
) -> Result<(Renderer, v2::View, g::Scene)> {
    let view = input(rows, now)?;
    let renderer = Renderer::new(dark)?;
    let mut p = presentation(&view, expand, state, core_ok);
    renderer.measure(&view, &mut p)?;
    let scene = g::Scene::build(&view, p, 8192.);
    Ok((renderer, view, scene))
}
pub fn content_height(rows: &[Row], expand_disabled: bool) -> f32 {
    state_height(rows, expand_disabled, None)
}
pub fn state_height(rows: &[Row], expand_disabled: bool, state: Option<&str>) -> f32 {
    // Compatibility callers can construct a Row without its authoritative state;
    // only their legacy geometry can be measured, never guessed into a painting.
    prepared(rows, false, SystemTime::now(), expand_disabled, true, state)
        .map(|(_, _, s)| s.rect.bottom)
        .unwrap_or_else(|_| crate::state_height(rows, expand_disabled, state))
}

#[allow(clippy::too_many_arguments)]
pub fn paint(
    target: &ID2D1RenderTarget,
    rows: &[Row],
    dark: bool,
    version: &str,
    now: SystemTime,
    expand_disabled: bool,
    core_ok: bool,
) -> Result<()> {
    paint_with_state(
        target,
        rows,
        dark,
        version,
        now,
        expand_disabled,
        core_ok,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn paint_with_state(
    target: &ID2D1RenderTarget,
    rows: &[Row],
    dark: bool,
    version: &str,
    now: SystemTime,
    expand_disabled: bool,
    core_ok: bool,
    state: Option<&str>,
) -> Result<()> {
    let (renderer, view, scene) = prepared(rows, dark, now, expand_disabled, core_ok, state)?;
    paint_prepared(target, &renderer, &view, &scene, version, now, state, false)
}
#[allow(clippy::too_many_arguments)]
fn paint_prepared(
    target: &ID2D1RenderTarget,
    renderer: &Renderer,
    view: &v2::View,
    scene: &g::Scene,
    version: &str,
    now: SystemTime,
    state: Option<&str>,
    border: bool,
) -> Result<()> {
    let errors = BTreeSet::new();
    let login = scene.login.as_ref().map(|_| LoginStatus {
        message: crate::i18n::static_text("브라우저에서 로그인을 마치면 자동으로 완료됩니다.")
            .into(),
        countdown: Some(crate::i18n::format("남은 시간 {}", &["9:55".into()])),
        copy_ready: true,
        retry: false,
    });
    let hover = state
        .filter(|s| s.starts_with("menu:"))
        .and_then(|s| s.rsplit(':').next())
        .and_then(|s| s.parse::<usize>().ok())
        .and_then(|i| scene.popup.as_ref()?.items.get(i))
        .map(|b| &b.target);
    let focus_id = state.and_then(|s| s.strip_prefix("focus:"));
    let focus = focus_id
        .and_then(|id| {
            scene
                .targets
                .iter()
                .find(|b| matches!(&b.target,g::Target::Inline{id:row,..} if row==id))
        })
        .map(|b| &b.target);
    let tooltip = state
        .and_then(|s| s.strip_prefix("tooltip:"))
        .and_then(|id| {
            let row = view
                .sections
                .iter()
                .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
                .find(|r| r.raw.id == id)?;
            let rect = scene.row(id)?.usage?;
            renderer
                .tooltip(&row.usage.as_ref()?.tooltip, rect, scene.rect)
                .ok()
        });
    renderer.paint(
        target,
        view,
        scene,
        &RenderOptions {
            version,
            now,
            hover,
            pressed: None,
            focus,
            core_note: None,
            last_success: None,
            errors: &errors,
            login: login.as_ref(),
            tooltip: tooltip.as_ref(),
            draw_window_border: border,
        },
    )
}

struct Apartment(bool);
impl Apartment {
    fn enter() -> Self {
        Self(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_png(
    rows: &[Row],
    scale: f32,
    dark: bool,
    version: &str,
    now: SystemTime,
    expand_disabled: bool,
    output: &Path,
    state: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!([1., 1.5, 2.].contains(&scale), "scale must be 1, 1.5, or 2");
    let _apartment = Apartment::enter();
    let (renderer, view, scene) = prepared(rows, dark, now, expand_disabled, true, Some(state))?;
    let width = (424. * scale).ceil() as u32;
    let height = ((scene.rect.bottom + 64.) * scale).ceil() as u32;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(
            width,
            height,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let target = factory.CreateWicBitmapRenderTarget(
            &bitmap,
            &D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96. * scale,
                dpiY: 96. * scale,
                ..Default::default()
            },
        )?;
        target.BeginDraw();
        target.Clear(Some(&D2D1_COLOR_F {
            r: 0.86,
            g: 0.89,
            b: 0.92,
            a: 1.,
        }));
        target.EndDraw(None, None)?;
        // Translation is in DIPs. Multiplying it by scale again shifted 2x PNGs.
        target.SetTransform(&Matrix3x2::translation(32., 32.));
        paint_prepared(
            &target,
            &renderer,
            &view,
            &scene,
            version,
            now,
            Some(state),
            true,
        )?;
        let stream = wic.CreateStream()?;
        let wide: Vec<u16> = output
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), 0x40000000)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut options = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        let mut format = GUID_WICPixelFormat32bppPBGRA;
        frame.SetPixelFormat(&mut format)?;
        frame.WriteSource(&bitmap, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_state_never_replaces_authoritative_pacing_or_attention() {
        let rows=crate::parse_state(r#"{"accounts":[{"id":"synthetic","provider":"codex","user_state":"active","refresh_attention":true,"pacing":{"backoff_until":"2099-01-01T00:00:00Z","waiting":2}}],"pools":{"codex":{"eligible":1}}}"#).unwrap();
        let now = SystemTime::now();
        for state in [
            None,
            Some("pacing"),
            Some("expanded"),
            Some("menu:synthetic:0"),
        ] {
            let (_, view, _) = prepared(&rows, false, now, false, true, state).unwrap();
            assert_eq!(view.raw.accounts[0].pacing.waiting, 2);
            assert!(view.raw.accounts[0].refresh_attention);
            assert_eq!(
                view.raw.accounts[0].pacing.backoff_until.as_deref(),
                Some("2099-01-01T00:00:00Z")
            );
        }
    }
    #[test]
    fn review_height_uses_v2_scene_and_spec_empty_and_login_dimensions() {
        assert_eq!(state_height(&[], false, None), 298.);
        let rows=crate::parse_state(r#"{"accounts":[{"id":"cl","provider":"claude","user_state":"disabled"},{"id":"cx","provider":"codex","user_state":"disabled"}]}"#).unwrap();
        let (_, _, list) = prepared(&rows, false, SystemTime::now(), false, true, None).unwrap();
        assert_eq!(state_height(&rows, false, None), list.rect.bottom);
        assert_eq!(list.header.forecasts.len(), 2);
        assert_eq!(
            list.header
                .forecasts
                .iter()
                .map(|r| r.bottom - r.top + 6.)
                .sum::<f32>(),
            44.
        );
        assert_eq!(list.rect.bottom, 419. + 44.);
        assert_eq!(state_height(&rows, false, Some("login:claude")), 337. + 44.);
        let (_, _, scene) = prepared(
            &rows,
            false,
            SystemTime::now(),
            false,
            true,
            Some("menu:cl:0"),
        )
        .unwrap();
        let popup = scene.popup.unwrap();
        assert!(popup.rect.top >= 8. && popup.rect.bottom <= scene.footer.rect.top);
    }
}
