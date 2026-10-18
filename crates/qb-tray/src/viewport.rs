//! Pure vertical DIP viewport; no window, renderer, or layout dependencies.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub offset: f32,
    pub content: f32,
    pub extent: f32,
}

fn nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

impl Viewport {
    /// Extent is actual client height in DIPs, not full content height.
    pub fn set_geometry(&mut self, content: f32, extent: f32) {
        self.content = nonnegative(content);
        self.extent = nonnegative(extent);
        self.clamp();
    }

    pub fn max_offset(&self) -> f32 {
        (nonnegative(self.content) - nonnegative(self.extent)).max(0.0)
    }

    /// Also repairs public fields after direct assignment (Home/End, etc.).
    pub fn clamp(&mut self) {
        self.content = nonnegative(self.content);
        self.extent = nonnegative(self.extent);
        self.offset = nonnegative(self.offset).min(self.max_offset());
    }

    /// Signed Win32 wheel units: +120 scrolls up by `step` DIPs.
    /// Retains partial detents. Nonfinite delta/step and negative step are ignored.
    pub fn wheel_delta(&mut self, delta: f32, step: f32) {
        self.clamp();
        if delta.is_finite() && step.is_finite() && step >= 0.0 {
            self.offset = (self.offset as f64 - delta as f64 / 120.0 * step as f64)
                .clamp(0.0, self.max_offset() as f64) as f32;
        }
    }

    /// Caller first rejects points outside the actual client rectangle.
    /// Nonfinite pointer input maps to viewport top; finite negatives stay negative.
    pub fn to_content_y(self, y: f32) -> f32 {
        let y = if y.is_finite() { y } else { 0.0 };
        (y as f64 + nonnegative(self.offset).min(self.max_offset()) as f64)
            .clamp(-(f32::MAX as f64), f32::MAX as f64) as f32
    }

    /// Rect is (top, bottom) in content DIPs; adapt shared Rect at the caller.
    /// Reserve is empty space BELOW the popup, not a sticky footer.
    /// Minimal movement to contain both ends; impossible/invalid requests return
    /// false and leave the normalized offset unchanged. Never claim partial fit.
    pub fn reveal_popup(&mut self, rect: (f32, f32), reserve_footer: f32) -> bool {
        self.clamp();
        let (top, bottom) = rect;
        if !top.is_finite()
            || !bottom.is_finite()
            || !reserve_footer.is_finite()
            || top < 0.0
            || bottom < top
            || bottom > self.content
            || reserve_footer < 0.0
        {
            return false;
        }
        let usable = self.extent - reserve_footer;
        if usable <= 0.0 || bottom - top > usable {
            return false;
        }
        let low = (bottom - usable).max(0.0);
        let high = top.min(self.max_offset());
        if low > high {
            return false;
        }
        self.offset = self.offset.clamp(low, high);
        true
    }
}

/// Round UP only after DPI scaling. Invalid input yields zero; caller owns
/// Win32 size limits and caps against integer monitor-work-area pixels.
pub fn pixel_ceil(dips: f32, dpi: f32) -> u32 {
    if !dips.is_finite() || !dpi.is_finite() || dips < 0.0 || dpi <= 0.0 {
        return 0;
    }
    (dips as f64 * dpi as f64 / 96.0).ceil() as u32
}

#[cfg(test)]
#[path = "viewport_tests.rs"]
mod tests;
