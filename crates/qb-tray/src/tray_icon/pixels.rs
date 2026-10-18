//! Runtime baton rasterizer; no font, resource, stock icon, or image dependency.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Normal,
    Attention,
    Error,
}

#[derive(Clone, Debug)]
pub struct Pixels {
    pub size: u32,
    /// Top-down, premultiplied BGRA for a 32-bit Win32 DIB.
    pub bgra: Vec<u8>,
}

/// The only supported physical sizes. Caller chooses again on DPI/theme change.
pub fn render(size: u32, dark: bool, badge: Badge) -> Option<Pixels> {
    if ![16, 20, 24, 32].contains(&size) {
        return None;
    }
    let mut bgra = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0_u32; 4];
            // Four samples per axis preserve the diagonal at 16 physical pixels.
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 4.0) * 32.0 / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / 4.0) * 32.0 / size as f32;
                    if let Some([r, g, b]) = sample(px, py, dark, badge) {
                        for (total, value) in sum.iter_mut().zip([b, g, r, 255]) {
                            *total += u32::from(value);
                        }
                    }
                }
            }
            bgra.extend(sum.map(|value| ((value + 8) / 16) as u8));
        }
    }
    Some(Pixels { size, bgra })
}

fn sample(x: f32, y: f32, dark: bool, badge: Badge) -> Option<[u8; 3]> {
    let ink = if dark { [245, 245, 245] } else { [28, 31, 36] };
    let rim = if dark { [30, 30, 30] } else { [248, 249, 251] };
    // Baton from lower-left to upper-right, with rounded ends and two grips.
    let t = (((x - 8.0) - (y - 24.0)) / 32.0).clamp(0.0, 1.0);
    let dx = x - (8.0 + 16.0 * t);
    let dy = y - (24.0 - 16.0 * t);
    let distance = dx.hypot(dy);
    let mut color = if distance <= 4.4 {
        Some(if distance > 3.4 {
            rim
        } else if !(0.13..=0.87).contains(&t) {
            if dark { [91, 156, 255] } else { [35, 104, 210] }
        } else {
            ink
        })
    } else {
        None
    };
    if badge != Badge::Normal {
        let distance = (x - 24.0).hypot(y - 24.0);
        if distance <= 7.0 {
            color = Some(if distance > 5.5 {
                rim
            } else {
                match (badge, dark) {
                    (Badge::Attention, true) => [247, 194, 92],
                    (Badge::Attention, false) => [174, 108, 0],
                    (Badge::Error, true) => [255, 139, 139],
                    _ => [190, 45, 45],
                }
            });
            // Error has a geometric cross, attention a solid disc; no hand font.
            if badge == Badge::Error
                && distance < 3.8
                && ((x - y).abs() < 1.0 || (x + y - 48.0).abs() < 1.0)
            {
                color = Some(rim);
            }
        }
    }
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sizes_themes_and_states_have_premultiplied_pixels_and_clear_corners() {
        for size in [16, 20, 24, 32] {
            for dark in [false, true] {
                let states = [Badge::Normal, Badge::Attention, Badge::Error]
                    .map(|badge| render(size, dark, badge).unwrap());
                for image in &states {
                    assert_eq!(image.bgra.len(), (size * size * 4) as usize);
                    assert_eq!(&image.bgra[..4], &[0; 4]);
                    assert!(image.bgra.chunks_exact(4).any(|p| p[3] == 255));
                    assert!(image.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255));
                    assert!(
                        image
                            .bgra
                            .chunks_exact(4)
                            .all(|p| p[..3].iter().all(|v| *v <= p[3]))
                    );
                }
                assert_ne!(states[0].bgra, states[1].bgra);
                assert_ne!(states[1].bgra, states[2].bgra);
            }
        }
        assert!(render(0, false, Badge::Normal).is_none());
        assert!(render(u32::MAX, false, Badge::Normal).is_none());
    }

    #[test]
    fn baton_body_contrasts_with_both_system_surfaces() {
        // Main monochrome body relative luminance: WCAG contrast > 7:1.
        fn luminance(rgb: [u8; 3]) -> f64 {
            rgb.into_iter()
                .zip([0.2126, 0.7152, 0.0722])
                .map(|(v, w)| {
                    let c = f64::from(v) / 255.0;
                    w * if c <= 0.04045 {
                        c / 12.92
                    } else {
                        ((c + 0.055) / 1.055).powf(2.4)
                    }
                })
                .sum()
        }
        for (dark, background) in [(false, [248, 249, 251]), (true, [30, 30, 30])] {
            let body = sample(16.0, 16.0, dark, Badge::Normal).unwrap();
            let a = luminance(body);
            let b = luminance(background);
            assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) > 7.0);
        }
    }
}
