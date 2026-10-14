use super::{Colors, Row, Tone, Usage, badge_color, chip_colors, colors, row_height};
use std::{path::Path, time::SystemTime};
use windows::{
    Win32::{
        Graphics::{
            Direct2D::{
                Common::{
                    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
                },
                D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_ROUNDED_RECT, D2D1CreateFactory,
                ID2D1Factory, ID2D1RenderTarget,
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
use windows_numerics::Matrix3x2;

const WIDTH: f32 = 360.0;
const PADDING: f32 = 16.0;
const SECTION_GAP: f32 = 36.0;

pub fn content_height(rows: &[Row], expand_disabled: bool) -> f32 {
    let mut height = 52.0;
    let mut sections = 0;
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
        sections += 1;
        height += SECTION_GAP
            + rows
                .iter()
                .filter(|row| row.provider == provider && row.user_chip != "비활성")
                .map(row_height)
                .sum::<f32>();
        let expand_section = expand_disabled && provider == "Codex";
        if expand_section {
            if disabled > 0 {
                height += 32.0;
            }
            height += rows
                .iter()
                .filter(|row| row.provider == provider && row.user_chip == "비활성")
                .map(row_height)
                .sum::<f32>();
            if disabled > 0 {
                height += 1.0;
            }
        } else if disabled > 0 {
            height += 32.0;
        }
    }
    // One-pixel section/body separators plus the final list inset.
    height + sections as f32 + 3.0 + 8.0 + 48.0
}

pub fn state_height(rows: &[Row], expand_disabled: bool, state: Option<&str>) -> f32 {
    if state.is_some_and(|value| value.starts_with("login:")) {
        276.0
    } else {
        content_height(rows, expand_disabled)
    }
}

fn display_rows(rows: &[Row], state: Option<&str>) -> Vec<Row> {
    if state.is_none() || state.is_some_and(|value| value == "pacing") {
        rows.to_vec()
    } else {
        rows.iter()
            .cloned()
            .map(|mut row| {
                if row.pacing_until.is_some() {
                    row.health_chip = "준비됨".to_owned();
                }
                row.pacing_until = None;
                if row.health_chip == "준비됨" {
                    row.health_tone = Tone::Good;
                    row.attention = false;
                }
                row
            })
            .collect()
    }
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

fn icon_format(
    factory: &IDWriteFactory,
    size: f32,
) -> Result<windows::Win32::Graphics::DirectWrite::IDWriteTextFormat> {
    let family: Vec<u16> = "Segoe Fluent Icons\0".encode_utf16().collect();
    let locale: Vec<u16> = "en-us\0".encode_utf16().collect();
    unsafe {
        factory.CreateTextFormat(
            PCWSTR(family.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
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

fn popup_surface(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    dark: bool,
    fill_color: BrushColor,
) -> Result<()> {
    let shadow = D2D1_ROUNDED_RECT {
        rect: rect(
            bounds.left,
            bounds.top + 4.0,
            bounds.right,
            bounds.bottom + 4.0,
        ),
        radiusX: 8.0,
        radiusY: 8.0,
    };
    let shadow_color = BrushColor(D2D1_COLOR_F {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: if dark { 0.37 } else { 0.14 },
    });
    let shadow_brush = unsafe { target.CreateSolidColorBrush(&shadow_color.0, None) }?;
    unsafe { target.FillRoundedRectangle(&shadow, &shadow_brush) };
    let rounded = D2D1_ROUNDED_RECT {
        rect: bounds,
        radiusX: 8.0,
        radiusY: 8.0,
    };
    let fill_brush = unsafe { target.CreateSolidColorBrush(&fill_color.0, None) }?;
    let border = unsafe {
        target.CreateSolidColorBrush(
            &brush_color(if dark {
                super::Rgb(71, 71, 71)
            } else {
                super::Rgb(212, 212, 212)
            })
            .0,
            None,
        )?
    };
    unsafe {
        target.FillRoundedRectangle(&rounded, &fill_brush);
        target.DrawRoundedRectangle(&rounded, &border, 1.0, None);
    }
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

#[allow(clippy::too_many_arguments)]
fn draw_meter_value(
    target: &ID2D1RenderTarget,
    format: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    row: &Row,
    usage: &Usage,
    x: f32,
    y: f32,
    width: f32,
    colors: Colors,
) -> Result<()> {
    draw_text(
        target,
        format,
        &format!("{} {}", usage.label, usage.percent_text),
        x,
        y,
        width,
        16.0,
        brush_color(colors.text),
    )?;
    fill(
        target,
        rect(x, y + 20.0, x + width, y + 24.0),
        brush_color(colors.track),
    )?;
    if let Some(pct) = usage.percent {
        let fill_width = (width * (pct as f32 / 100.0).clamp(0.0, 1.0)).max(4.0);
        fill(
            target,
            rect(x, y + 20.0, x + fill_width, y + 24.0),
            meter_color(row, Some(pct), colors),
        )?;
    }
    draw_text(
        target,
        format,
        &usage.reset_text,
        x,
        y + 28.0,
        width,
        14.0,
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
    let rows = display_rows(rows, state);
    let colors = colors(dark);
    let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
    let title = text_format(&dwrite, 14.0, true)?;
    let body = text_format(&dwrite, 13.0, false)?;
    let small = text_format(&dwrite, 11.0, false)?;
    let section = text_format(&dwrite, 12.0, true)?;
    let icons = icon_format(&dwrite, 14.0)?;
    unsafe {
        target.BeginDraw();
    }
    fill(
        target,
        rect(0.0, 0.0, WIDTH, state_height(&rows, expand_disabled, state)),
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
    let status = if core_ok {
        format!("코어 실행 중 · {version}")
    } else {
        "코어에 연결할 수 없음".to_owned()
    };
    draw_text(
        target,
        &small,
        &status,
        PADDING,
        30.0,
        250.0,
        17.0,
        brush_color(colors.muted),
    )?;
    unsafe {
        let brush = target
            .CreateSolidColorBrush(&brush_color(badge_color(&rows, core_ok, dark)).0, None)?;
        target.FillEllipse(
            &D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: 344.0, Y: 21.0 },
                radiusX: 4.0,
                radiusY: 4.0,
            },
            &brush,
        );
    }
    let login_state = state.is_some_and(|value| value == "login:claude" || value == "login:codex");
    let mut y = 52.0;
    if !login_state {
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
            let expand_section = expand_disabled && provider == "codex";
            if expand_section && !disabled.is_empty() {
                draw_text(
                    target,
                    &small,
                    &format!("비활성 계정 {}개 ⌄", disabled.len()),
                    PADDING,
                    y,
                    220.0,
                    22.0,
                    brush_color(colors.muted),
                )?;
                y += 28.0;
            }
            for row in active.into_iter().chain(if expand_section {
                disabled.clone()
            } else {
                Vec::new()
            }) {
                draw_row(target, &body, &small, row, y, colors, dark, now)?;
                y += row_height(row);
                fill(
                    target,
                    rect(PADDING, y - 1.0, WIDTH - PADDING, y),
                    brush_color(colors.divider),
                )?;
            }
            if !expand_section && !disabled.is_empty() {
                draw_text(
                    target,
                    &small,
                    &format!(
                        "비활성 계정 {}개 {}",
                        disabled.len(),
                        if expand_section { "⌄" } else { "›" }
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
    } else {
        let provider = if state.is_some_and(|value| value.ends_with("claude")) {
            "Claude"
        } else {
            "Codex"
        };
        draw_text(
            target,
            &title,
            &format!("{provider} 로그인"),
            16.0,
            68.0,
            260.0,
            20.0,
            brush_color(colors.text),
        )?;
        draw_text(
            target,
            &body,
            "브라우저에서 로그인을 마치면 자동으로",
            16.0,
            100.0,
            328.0,
            20.0,
            brush_color(colors.text),
        )?;
        draw_text(
            target,
            &body,
            "완료됩니다.",
            16.0,
            120.0,
            328.0,
            20.0,
            brush_color(colors.text),
        )?;
        draw_text(
            target,
            &small,
            "남은 시간 9:41",
            42.0,
            148.0,
            180.0,
            16.0,
            brush_color(colors.muted),
        )?;
        for (x, label) in [(16.0, "주소 복사"), (184.0, "취소")] {
            pill(
                target,
                rect(x, 180.0, x + 160.0, 212.0),
                brush_color(colors.surface),
            )?;
            draw_text(
                target,
                &body,
                label,
                x + 52.0,
                187.0,
                80.0,
                18.0,
                brush_color(colors.text),
            )?;
        }
        y = 228.0;
    }
    fill(
        target,
        rect(0.0, y, WIDTH, y + 48.0),
        brush_color(colors.background),
    )?;
    fill(
        target,
        rect(0.0, y, WIDTH, y + 1.0),
        brush_color(colors.divider),
    )?;
    draw_text(
        target,
        &small,
        &format!("갱신 {}", now_time(now)),
        PADDING,
        y + 10.0,
        74.0,
        20.0,
        brush_color(colors.muted),
    )?;
    for (x, width, icon, label) in [
        (88.0, 81.0, "\u{E710}", "계정 추가"),
        (171.0, 54.0, "\u{E713}", "설정"),
        (227.0, 54.0, "\u{E8A5}", "로그"),
        (283.0, 54.0, "\u{E7E8}", "종료"),
    ] {
        pill(
            target,
            rect(x, y + 8.0, x + width, y + 40.0),
            brush_color(colors.background),
        )?;
        draw_text(
            target,
            &icons,
            icon,
            x + 6.0,
            y + 15.0,
            14.0,
            16.0,
            brush_color(colors.text),
        )?;
        draw_text(
            target,
            &small,
            label,
            x + 24.0,
            y + 16.0,
            width - 28.0,
            16.0,
            brush_color(colors.text),
        )?;
    }
    if let Some(state) = state.filter(|value| {
        !matches!(*value, "list" | "expanded" | "pacing" | "truncation")
            && !value.starts_with("login:")
    }) {
        let (heading, lines): (&str, Vec<String>) = if state.starts_with("menu:") {
            let id = state
                .strip_prefix("menu:")
                .and_then(|value| value.split(':').next())
                .unwrap_or_default();
            let menu_row = rows
                .iter()
                .find(|row| row.id == id)
                .or_else(|| rows.first());
            let labels = menu_row
                .map(|row| {
                    super::actions::menu_items(row)
                        .into_iter()
                        .map(|item| item.label(row).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            ("계정 메뉴", labels)
        } else if state.starts_with("confirm-delete:") {
            let id = state.strip_prefix("confirm-delete:").unwrap_or_default();
            let email = rows
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.email.as_deref())
                .unwrap_or("이 계정");
            (
                "계정 삭제",
                vec![
                    email.to_owned(),
                    "계정을 quota-baton에서 삭제할까요?".to_owned(),
                    "제공자 쪽 로그인은 취소되지 않습니다.".to_owned(),
                    "[삭제]    [취소]".to_owned(),
                ],
            )
        } else if state == "login:claude" || state == "login:codex" {
            let provider = if state.ends_with("claude") {
                "Claude"
            } else {
                "Codex"
            };
            (
                "로그인",
                vec![
                    provider.to_owned(),
                    "브라우저에서 로그인을 마치면 자동으로 완료됩니다.".to_owned(),
                    "주소 복사   취소".to_owned(),
                    "남은 시간 9:41".to_owned(),
                ],
            )
        } else if state == "quit" {
            (
                "종료",
                vec![
                    "트레이만 종료".to_owned(),
                    "트레이와 프록시 종료".to_owned(),
                    "취소".to_owned(),
                ],
            )
        } else {
            ("계정 메뉴", vec!["알 수 없는 상태".to_owned()])
        };
        let (left, top, right, bottom) = if state.starts_with("menu:") {
            (136.0, 126.0, 352.0, 312.0)
        } else if state.starts_with("confirm-delete:") {
            let card_height = 193.0;
            let flyout = content_height(&rows, expand_disabled);
            (
                16.0,
                (flyout - card_height).round() / 2.0,
                344.0,
                (flyout + card_height).round() / 2.0,
            )
        } else if state == "quit" {
            (
                152.0,
                content_height(&rows, expand_disabled) - 124.0,
                352.0,
                content_height(&rows, expand_disabled) - 52.0,
            )
        } else {
            (28.0, 68.0, WIDTH - 12.0, 200.0)
        };
        if state.starts_with("confirm-delete:") {
            fill(
                target,
                rect(0.0, 0.0, WIDTH, content_height(&rows, expand_disabled)),
                BrushColor(D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: if dark { 0.50 } else { 0.30 },
                }),
            )?;
        }
        popup_surface(
            target,
            rect(left, top, right, bottom),
            dark,
            brush_color(if dark {
                super::Rgb(44, 44, 44)
            } else {
                super::Rgb(252, 252, 252)
            }),
        )?;
        draw_text(
            target,
            &section,
            heading,
            left + 16.0,
            top + 10.0,
            260.0,
            20.0,
            brush_color(colors.text),
        )?;
        for (index, line) in lines.iter().enumerate() {
            let menu_icon = if state.starts_with("menu:") {
                ["\u{e769}", "\u{e733}", "\u{e8d7}", "\u{e72c}", "\u{e74d}"]
                    .get(index)
                    .copied()
            } else {
                None
            };
            if let Some(icon) = menu_icon {
                draw_text(
                    target,
                    &icons,
                    icon,
                    left + 16.0,
                    top + 38.0 + index as f32 * 27.0,
                    16.0,
                    20.0,
                    brush_color(colors.text),
                )?;
            }
            draw_text(
                target,
                if menu_icon.is_some() { &body } else { &small },
                line,
                left + if menu_icon.is_some() { 40.0 } else { 16.0 },
                top + 38.0 + index as f32 * 27.0,
                286.0,
                20.0,
                brush_color(colors.text),
            )?;
        }
        if state.starts_with("confirm-delete:") {
            fill(
                target,
                rect(left, bottom - 64.0, right, bottom),
                brush_color(colors.background),
            )?;
            fill(
                target,
                rect(left, bottom - 64.0, right, bottom - 63.0),
                brush_color(colors.divider),
            )?;
            draw_text(
                target,
                &body,
                "삭제",
                left + 58.0,
                bottom - 44.0,
                70.0,
                18.0,
                brush_color(colors.critical),
            )?;
            draw_text(
                target,
                &body,
                "취소",
                left + 204.0,
                bottom - 44.0,
                70.0,
                18.0,
                brush_color(colors.text),
            )?;
        }
    }
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
    let display_chip = if row.user_chip == "비활성" {
        "비활성"
    } else {
        row.health_chip.as_str()
    };
    let chip_width = display_chip.chars().count() as f32 * 9.0 + 22.0;
    let chip_left = 320.0 - chip_width;
    let plan_width = row
        .plan
        .as_deref()
        .filter(|plan| !plan.is_empty())
        .map(|plan| plan.chars().count() as f32 * 7.0 + 14.0)
        .unwrap_or(0.0);
    let email_width = (chip_left - 8.0 - plan_width - 6.0 - 16.0).max(40.0);
    let email = trim_text(name, (email_width / 7.0).floor() as usize);
    draw_text(
        target,
        body,
        &email,
        PADDING,
        y + 10.0,
        290.0,
        18.0,
        brush_color(colors.text),
    )?;
    // The right edge is laid out first: ellipsis, chip, then the plan pill.
    draw_text(
        target,
        body,
        "⋯",
        330.0,
        y + 10.0,
        22.0,
        20.0,
        brush_color(colors.muted),
    )?;
    if let Some(plan) = row.plan.as_deref().filter(|p| !p.is_empty()) {
        let plan_x = 16.0 + email.chars().count() as f32 * 7.0 + 6.0;
        pill(
            target,
            rect(plan_x, y + 11.0, plan_x + plan_width, y + 29.0),
            brush_color(colors.track),
        )?;
        draw_text(
            target,
            small,
            plan,
            plan_x + 7.0,
            y + 12.0,
            50.0,
            16.0,
            brush_color(colors.muted),
        )?;
    }
    pill(target, rect(chip_left, y + 10.0, 320.0, y + 31.0), bg)?;
    draw_text(
        target,
        small,
        display_chip,
        chip_left + 9.0,
        y + 12.0,
        chip_width - 10.0,
        17.0,
        fg,
    )?;
    if row.user_chip == "비활성" {
        return Ok(());
    }
    for (index, usage) in row.meter_windows.iter().take(2).enumerate() {
        let x = if index == 0 { 16.0 } else { 188.0 };
        draw_meter_value(target, small, row, usage, x, y + 40.0, 156.0, colors)?;
    }
    if row.health_tone != Tone::Good && row.user_chip != "비활성" {
        let reason = if row.user_chip == "비활성" {
            "비활성".to_owned()
        } else if row.health_chip == "재로그인 필요" {
            "다시 로그인해야 합니다".to_owned()
        } else if let Some(until) = row.health_chip.strip_prefix("한도 · ") {
            format!("한도 대기 · {until}까지")
        } else if let Some(seconds) = row.health_chip.strip_prefix("속도 조절 중 · ") {
            format!("요청 속도 조절 중 · {seconds}")
        } else {
            row.health_chip.clone()
        };
        let reason_y = y + if row.meter_windows.is_empty() {
            42.0
        } else {
            90.0
        };
        draw_text(
            target,
            small,
            &reason,
            PADDING,
            reason_y,
            if row.reason_button { 220.0 } else { 328.0 },
            if row.reason_button { 24.0 } else { 16.0 },
            brush_color(colors.muted),
        )?;
        if row.reason_button {
            pill(
                target,
                rect(264.0, reason_y - 4.0, 344.0, reason_y + 20.0),
                brush_color(colors.accent),
            )?;
            draw_text(
                target,
                small,
                "다시 로그인",
                274.0,
                reason_y,
                62.0,
                16.0,
                brush_color(colors.surface),
            )?;
        }
    }
    let _ = (now, dark);
    Ok(())
}

fn now_time(now: SystemTime) -> String {
    let local = chrono::DateTime::<chrono::Local>::from(chrono::DateTime::<chrono::Utc>::from(now));
    local.format("%H:%M").to_string()
}

fn trim_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".into();
    }
    value.chars().take(max_chars - 1).collect::<String>() + "…"
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
    anyhow::ensure!(
        [1.0, 1.5, 2.0].contains(&scale),
        "scale must be 1, 1.5, or 2"
    );
    // Keep the same 32-DIP desktop margin as the reference captures. The
    // margin represents the shell/DWM shadow; the flyout itself is still
    // painted at the spec's 360-DIP origin.
    let margin = (32.0 * scale).ceil() as u32;
    let flyout_width = (WIDTH * scale) as u32;
    let review_rows = display_rows(rows, Some(state));
    let flyout_height =
        (state_height(&review_rows, expand_disabled, Some(state)) * scale).ceil() as u32;
    let width = flyout_width + margin * 2;
    let height = flyout_height + margin * 2;
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
        let desktop = if dark {
            D2D1_COLOR_F {
                r: 14.0 / 255.0,
                g: 16.0 / 255.0,
                b: 19.0 / 255.0,
                a: 1.0,
            }
        } else {
            D2D1_COLOR_F {
                r: 220.0 / 255.0,
                g: 227.0 / 255.0,
                b: 234.0 / 255.0,
                a: 1.0,
            }
        };
        target.BeginDraw();
        target.Clear(Some(&desktop));
        target.EndDraw(None, None)?;
        let transform = Matrix3x2 {
            M11: 1.0,
            M22: 1.0,
            M31: margin as f32,
            M32: margin as f32,
            ..Default::default()
        };
        target.SetTransform(&transform);
        paint_with_state(
            &target,
            rows,
            dark,
            version,
            now,
            expand_disabled,
            true,
            Some(state),
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
