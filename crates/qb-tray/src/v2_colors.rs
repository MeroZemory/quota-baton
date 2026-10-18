//! SPEC-v2 section 3. One palette for HWND, WIC, menus and shell semantics.
use super::v2::Tone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);
impl Rgb {
    pub const fn hex(rgb: u32) -> Self {
        Self((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub surface: Rgb,
    pub footer: Rgb,
    pub popup: Rgb,
    pub border: Rgb,
    pub popup_border: Rgb,
    pub text: Rgb,
    pub secondary: Rgb,
    pub disabled: Rgb,
    pub divider: Rgb,
    pub track: Rgb,
    pub bars: [Rgb; 4],
    pub dots: [Rgb; 4],
    pub chips: [(Rgb, Rgb); 4],
    pub warning: Rgb,
    pub bad: Rgb,
    pub accent: Rgb,
    pub on_accent: Rgb,
    pub button: Rgb,
    pub button_border: Rgb,
    pub check_border: Rgb,
    pub focus_outer: Rgb,
    pub focus_inner: Rgb,
    pub dark: bool,
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        let c = Rgb::hex;
        if dark {
            Self {
                surface: c(0x202020),
                footer: c(0x1b1b1b),
                popup: c(0x2c2c2c),
                border: c(0x3a3a3a),
                popup_border: c(0x474747),
                text: c(0xf3f3f3),
                secondary: c(0xa8a8a8),
                disabled: c(0x727272),
                divider: c(0x313131),
                track: c(0x3a3a3a),
                bars: [c(0x5fa4e6), c(0xe0a84a), c(0xef6f61), c(0x727272)],
                dots: [c(0x6ccb8f), c(0xe0a84a), c(0xef6f61), c(0x8a8a8a)],
                chips: [
                    (c(0x1d3346), c(0x60cdff)),
                    (c(0x3d2f14), c(0xe8b864)),
                    (c(0x432020), c(0xf08a80)),
                    (c(0x2e2e2e), c(0xb5b5b5)),
                ],
                warning: c(0xe8b864),
                bad: c(0xf08a80),
                accent: c(0x60cdff),
                on_accent: c(0),
                button: c(0x2d2d2d),
                button_border: c(0x3d3d3d),
                check_border: c(0x9a9a9a),
                focus_outer: c(0xffffff),
                focus_inner: c(0),
                dark,
            }
        } else {
            Self {
                surface: c(0xffffff),
                footer: c(0xf7f7f7),
                popup: c(0xffffff),
                border: c(0xd9d9d9),
                popup_border: c(0xd4d4d4),
                text: c(0x1a1a1a),
                secondary: c(0x5f6368),
                disabled: c(0xa3a3a3),
                divider: c(0xebebeb),
                track: c(0xe3e6ea),
                bars: [c(0x2f6fb0), c(0xb7791f), c(0xc42b1c), c(0xa3a3a3)],
                dots: [c(0x2c7a4b), c(0xb7791f), c(0xc42b1c), c(0x8a8f95)],
                chips: [
                    (c(0xe8f1fb), c(0x005fb8)),
                    (c(0xfbf0dc), c(0x8a560a)),
                    (c(0xfcebe9), c(0xb42318)),
                    (c(0xeef0f2), c(0x50555b)),
                ],
                warning: c(0x8a560a),
                bad: c(0xb42318),
                accent: c(0x005fb8),
                on_accent: c(0xffffff),
                button: c(0xfbfbfb),
                button_border: c(0xd1d1d1),
                check_border: c(0x8a8a8a),
                focus_outer: c(0),
                focus_inner: c(0xffffff),
                dark,
            }
        }
    }
    pub fn chip(self, tone: Tone) -> (Rgb, Rgb) {
        self.chips[match tone {
            Tone::Use => 0,
            Tone::Warn => 1,
            Tone::Bad => 2,
            _ => 3,
        }]
    }
    pub fn tone(self, tone: Tone) -> Rgb {
        match tone {
            Tone::Use => self.accent,
            Tone::Warn => self.warning,
            Tone::Bad => self.bad,
            Tone::Off | Tone::Secondary => self.secondary,
            Tone::Ok => self.dots[0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_opt_in_dark_and_single_meaning_chip_tokens() {
        let light = Palette::new(false);
        let dark = Palette::new(true);
        assert_eq!(light.surface, Rgb::hex(0xffffff));
        assert_eq!(dark.surface, Rgb::hex(0x202020));
        for p in [light, dark] {
            assert_eq!(p.chip(Tone::Use).1, p.accent);
            assert_eq!(p.chip(Tone::Warn).1, p.warning);
            assert_eq!(p.chip(Tone::Bad).1, p.bad);
            assert_ne!(p.chip(Tone::Off), p.chip(Tone::Bad));
        }
    }
}
