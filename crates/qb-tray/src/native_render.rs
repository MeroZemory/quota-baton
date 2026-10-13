use super::{Colors, Row, Tone, badge_color, chip_colors, colors};
use std::{path::Path, time::SystemTime};
use windows::{
    Win32::{
        Graphics::{
            Direct2D::{
                Common::{
                    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
                },
                D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1CreateFactory, ID2D1Factory,
                ID2D1RenderTarget,
            },
            DirectWrite::{
                DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_WEIGHT_NORMAL, DWriteCreateFactory, IDWriteFactory,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Imaging::{
                CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppPBGRA,
                IWICImagingFactory, WICBitmapCacheOnLoad,
            },
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        },
    },
    core::{PCWSTR, Result},
};

const WIDTH: f32 = 360.0;
const PADDING: f32 = 12.0;
const SECTION_GAP: f32 = 14.0;
const ROW_HEIGHT: f32 = 104.0;

pub fn content_height(rows: &[Row], expand_disabled: bool) -> f32 {
    let mut height = 56.0;
    for provider in ["Claude", "Codex"] {
        let active = rows
            .iter()
            .filter(|r| r.provider == provider && r.user_chip != "비활성")
            .count();
        let disabled = rows
            .iter()
            .filter(|r| r.provider == provider && r.user_chip == "비활성")
            .count();
        if active + disabled == 0 {
            continue;
        }
        height += SECTION_GAP + active as f32 * ROW_HEIGHT;
        if expand_disabled {
            if disabled > 0 {
                height += 28.0;
            }
            height += disabled as f32 * ROW_HEIGHT;
        } else if disabled > 0 {
            height += 28.0;
        }
    }
    height + 30.0
}

#[derive(Clone, Copy)]
struct BrushColor(D2D1_COLOR_F);

fn brush_color(rgb: super::Rgb) -> BrushColor {
    BrushColor(D2D1_COLOR_F {
        r: rgb.0 as f32 / 255.0,
        g: rgb.1 as f32 / 255.0,
        b: rgb.2 as f32 / 255.0,
        a: 1.0,
    })
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

fn text_format(
    factory: &IDWriteFactory,
    size: f32,
    bold: bool,
) -> Result<windows::Win32::Graphics::DirectWrite::IDWriteTextFormat> {
    let family: Vec<u16> = "Segoe UI Variable\0".encode_utf16().collect();
    let locale: Vec<u16> = "ko-kr\0".encode_utf16().collect();
    unsafe {
        factory.CreateTextFormat(
            PCWSTR(family.as_ptr()),
            None,
            if bold {
                windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT_SEMI_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            },
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(locale.as_ptr()),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    target: &ID2D1RenderTarget,
    format: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    value: &str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: BrushColor,
) -> Result<()> {
    let brush = unsafe { target.CreateSolidColorBrush(&color.0, None) }?;
    let text: Vec<u16> = value.encode_utf16().collect();
    unsafe {
        target.DrawText(
            &text,
            format,
            &rect(x, y, x + width, y + height),
            &brush,
            windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_NONE,
            windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL,
        );
        Ok(())
    }
}

fn fill(target: &ID2D1RenderTarget, bounds: D2D_RECT_F, color: BrushColor) -> Result<()> {
    let brush = unsafe { target.CreateSolidColorBrush(&color.0, None) }?;
    unsafe { target.FillRectangle(&bounds, &brush) };
    Ok(())
}

fn pill(target: &ID2D1RenderTarget, bounds: D2D_RECT_F, color: BrushColor) -> Result<()> {
    let brush = unsafe { target.CreateSolidColorBrush(&color.0, None) }?;
    let rounded = windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
        rect: bounds,
        radiusX: 8.0,
        radiusY: 8.0,
    };
    unsafe { target.FillRoundedRectangle(&rounded, &brush) };
    Ok(())
}

fn meter_color(row: &Row, pct: Option<f64>, colors: Colors) -> BrushColor {
    let rgb = match pct {
        Some(value) if value >= 90.0 => colors.critical,
        Some(value) if value >= 70.0 => colors.warning,
        _ => colors.accent,
    };
    let _ = row;
    brush_color(rgb)
}

fn health_colors(row: &Row, dark: bool) -> (BrushColor, BrushColor) {
    let (bg, fg) = chip_colors(dark, row.health_tone, row.user_chip == "비활성");
    (brush_color(bg), brush_color(fg))
}

fn reset_label(row: &Row, first: bool) -> &str {
    if first {
        &row.five_hour.reset_text
    } else {
        &row.seven_day.reset_text
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_meter(
    target: &ID2D1RenderTarget,
    format: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    row: &Row,
    x: f32,
    y: f32,
    width: f32,
    first: bool,
    colors: Colors,
) -> Result<()> {
    let usage = if first {
        &row.five_hour
    } else {
        &row.seven_day
    };
    draw_text(
        target,
        format,
        &format!("{} {}", usage.label, usage.percent_text),
        x,
        y,
        width,
        18.0,
        brush_color(colors.text),
    )?;
    fill(
        target,
        rect(x, y + 21.0, x + width, y + 25.0),
        brush_color(colors.track),
    )?;
    if let Some(pct) = usage.percent {
        fill(
            target,
            rect(
                x,
                y + 21.0,
                x + width * (pct as f32 / 100.0).clamp(0.0, 1.0),
                y + 25.0,
            ),
            meter_color(row, Some(pct), colors),
        )?;
    }
    draw_text(
        target,
        format,
        reset_label(row, first),
        x,
        y + 29.0,
        width,
        17.0,
        brush_color(colors.muted),
    )
}

pub fn paint(
    target: &ID2D1RenderTarget,
    rows: &[Row],
    dark: bool,
    version: &str,
    now: SystemTime,
    expand_disabled: bool,
    core_ok: bool,
) -> Result<()> {
    let colors = colors(dark);
    let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
    let title = text_format(&dwrite, 14.0, true)?;
    let body = text_format(&dwrite, 13.0, false)?;
    let small = text_format(&dwrite, 11.0, false)?;
    let section = text_format(&dwrite, 12.0, true)?;
    unsafe {
        target.BeginDraw();
    }
    fill(
        target,
        rect(0.0, 0.0, WIDTH, content_height(rows, expand_disabled)),
        brush_color(colors.surface),
    )?;
    draw_text(
        target,
        &title,
        "quota-baton",
        PADDING,
        10.0,
        190.0,
        20.0,
        brush_color(colors.text),
    )?;
    draw_text(
        target,
        &small,
        &format!("코어 실행 중 · {version}"),
        PADDING,
        30.0,
        250.0,
        17.0,
        brush_color(colors.muted),
    )?;
    unsafe {
        let brush =
            target.CreateSolidColorBrush(&brush_color(badge_color(rows, core_ok, dark)).0, None)?;
        target.FillEllipse(
            &D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: 344.0, Y: 21.0 },
                radiusX: 4.0,
                radiusY: 4.0,
            },
            &brush,
        );
    }
    let mut y = 56.0;
    for provider in ["claude", "codex"] {
        let active = rows
            .iter()
            .filter(|r| r.provider == super::provider_name(provider) && r.user_chip != "비활성")
            .collect::<Vec<_>>();
        let disabled = rows
            .iter()
            .filter(|r| r.provider == super::provider_name(provider) && r.user_chip == "비활성")
            .collect::<Vec<_>>();
        if active.is_empty() && disabled.is_empty() {
            continue;
        }
        draw_text(
            target,
            &section,
            super::provider_name(provider),
            PADDING,
            y,
            200.0,
            18.0,
            brush_color(colors.muted),
        )?;
        y += SECTION_GAP;
        if expand_disabled && !disabled.is_empty() {
            draw_text(
                target,
                &small,
                &format!("비활성 계정 {}개 ⌃", disabled.len()),
                PADDING,
                y,
                220.0,
                22.0,
                brush_color(colors.muted),
            )?;
            y += 28.0;
        }
        for row in active.into_iter().chain(if expand_disabled {
            disabled.clone()
        } else {
            Vec::new()
        }) {
            draw_row(target, &body, &small, row, y, colors, dark, now)?;
            y += ROW_HEIGHT;
        }
        if !expand_disabled && !disabled.is_empty() {
            draw_text(
                target,
                &small,
                &format!(
                    "비활성 계정 {}개 {}",
                    disabled.len(),
                    if expand_disabled { "⌃" } else { "›" }
                ),
                PADDING,
                y,
                220.0,
                22.0,
                brush_color(colors.muted),
            )?;
            y += 28.0;
        }
    }
    draw_text(
        target,
        &small,
        &format!("갱신 {}", now_time(now)),
        PADDING,
        y + 10.0,
        120.0,
        20.0,
        brush_color(colors.muted),
    )?;
    unsafe {
        target.EndDraw(None, None)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    target: &ID2D1RenderTarget,
    body: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    small: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    row: &Row,
    y: f32,
    colors: Colors,
    dark: bool,
    now: SystemTime,
) -> Result<()> {
    let (bg, fg) = health_colors(row, dark);
    let name = row.email.as_deref().unwrap_or(&row.name);
    draw_text(
        target,
        body,
        name,
        PADDING,
        y + 10.0,
        150.0,
        20.0,
        brush_color(colors.text),
    )?;
    if let Some(plan) = row.plan.as_deref().filter(|p| !p.is_empty()) {
        pill(
            target,
            rect(
                145.0,
                y + 11.0,
                145.0 + plan.len() as f32 * 7.0 + 14.0,
                y + 30.0,
            ),
            brush_color(colors.track),
        )?;
        draw_text(
            target,
            small,
            plan,
            152.0,
            y + 12.0,
            50.0,
            16.0,
            brush_color(colors.muted),
        )?;
    }
    let display_chip = if row.user_chip == "비활성" {
        "비활성"
    } else {
        row.health_chip.as_str()
    };
    let chip_width = display_chip.chars().count() as f32 * 11.0 + 18.0;
    pill(
        target,
        rect(
            WIDTH - PADDING - chip_width,
            y + 10.0,
            WIDTH - PADDING,
            y + 31.0,
        ),
        bg,
    )?;
    draw_text(
        target,
        small,
        display_chip,
        WIDTH - PADDING - chip_width + 9.0,
        y + 12.0,
        chip_width - 10.0,
        17.0,
        fg,
    )?;
    if !row.five_hour.label.is_empty() {
        draw_meter(target, small, row, PADDING, y + 37.0, 160.0, true, colors)?;
    }
    if !row.seven_day.label.is_empty() {
        draw_meter(target, small, row, 188.0, y + 37.0, 160.0, false, colors)?;
    }
    if row.health_tone != Tone::Good && row.user_chip != "비활성" {
        let reason = if row.user_chip == "비활성" {
            "비활성".to_owned()
        } else if row.health_chip == "재로그인 필요" {
            "다시 로그인해야 합니다".to_owned()
        } else if let Some(until) = row.health_chip.strip_prefix("한도 · ") {
            format!("한도 대기 · {until}까지")
        } else {
            row.health_chip.clone()
        };
        draw_text(
            target,
            small,
            &reason,
            PADDING,
            y + 86.0,
            330.0,
            18.0,
            brush_color(colors.muted),
        )?;
    }
    let _ = (now, dark);
    Ok(())
}

fn now_time(now: SystemTime) -> String {
    let value = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:02}:{:02}", (value / 3600) % 24, (value / 60) % 60)
}

pub fn render_png(
    rows: &[Row],
    scale: f32,
    dark: bool,
    version: &str,
    now: SystemTime,
    expand_disabled: bool,
    output: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        [1.0, 1.5, 2.0].contains(&scale),
        "scale must be 1, 1.5, or 2"
    );
    let width = (WIDTH * scale) as u32;
    let height = (content_height(rows, expand_disabled) * scale).ceil() as u32;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(
            width,
            height,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0 * scale,
            dpiY: 96.0 * scale,
            ..Default::default()
        };
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let target = factory.CreateWicBitmapRenderTarget(&bitmap, &props)?;
        target.SetDpi(96.0 * scale, 96.0 * scale);
        paint(&target, rows, dark, version, now, expand_disabled, true)?;
        let stream = wic.CreateStream()?;
        let wide: Vec<u16> = output
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), 0x40000000)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(
            &stream,
            windows::Win32::Graphics::Imaging::WICBitmapEncoderNoCache,
        )?;
        let mut frame = None;
        let mut options = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.unwrap();
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        let mut pixel_format = GUID_WICPixelFormat32bppPBGRA;
        frame.SetPixelFormat(&mut pixel_format)?;
        frame.WriteSource(&bitmap, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}
