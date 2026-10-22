//! Offline PNG adapter for the same view models and DirectWrite renderers as HWNDs.
use crate::{dashboard_model as model, render, state, v2_geometry as flyout, v2_render};
use chrono::{DateTime, FixedOffset};
use std::{collections::BTreeSet, path::Path};
use windows::{
    Win32::{
        Foundation::E_FAIL,
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1CreateFactory, ID2D1Factory,
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
    core::{Error, PCWSTR},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Overview,
    Usage,
    Events,
    Flyout,
    Menu,
}
impl std::str::FromStr for View {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "overview" => Ok(Self::Overview),
            "usage" => Ok(Self::Usage),
            "events" => Ok(Self::Events),
            "flyout" => Ok(Self::Flyout),
            "menu" => Ok(Self::Menu),
            _ => Err("invalid snapshot view"),
        }
    }
}

pub struct Options {
    pub view: View,
    /// Logical dimensions; output pixels are multiplied by scale.
    pub size: [u32; 2],
    pub scale: u32,
    pub dark: bool,
    pub now: DateTime<FixedOffset>,
}

pub fn size(value: &str) -> Result<[u32; 2], &'static str> {
    let (width, height) = value.split_once('x').ok_or("size must be WxH")?;
    Ok([
        width.parse().map_err(|_| "invalid width")?,
        height.parse().map_err(|_| "invalid height")?,
    ])
}

struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn dashboard_state(view: View) -> state::DashboardState {
    state::DashboardState {
        range: state::Range::All,
        tab: match view {
            View::Usage => state::Tab::Usage,
            View::Events => state::Tab::Events,
            _ => state::Tab::Overview,
        },
        ..Default::default()
    }
}

fn flyout_presentation(
    data: &model::DashboardSnapshot,
    view: View,
) -> anyhow::Result<flyout::Presentation> {
    let mut presentation = flyout::Presentation {
        core_ok: data.connection.core_ok,
        ..Default::default()
    };
    if view == View::Menu {
        let row = data
            .accounts
            .sections
            .iter()
            .flat_map(|s| &s.rows)
            .next()
            .ok_or_else(|| anyhow::anyhow!("menu requires an account"))?;
        presentation.overlay = flyout::Overlay::RowMenu(row.raw.id.clone());
    }
    Ok(presentation)
}

/// A production scene plus unellipsized DirectWrite metrics for its single-line text.
/// All coordinates and advances are logical DIPs, including at output scale 2.
/// Wrapped paragraphs are deliberately excluded: their renderer token splitting
/// is a separate contract. This helper does not alter the scene or paint path.
#[derive(Debug)]
pub struct MeasuredDashboard {
    pub scene: crate::geometry::Scene,
    pub text: Vec<MeasuredText>,
}

#[derive(Debug)]
pub struct MeasuredText {
    pub draw_index: usize,
    pub rect: crate::Rect,
    /// Intersection of the client rectangle and active production draw clips.
    pub clip: crate::Rect,
    pub natural_width: f32,
    pub required_height: f32,
    pub trimmed: bool,
}

/// Build through the same project -> Renderer::scene path used by `save`.
/// Metrics use the production text engine/fonts/style, not character estimates
/// or the already-trimmed displayed width. No file or live client is opened.
pub fn measure_dashboard(
    data: &model::DashboardSnapshot,
    options: &Options,
) -> anyhow::Result<MeasuredDashboard> {
    use crate::{
        geometry::{Draw, Face, TextAlign},
        v2_text,
    };
    use windows::Win32::Graphics::{
        Direct2D::Common::D2D_RECT_F,
        DirectWrite::{DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD},
    };
    anyhow::ensure!(
        matches!(options.view, View::Overview | View::Usage | View::Events),
        "dashboard view required"
    );
    anyhow::ensure!([1, 2].contains(&options.scale), "scale must be 1 or 2");
    anyhow::ensure!(
        options.size.iter().all(|n| (240..=4096).contains(n)),
        "snapshot dimensions must be between 240 and 4096"
    );
    let _apartment = Apartment(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok());
    let mut state = dashboard_state(options.view);
    let start = options
        .now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_local_timezone(*options.now.offset())
        .single()
        .expect("fixed offset");
    let clock = model::Clock {
        now: options.now,
        day_start: start.to_utc(),
        day_end: (start + chrono::Duration::days(1)).to_utc(),
    };
    let view = model::project(data, &mut state, &clock);
    let renderer = render::Renderer::new(options.dark)?;
    let scene = renderer.scene(&view, &state, options.size.map(|n| n as f32))?;
    let text_engine = v2_text::Text::new()?;
    let mut clips = vec![scene.rect];
    let mut text = Vec::new();
    for (draw_index, draw) in scene.draw.iter().enumerate() {
        match draw {
            Draw::Clip(r) => {
                let prior = *clips.last().expect("client clip");
                clips.push(crate::Rect {
                    left: prior.left.max(r.left),
                    top: prior.top.max(r.top),
                    right: prior.right.min(r.right),
                    bottom: prior.bottom.min(r.bottom),
                });
            }
            Draw::Unclip => {
                anyhow::ensure!(clips.len() > 1, "unbalanced scene clip");
                clips.pop();
            }
            Draw::Text {
                rect, measure: m, ..
            } if !m.wrap => {
                let metrics = text_engine.measure(
                    &m.text,
                    D2D_RECT_F {
                        left: rect.left,
                        top: rect.top,
                        right: rect.right.max(rect.left + 1.),
                        bottom: rect.bottom.max(rect.top),
                    },
                    v2_text::Style {
                        face: match m.face {
                            Face::Text => v2_text::Face::Text,
                            Face::Mono => v2_text::Face::Mono,
                            Face::Icon => v2_text::Face::Icon,
                        },
                        size: m.size,
                        line_height: m.line_height,
                        weight: if m.bold {
                            DWRITE_FONT_WEIGHT_SEMI_BOLD
                        } else {
                            DWRITE_FONT_WEIGHT_NORMAL
                        },
                        align: match m.align {
                            TextAlign::Leading => v2_text::Align::Leading,
                            TextAlign::Center => v2_text::Align::Center,
                        },
                        wrap: v2_text::Wrap::SingleLine,
                    },
                )?;
                text.push(MeasuredText {
                    draw_index,
                    rect: *rect,
                    clip: *clips.last().expect("client clip"),
                    natural_width: metrics.natural_width,
                    required_height: metrics.required_height,
                    trimmed: metrics.trimmed,
                });
            }
            _ => {}
        }
    }
    anyhow::ensure!(clips.len() == 1, "unbalanced scene clip");
    Ok(MeasuredDashboard { scene, text })
}

pub fn save(data: &model::DashboardSnapshot, options: &Options, path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!([1, 2].contains(&options.scale), "scale must be 1 or 2");
    anyhow::ensure!(
        options.size.iter().all(|n| (240..=4096).contains(n)),
        "snapshot dimensions must be between 240 and 4096"
    );
    let [width, height] = options.size.map(|n| n * options.scale);
    anyhow::ensure!(
        u64::from(width) * u64::from(height) <= 16_777_216,
        "snapshot pixel limit"
    );
    let _apartment = Apartment(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok());
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
        let dpi = 96. * options.scale as f32;
        let target = factory.CreateWicBitmapRenderTarget(
            &bitmap,
            &D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            },
        )?;
        let dimensions = options.size.map(|n| n as f32);
        match options.view {
            View::Overview | View::Usage | View::Events => {
                let mut state = dashboard_state(options.view);
                let midnight = options
                    .now
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight");
                let start = midnight
                    .and_local_timezone(*options.now.offset())
                    .single()
                    .expect("fixed offset");
                let clock = model::Clock {
                    now: options.now,
                    day_start: start.to_utc(),
                    day_end: (start + chrono::Duration::days(1)).to_utc(),
                };
                let view = model::project(data, &mut state, &clock);
                let renderer = render::Renderer::new(options.dark)?;
                let scene = renderer.scene(&view, &state, dimensions)?;
                renderer.paint(&target, &scene, &render::Interaction::default())?;
            }
            View::Flyout | View::Menu => {
                anyhow::ensure!(dimensions[0] >= 360., "flyout width must be at least 360");
                let mut presentation = flyout_presentation(data, options.view)?;
                let renderer = v2_render::Renderer::new(options.dark)?;
                renderer.measure(&data.accounts, &mut presentation)?;
                let scene = flyout::Scene::build(&data.accounts, presentation, dimensions[1]);
                target.BeginDraw();
                let background = if options.dark { 0.07 } else { 0.96 };
                target.Clear(Some(&D2D1_COLOR_F {
                    r: background,
                    g: background,
                    b: background,
                    a: 1.,
                }));
                target.EndDraw(None, None)?;
                target.SetTransform(&windows_numerics::Matrix3x2::translation(
                    (dimensions[0] - 360.) / 2.,
                    0.,
                ));
                renderer.paint(
                    &target,
                    &data.accounts,
                    &scene,
                    &v2_render::RenderOptions {
                        version: &data.accounts.raw.version,
                        now: std::time::SystemTime::from(options.now.to_utc()),
                        hover: None,
                        pressed: None,
                        focus: None,
                        core_note: (!data.connection.core_ok)
                            .then(|| crate::i18n::static_text("코어에 연결할 수 없습니다")),
                        last_success: None,
                        errors: &BTreeSet::new(),
                        login: None,
                        tooltip: None,
                        draw_window_border: true,
                    },
                )?;
            }
        }
        let stream = wic.CreateStream()?;
        let wide: Vec<_> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), 0x40000000)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let (mut frame, mut properties) = (None, None);
        encoder.CreateNewFrame(&mut frame, &mut properties)?;
        let frame = frame.ok_or_else(|| Error::from_hresult(E_FAIL))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        frame.SetResolution(dpi as f64, dpi as f64)?;
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
    #[test]
    fn r16_size_and_view_are_explicit() {
        assert_eq!(super::size("1280x800").unwrap(), [1280, 800]);
        assert!(super::size("1280x800x2").is_err());
        assert!(super::size("bad").is_err());
        assert!("overview".parse::<super::View>().is_ok());
        assert!("unknown".parse::<super::View>().is_err());
    }
    #[test]
    fn r16_offline_scene_carries_core_health_and_all_session_range() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-23T14:05:00+09:00").unwrap();
        let accounts = crate::v2::project(r#"{"accounts":[{"id":"sample-acct","provider":"codex","user_state":"active","health":{"kind":"ready"}}]}"#, at.to_utc().into()).unwrap();
        let mut data = crate::dashboard_model::DashboardSnapshot::new(accounts);
        for healthy in [true, false] {
            data.connection.core_ok = healthy;
            for view in [super::View::Flyout, super::View::Menu] {
                let presentation = super::flyout_presentation(&data, view).unwrap();
                let scene = crate::v2_geometry::Scene::build(&data.accounts, presentation, 900.);
                assert_eq!(scene.core_ok, healthy);
                assert_eq!(scene.header.banner.is_some(), !healthy);
            }
        }
        data.sessions = serde_json::from_value(serde_json::json!({"sessions":[{
            "id":"0123456789ab","provider":"codex","account":"sample-acct",
            "last_used_at":at.to_utc()-chrono::Duration::hours(2),"active":false
        }]}))
        .unwrap();
        let mut state = super::dashboard_state(super::View::Overview);
        let clock = crate::dashboard_model::Clock {
            now: at,
            day_start: at.to_utc() - chrono::Duration::hours(14),
            day_end: at.to_utc() + chrono::Duration::hours(10),
        };
        let view = crate::dashboard_model::project(&data, &mut state, &clock);
        assert_eq!(
            view.session_count, 1,
            "all-range snapshot lost an older session"
        );
    }

    #[test]
    fn r16_move_observation_dates_follow_explicit_clock_offset() {
        let cost: crate::dashboard_model::MoveCost = serde_json::from_value(serde_json::json!({
            "rolling24h":{"observed_since":"2026-10-23T01:00:00Z"}
        }))
        .unwrap();
        crate::i18n::with_language(crate::i18n::UiLanguage::En, || {
            assert!(
                cost.lines_at(chrono::FixedOffset::east_opt(0).unwrap())[1]
                    .contains("Oct 23 01:00")
            );
            assert!(
                cost.lines_at(chrono::FixedOffset::east_opt(32400).unwrap())[1]
                    .contains("Oct 23 10:00")
            );
        });
    }
}
