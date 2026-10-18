use super::*;

fn viewport(content: f32, extent: f32) -> Viewport {
    let mut v = Viewport::default();
    v.set_geometry(content, extent);
    v
}

#[test]
fn dpi_and_fractional_pixels_round_up_after_scaling() {
    for (dpi, expected) in [(96.0, 361), (144.0, 541), (192.0, 721), (137.0, 515)] {
        assert_eq!(pixel_ceil(360.25, dpi), expected);
    }
    for dpi in [96.0, 144.0, 192.0, 137.0] {
        let work_pixels = 901_u32;
        let available = work_pixels as f32 * 96.0 / dpi;
        let v = viewport(1200.0, available);
        let height = pixel_ceil(v.content.min(v.extent), dpi).min(work_pixels);
        assert_eq!(height, work_pixels);
        assert!(v.max_offset() > 0.0);
    }
    assert_eq!(pixel_ceil(360.0, 144.0), 540);
}

#[test]
fn fitting_content_has_identity_coordinates() {
    for extent in [765.0, 900.0] {
        let mut v = viewport(765.0, extent);
        v.wheel_delta(-120.0, 48.0);
        assert_eq!(v.max_offset(), 0.0);
        assert_eq!(v.offset, 0.0);
        assert_eq!(v.to_content_y(52.25), 52.25);
        assert!(v.reveal_popup((100.0, 717.0), 48.0));
        assert_eq!(v.offset, 0.0);
    }
}

#[test]
fn fractional_wheel_and_both_bounds() {
    let mut v = viewport(1000.0, 300.0);
    v.wheel_delta(-15.0, 48.0);
    assert_eq!(v.offset, 6.0);
    assert_eq!(v.to_content_y(12.5), 18.5);
    v.wheel_delta(-f32::MAX, f32::MAX);
    assert_eq!(v.offset, 700.0);
    v.wheel_delta(f32::MAX, f32::MAX);
    assert_eq!(v.offset, 0.0);
}

#[test]
fn popup_reveal_contains_both_ends_and_moves_minimally() {
    let mut v = viewport(1000.0, 300.0);
    assert!(v.reveal_popup((700.0, 900.0), 48.0));
    assert_eq!(v.offset, 648.0);
    assert!(700.0 >= v.offset && 900.0 <= v.offset + v.extent - 48.0);
    assert!(v.reveal_popup((700.0, 900.0), 48.0));
    assert_eq!(v.offset, 648.0);
    assert!(v.reveal_popup((10.0, 110.0), 48.0));
    assert_eq!(v.offset, 10.0);
    assert!(10.0 >= v.offset && 110.0 <= v.offset + v.extent - 48.0);
    assert!(v.reveal_popup((700.0, 1000.0), 0.0));
    assert_eq!(v.offset, 700.0);
    assert!(!v.reveal_popup((700.0, 1000.0), 48.0));
    assert!(!v.reveal_popup((0.0, 301.0), 0.0));
    assert_eq!(v.offset, 700.0);
}

#[test]
fn geometry_changes_clamp_offset() {
    let mut v = viewport(1000.0, 300.0);
    v.offset = v.max_offset();
    v.set_geometry(500.0, 300.0);
    assert_eq!(v.offset, 200.0);
    v.set_geometry(500.0, 450.0);
    assert_eq!(v.offset, 50.0);
    v.set_geometry(100.0, 450.0);
    assert_eq!(v.offset, 0.0);
}

#[test]
fn invalid_values_are_finite_and_do_not_poison_geometry() {
    let mut v = viewport(1000.0, 300.0);
    v.offset = 100.0;
    for delta in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        v.wheel_delta(delta, 48.0);
        assert_eq!(v.offset, 100.0);
    }
    v.wheel_delta(-120.0, -1.0);
    v.wheel_delta(-120.0, f32::INFINITY);
    assert_eq!(v.offset, 100.0);
    for rect in [
        (f32::NAN, 20.0),
        (20.0, f32::INFINITY),
        (30.0, 20.0),
        (-1.0, 20.0),
        (0.0, 1001.0),
    ] {
        assert!(!v.reveal_popup(rect, 0.0));
        assert_eq!(v.offset, 100.0);
    }
    assert!(!v.reveal_popup((0.0, 20.0), f32::NAN));
    assert!(!v.reveal_popup((0.0, 20.0), -1.0));
    assert!(!v.reveal_popup((0.0, 20.0), 300.0));
    v.set_geometry(f32::INFINITY, f32::NAN);
    v.offset = f32::NAN;
    v.clamp();
    assert_eq!(v, Viewport::default());
    assert_eq!(v.to_content_y(f32::NAN), 0.0);
    assert_eq!(pixel_ceil(f32::NAN, 96.0), 0);
    assert_eq!(pixel_ceil(360.0, 0.0), 0);
}
