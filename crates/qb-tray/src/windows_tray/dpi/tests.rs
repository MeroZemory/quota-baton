use super::*;

#[test]
fn fixture_dpi_requires_explicit_supported_value_and_every_isolation_control() {
    for (marker, expected) in [("144", 144), ("192", 192)] {
        for base in ["http://127.0.0.1:32123", "http://[::1]:32123"] {
            assert_eq!(
                validate(Some(marker), Some(base), true, true, false),
                Ok(Some(expected))
            );
        }
        for (browser, clipboard) in [(false, true), (true, false), (false, false)] {
            assert_eq!(
                validate(
                    Some(marker),
                    Some("http://127.0.0.1:32123"),
                    browser,
                    clipboard,
                    false
                ),
                Err(INVALID)
            );
        }
        assert_eq!(
            validate(Some(marker), None, true, true, false),
            Err(INVALID)
        );
        assert_eq!(
            validate(
                Some(marker),
                Some("http://127.0.0.1:32123"),
                true,
                true,
                true
            ),
            Err(INVALID)
        );
    }
    for marker in ["", "96", "145", "288", "0", " 144", "144\n"] {
        assert_eq!(
            validate(
                Some(marker),
                Some("http://127.0.0.1:32123"),
                true,
                true,
                false
            ),
            Err(INVALID)
        );
    }
}

#[test]
fn nonisolated_authorities_credentials_paths_and_management_ports_refuse_without_echo() {
    for base in [
        "http://localhost:32123",
        "https://127.0.0.1:32123",
        "http://192.0.2.1:32123",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
        "http://127.0.0.1:10100",
        "http://127.0.0.1:10101",
        "http://user:synthetic-secret@127.0.0.1:32123",
        "http://127.0.0.1:32123/path",
        "http://127.0.0.1:32123?synthetic-secret",
        "http://127.0.0.1:32123#synthetic-secret",
        "http://127.0.0.1:32123\n",
        "http://127.0.0.1:32123\\path",
        "not-a-url",
    ] {
        let refusal = validate(Some("144"), Some(base), true, true, false).unwrap_err();
        assert_eq!(refusal, INVALID);
        assert!(!refusal.contains("synthetic-secret") && !refusal.contains(base));
    }
    // Without an explicit DPI marker, testkit is still the ordinary native path.
    assert_eq!(
        validate(None, Some("not-a-url"), false, false, true),
        Ok(None)
    );
}

#[test]
fn physical_pixels_hit_coordinates_and_uia_share_the_same_fractional_scale() {
    use crate::windows_tray as tray;
    use windows::Win32::Foundation::POINT;
    let bounds = qb_tray::Rect {
        left: 28.,
        top: 64.,
        right: 176.,
        bottom: 96.,
    };
    let origin = POINT { x: -240, y: 120 };
    for dpi in [96_u32, 144, 192] {
        let scale = f64::from(dpi) / 96.;
        let pixels = tray::physical(bounds, origin, scale);
        assert_eq!(
            pixels[0],
            f64::from(origin.x) + f64::from(bounds.left) * scale
        );
        assert_eq!(
            pixels[1],
            f64::from(origin.y) + f64::from(bounds.top) * scale
        );
        assert_eq!(pixels[2], 148. * scale);
        assert_eq!(pixels[3], 32. * scale);
        // Native WM mouse positions are client pixels; UIA positions include screen origin.
        let center_x = pixels[0] + pixels[2] / 2.;
        let center_y = pixels[1] + pixels[3] / 2.;
        let hit_x = ((center_x - f64::from(origin.x)) / scale) as f32;
        let hit_y = ((center_y - f64::from(origin.y)) / scale) as f32;
        assert!(tray::contains(bounds, hit_x, hit_y));
        assert_eq!(
            crate::viewport::pixel_ceil(360., dpi as f32),
            (360. * scale) as u32
        );
        let scene_height = 413.25;
        let pixel_height = crate::viewport::pixel_ceil(scene_height, dpi as f32);
        assert!(pixel_height as f64 >= f64::from(scene_height) * scale);
        assert!((pixel_height as f64) < f64::from(scene_height) * scale + 1.);
    }
}
