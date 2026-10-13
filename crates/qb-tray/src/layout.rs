//! Pure M6a.1 tray layout and review PNG renderer.
//!
//! This module intentionally knows nothing about Win32, HTTP, or credentials.  The
//! JSON shape is the read-only `/admin/v1/state` shape, so fixture and UI tests can
//! exercise it without starting the core.

use flate2::{Compression, write::ZlibEncoder};
use serde::Deserialize;
use std::{
    io::Write,
    time::{Duration, SystemTime},
};

#[derive(Debug, Clone, Deserialize)]
pub struct State {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub provider: String,
    pub label: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub user_state: String,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub probe_pending: bool,
    #[serde(default)]
    pub windows: Vec<Window>,
    #[serde(default)]
    pub refresh_attention: bool,
    #[serde(default)]
    pub drain: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub kind: String,
    pub reason: Option<String>,
    pub until: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub name: String,
    pub used_pct: Option<f64>,
    pub reset_at: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub provider: String,
    pub name: String,
    pub plan: Option<String>,
    pub user_chip: String,
    pub health_chip: String,
    pub health_tone: Tone,
    pub five_hour: Usage,
    pub seven_day: Usage,
    pub attention: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub percent: Option<f64>,
    pub percent_text: String,
    pub reset_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Normal,
    Attention,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub background: Rgb,
    pub surface: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub good: Rgb,
    pub warning: Rgb,
    pub critical: Rgb,
    pub track: Rgb,
}

pub fn parse_state(json: &str) -> serde_json::Result<Vec<Row>> {
    parse_state_at(json, SystemTime::now())
}

pub fn parse_state_at(json: &str, now: SystemTime) -> serde_json::Result<Vec<Row>> {
    let state: State = serde_json::from_str(json)?;
    Ok(state
        .accounts
        .iter()
        .map(|account| row(account, now))
        .collect())
}

pub fn row(account: &Account, now: SystemTime) -> Row {
    let name = account
        .label
        .clone()
        .or_else(|| account.email.clone())
        .unwrap_or_else(|| "이름 없는 계정".into());
    let health_chip = health_chip(account, now);
    let health_tone = if account.probe_pending || account.drain || account.refresh_attention {
        Tone::Warning
    } else {
        health_tone(account)
    };
    Row {
        provider: provider_name(&account.provider).into(),
        name,
        plan: account.plan.clone(),
        user_chip: user_chip(&account.user_state),
        health_chip,
        health_tone,
        five_hour: usage(account.windows.iter().find(|w| w.name == "5h"), now),
        seven_day: usage(
            account
                .windows
                .iter()
                .find(|w| w.name == "7d")
                .or_else(|| account.windows.iter().find(|w| w.name != "5h")),
            now,
        ),
        attention: account.refresh_attention
            || account.probe_pending
            || account.drain
            || health_tone == Tone::Warning
            || health_tone == Tone::Critical,
    }
}

pub fn user_chip(state: &str) -> String {
    match state {
        "active" => "활성",
        "paused" => "일시정지",
        "disabled" => "비활성",
        _ => "알 수 없음",
    }
    .into()
}

pub fn health_chip(account: &Account, now: SystemTime) -> String {
    if account.probe_pending {
        return "인증 확인 대기".into();
    }
    if account.drain {
        return "전환 대기".into();
    }
    match account.health.kind.as_str() {
        "ready" => "준비됨".into(),
        "cooling" => {
            let label = match account.health.reason.as_deref() {
                Some("quota") => "한도",
                Some("rate") | Some("auth") | Some("link_stale") => "대기",
                Some("permission") | Some("payment") | Some("store_error") => "확인 필요",
                _ => "대기",
            };
            account.health.until.as_deref().map_or_else(
                || label.into(),
                |until| format!("{label} · {}", short_time(until, now)),
            )
        }
        "needs_relogin" => "재로그인 필요".into(),
        _ => "알 수 없음".into(),
    }
}

fn short_time(value: &str, now: SystemTime) -> String {
    let rendered = format_reset(value, now);
    rendered
        .strip_suffix(" 초기화")
        .unwrap_or(&rendered)
        .to_string()
}

pub fn health_tone(account: &Account) -> Tone {
    if account.user_state != "active" {
        return Tone::Neutral;
    }
    match account.health.kind.as_str() {
        "ready" => Tone::Good,
        "cooling" if account.health.reason.as_deref() == Some("store_error") => Tone::Critical,
        "cooling" => Tone::Warning,
        "needs_relogin" => Tone::Critical,
        _ => Tone::Neutral,
    }
}

pub fn provider_name(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        _ => provider,
    }
}

fn usage(window: Option<&Window>, now: SystemTime) -> Usage {
    let Some(window) = window else {
        return Usage {
            percent: None,
            percent_text: "-".into(),
            reset_text: "-".into(),
        };
    };
    let expired = window
        .reset_at
        .as_deref()
        .and_then(parse_rfc3339)
        .is_some_and(|t| t <= now);
    let percent = (!expired)
        .then_some(window.used_pct)
        .flatten()
        .filter(|p| p.is_finite() && *p >= 0.0);
    Usage {
        percent,
        percent_text: percent.map_or_else(
            || {
                if expired {
                    "리셋됨".into()
                } else {
                    "-".into()
                }
            },
            |p| format!("{p:.1}%"),
        ),
        reset_text: window
            .reset_at
            .as_deref()
            .map_or_else(|| "-".into(), |s| format_reset(s, now)),
    }
}

pub fn format_reset(value: &str, now: SystemTime) -> String {
    let Some(at) = parse_rfc3339(value) else {
        return value.into();
    };
    if at <= now {
        return "리셋됨".into();
    }
    let remaining = at.duration_since(now).unwrap_or_default();
    if remaining > Duration::from_secs(24 * 3600) {
        let month = value.get(5..7).unwrap_or("??");
        let day = value.get(8..10).unwrap_or("??");
        let hour = value.get(11..13).unwrap_or("??");
        let minute = value.get(14..16).unwrap_or("??");
        return format!("{month}/{day} {hour}:{minute} 초기화");
    }
    let hours = remaining.as_secs() / 3600;
    if hours > 0 {
        format!("{hours}시간 후 초기화")
    } else {
        format!("{}분 후 초기화", (remaining.as_secs() / 60).max(1))
    }
}

pub fn tooltip(rows: &[Row], core_ok: bool) -> String {
    let badge = match badge(rows, core_ok) {
        Badge::Error => "코어 연결 끊김",
        Badge::Attention => "주의 필요",
        Badge::Normal => "정상",
    };
    let text = format!("quota-baton · {badge} · 계정 {}개", rows.len());
    text.chars().take(127).collect()
}

pub fn badge(rows: &[Row], core_ok: bool) -> Badge {
    if !core_ok {
        Badge::Error
    } else if rows.is_empty()
        || rows
            .iter()
            .any(|r| r.attention || r.health_tone == Tone::Critical)
        || !rows
            .iter()
            .any(|r| r.user_chip == "활성" && r.health_tone == Tone::Good)
    {
        Badge::Attention
    } else {
        Badge::Normal
    }
}

pub fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            background: Rgb(30, 30, 30),
            surface: Rgb(45, 45, 48),
            text: Rgb(245, 245, 245),
            muted: Rgb(180, 180, 185),
            accent: Rgb(91, 156, 255),
            good: Rgb(74, 190, 110),
            warning: Rgb(235, 174, 58),
            critical: Rgb(235, 83, 83),
            track: Rgb(75, 75, 80),
        }
    } else {
        Colors {
            background: Rgb(248, 249, 251),
            surface: Rgb(255, 255, 255),
            text: Rgb(28, 31, 36),
            muted: Rgb(99, 106, 116),
            accent: Rgb(35, 104, 210),
            good: Rgb(30, 145, 73),
            warning: Rgb(174, 108, 0),
            critical: Rgb(190, 45, 45),
            track: Rgb(224, 228, 234),
        }
    }
}

pub fn render_png(rows: &[Row], scale: f32, dark: bool) -> Result<Vec<u8>, String> {
    let factor = match scale {
        1.0 => 1u32,
        1.5 => 3,
        2.0 => 2,
        _ => return Err("scale must be 1, 1.5, or 2".into()),
    };
    let (base_w, base_h) = (360u32, 96 + rows.len() as u32 * 88);
    let (w, h) = if scale == 1.5 {
        (base_w * 3 / 2, base_h * 3 / 2)
    } else {
        (
            base_w * factor / if factor == 3 { 2 } else { 1 },
            base_h * factor / if factor == 3 { 2 } else { 1 },
        )
    };
    let c = colors(dark);
    let mut pixels = vec![c.background; (w * h) as usize];
    let sy = |n: u32| n * if scale == 1.5 { 3 } else { factor } / if scale == 1.5 { 2 } else { 1 };
    text(&mut pixels, w, h, 20, 20, "quota-baton", c.text, scale);
    text(&mut pixels, w, h, 196, 55, "5시간", c.muted, scale);
    text(&mut pixels, w, h, 286, 55, "주간", c.muted, scale);
    for (i, row) in rows.iter().enumerate() {
        let y0 = sy(80 + i as u32 * 88);
        let height = sy(72);
        let x0 = sy(16);
        let width = w.saturating_sub(sy(32));
        fill(&mut pixels, w, h, x0, y0, width, height, c.surface);
        let tone = match row.health_tone {
            Tone::Good => c.good,
            Tone::Warning => c.warning,
            Tone::Critical => c.critical,
            Tone::Neutral => c.muted,
        };
        fill(&mut pixels, w, h, x0, y0, sy(5), height, tone);
        let y = 80 + i as u32 * 88;
        text(&mut pixels, w, h, 28, y + 8, &row.provider, c.muted, scale);
        text(
            &mut pixels,
            w,
            h,
            28,
            y + 24,
            &row.name.chars().take(12).collect::<String>(),
            c.text,
            scale,
        );
        text(&mut pixels, w, h, 28, y + 46, &row.health_chip, tone, scale);
        for (x, usage) in [(196, &row.five_hour), (286, &row.seven_day)] {
            text(
                &mut pixels,
                w,
                h,
                x,
                y + 8,
                &usage.percent_text,
                c.text,
                scale,
            );
            text(
                &mut pixels,
                w,
                h,
                x,
                y + 43,
                &usage.reset_text,
                c.muted,
                scale,
            );
        }
        bar(
            &mut pixels,
            w,
            h,
            x0 + sy(180),
            y0 + sy(27),
            sy(130),
            row.five_hour.percent,
            c,
            scale,
        );
        bar(
            &mut pixels,
            w,
            h,
            x0 + sy(270),
            y0 + sy(27),
            sy(130),
            row.seven_day.percent,
            c,
            scale,
        );
    }
    png_rgba(w, h, &pixels)
}

// A deliberately small, embedded stroke font keeps review rendering reproducible
// without installed fonts. Hangul syllables are composed from their three jamo.
#[allow(clippy::too_many_arguments)]
fn text(px: &mut [Rgb], w: u32, h: u32, x: u32, y: u32, value: &str, color: Rgb, scale: f32) {
    let mut cursor = x;
    for ch in value.chars() {
        let mut dot = |xx: u32, yy: u32| {
            let left = ((cursor + xx) as f32 * scale) as u32;
            let top = ((y + yy) as f32 * scale) as u32;
            let right = ((cursor + xx + 1) as f32 * scale) as u32;
            let bottom = ((y + yy + 1) as f32 * scale) as u32;
            fill(px, w, h, left, top, right - left, bottom - top, color);
        };
        let code = ch as u32;
        if (0xac00..=0xd7a3).contains(&code) {
            let syllable = code - 0xac00;
            let initial = syllable / 588;
            let vowel = (syllable / 28) % 21;
            let final_jamo = syllable % 28;
            let horizontal = matches!(vowel, 8..=13 | 17..=18);
            let body_h = if final_jamo == 0 { 11 } else { 8 };
            if horizontal {
                jamo(&mut dot, consonant(initial), 1, 0, 10, body_h / 2);
                jamo(&mut dot, vowel_bits(vowel), 1, body_h / 2, 10, body_h / 2);
            } else {
                jamo(&mut dot, consonant(initial), 0, 0, 6, body_h);
                jamo(&mut dot, vowel_bits(vowel), 7, 0, 5, body_h);
            }
            if final_jamo != 0 {
                let finals = [
                    0, 0, 1, 0, 2, 2, 2, 3, 5, 5, 5, 5, 5, 5, 5, 5, 6, 7, 7, 9, 10, 11, 12, 14, 15,
                    16, 17, 18,
                ];
                jamo(&mut dot, consonant(finals[final_jamo as usize]), 2, 9, 8, 4);
            }
            cursor += 14;
        } else {
            jamo(&mut dot, ascii(ch), 0, 2, 5, 7);
            cursor += 7;
        }
    }
}

fn jamo(dot: &mut impl FnMut(u32, u32), bits: [u8; 7], x: u32, y: u32, width: u32, height: u32) {
    for yy in 0..height {
        for xx in 0..width {
            if bits[(yy * 7 / height) as usize] & (1 << (4 - xx * 5 / width)) != 0 {
                dot(x + xx, y + yy);
            }
        }
    }
}

fn consonant(index: u32) -> [u8; 7] {
    let patterns = [
        [31, 1, 1, 1, 1, 1, 1],
        [27, 9, 9, 9, 9, 9, 9],
        [16, 16, 16, 16, 16, 16, 31],
        [31, 16, 16, 16, 16, 16, 31],
        [27, 18, 18, 18, 18, 18, 27],
        [31, 1, 1, 31, 16, 16, 31],
        [31, 17, 17, 17, 17, 17, 31],
        [17, 17, 17, 31, 17, 17, 31],
        [27, 27, 27, 31, 27, 27, 31],
        [4, 4, 10, 10, 17, 17, 17],
        [10, 10, 21, 21, 21, 21, 21],
        [14, 17, 17, 17, 17, 17, 14],
        [31, 4, 4, 10, 10, 17, 17],
        [27, 10, 10, 21, 21, 21, 21],
        [4, 31, 4, 10, 10, 17, 17],
        [31, 1, 1, 31, 1, 1, 1],
        [31, 16, 16, 31, 16, 16, 31],
        [31, 10, 10, 10, 10, 10, 31],
        [4, 31, 0, 14, 17, 17, 14],
    ];
    patterns[index as usize]
}

fn vowel_bits(index: u32) -> [u8; 7] {
    match index {
        0 => [16, 16, 16, 30, 16, 16, 16],
        1 => [18, 18, 18, 30, 18, 18, 18],
        2 => [16, 16, 30, 16, 30, 16, 16],
        3 => [18, 18, 30, 18, 30, 18, 18],
        4 => [1, 1, 1, 15, 1, 1, 1],
        5 => [9, 9, 9, 15, 9, 9, 9],
        6 => [1, 1, 15, 1, 15, 1, 1],
        7 => [9, 9, 15, 9, 15, 9, 9],
        8 => [4, 4, 4, 4, 4, 31, 0],
        12 => [10, 10, 10, 10, 10, 31, 0],
        13 => [0, 31, 4, 4, 4, 4, 4],
        17 => [0, 31, 10, 10, 10, 10, 10],
        18 => [0, 0, 0, 31, 0, 0, 0],
        20 => [4, 4, 4, 4, 4, 4, 4],
        19 => [1, 1, 1, 31, 1, 1, 1],
        // Compound vowels retain both the horizontal and vertical stroke.
        9..=11 => [5, 5, 5, 15, 5, 31, 1],
        14..=16 => [1, 31, 5, 15, 5, 5, 5],
        _ => [31, 17, 17, 17, 17, 17, 31],
    }
}

fn ascii(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 2, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 25, 21, 19, 19, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '%' => [25, 25, 2, 4, 8, 19, 19],
        ':' => [0, 4, 4, 0, 4, 4, 0],
        '.' => [0, 0, 0, 0, 0, 4, 4],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        ' ' => [0; 7],
        _ => [31, 17, 21, 21, 21, 17, 31],
    }
}

#[allow(clippy::too_many_arguments)]
fn bar(
    px: &mut [Rgb],
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    width: u32,
    pct: Option<f64>,
    c: Colors,
    scale: f32,
) {
    fill(px, w, h, x, y, width, (4.0 * scale) as u32, c.track);
    if let Some(p) = pct {
        fill(
            px,
            w,
            h,
            x,
            y,
            ((width as f64 * (p / 100.0).clamp(0.0, 1.0)) as u32).min(width),
            (4.0 * scale) as u32,
            if p >= 100.0 { c.critical } else { c.accent },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn fill(px: &mut [Rgb], w: u32, h: u32, x: u32, y: u32, width: u32, height: u32, color: Rgb) {
    for yy in y.min(h)..y.saturating_add(height).min(h) {
        for xx in x.min(w)..x.saturating_add(width).min(w) {
            px[(yy * w + xx) as usize] = color;
        }
    }
}

fn png_rgba(w: u32, h: u32, pixels: &[Rgb]) -> Result<Vec<u8>, String> {
    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for row in pixels.chunks(w as usize) {
        raw.push(0);
        for &Rgb(r, g, b) in row {
            raw.extend([r, g, b, 255]);
        }
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).map_err(|e| e.to_string())?;
    let data = encoder.finish().map_err(|e| e.to_string())?;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(
        &mut out,
        b"IHDR",
        &[
            w.to_be_bytes().as_slice(),
            h.to_be_bytes().as_slice(),
            &[8, 6, 0, 0, 0],
        ]
        .concat(),
    );
    chunk(&mut out, b"IDAT", &data);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    out.extend(crc32(&[kind.as_slice(), data].concat()).to_be_bytes());
}

/// Restart policy used by the native tray supervisor. Keeping it pure makes
/// the policy testable without starting a process or a core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorBackoff {
    seconds: u64,
}

impl Default for SupervisorBackoff {
    fn default() -> Self {
        Self { seconds: 5 }
    }
}

impl SupervisorBackoff {
    pub fn delay_seconds(&self) -> u64 {
        self.seconds
    }
    pub fn next(&mut self) {
        self.seconds = (self.seconds * 2).min(300);
    }
    pub fn reset(&mut self) {
        self.seconds = 5;
    }
}
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn parse_rfc3339(value: &str) -> Option<SystemTime> {
    // Reuse the core's chrono-backed timestamp without another dependency.
    let meta = qb_core::accounts::TokenMeta {
        expires_at: serde_json::from_value(serde_json::Value::String(value.into())).ok(),
        ..Default::default()
    };
    meta.expires_at.map(SystemTime::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;
    const FIXTURE: &str = include_str!("../fixtures/m6a.1-state.json");
    fn rows() -> Vec<Row> {
        parse_state(FIXTURE).unwrap()
    }
    #[test]
    fn fixture_has_both_windows_and_chips() {
        let r = rows();
        assert_eq!(r.len(), 6);
        assert_eq!(r[0].five_hour.percent_text, "45.0%");
        assert_eq!(r[0].seven_day.percent_text, "76.0%");
        assert_eq!(r[1].health_chip, "재로그인 필요");
        assert_eq!(r[4].user_chip, "비활성");
    }
    #[test]
    fn reset_and_date_are_stable() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            format_reset("2027-01-01T00:00:00Z", now),
            "01/01 00:00 초기화"
        );
        assert_eq!(format_reset("2020-01-01T00:00:00Z", now), "리셋됨");
    }
    #[test]
    fn tooltip_and_badge_are_bounded() {
        let r = rows();
        assert!(tooltip(&r, true).chars().count() < 128);
        assert_eq!(badge(&r, true), Badge::Attention);
        assert_eq!(badge(&r, false), Badge::Error);
    }
    #[test]
    fn colors_differ_and_png_is_rgba() {
        assert_ne!(colors(false).background, colors(true).background);
        let png = render_png(&rows(), 1.5, false).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.windows(4).any(|w| w == b"IHDR"));
    }
    #[test]
    fn invalid_scale_is_rejected() {
        assert!(render_png(&rows(), 1.25, false).is_err());
    }

    #[test]
    fn supervisor_backoff_is_bounded_and_resettable() {
        let mut backoff = SupervisorBackoff::default();
        assert_eq!(backoff.delay_seconds(), 5);
        for _ in 0..10 {
            backoff.next();
        }
        assert_eq!(backoff.delay_seconds(), 300);
        backoff.reset();
        assert_eq!(backoff.delay_seconds(), 5);
    }
}
