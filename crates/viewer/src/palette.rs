// The colours the viewer draws with. The panel has the same values under the
// same names in crates/app/src/theme.rs.

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colour(pub u32);

// The window tone: the bars around the picture, the strip's band and the
// title bar.
pub(crate) const WINDOW: Colour = Colour(0x141412);
// The title bar's text while the window is active; ash while it is not.
pub(crate) const CHALK: Colour = Colour(0xE4E2DD);
pub(crate) const ASH: Colour = Colour(0x9A978E);
pub(crate) const AMBER: Colour = Colour(0xE8912F);
pub(crate) const SAGE: Colour = Colour(0x6FA86F);
pub(crate) const WARN: Colour = Colour(0xCBB04B);
pub(crate) const BAD: Colour = Colour(0xDE7461);

impl Colour {
    pub(crate) fn rgb(self) -> [u8; 3] {
        [(self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8]
    }

    // As GDI and DWM take it, 0x00BBGGRR.
    pub(crate) const fn colorref(self) -> COLORREF {
        COLORREF(((self.0 & 0xFF) << 16) | (self.0 & 0xFF00) | ((self.0 >> 16) & 0xFF))
    }

    // As the shaders and Direct2D take it. The back buffer is UNORM, not
    // sRGB, so the code values go straight through.
    pub(crate) fn floats(self) -> [f32; 4] {
        let [r, g, b] = self.rgb();
        [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
    }

    pub(crate) fn d2d(self) -> D2D1_COLOR_F {
        let [r, g, b, a] = self.floats();
        D2D1_COLOR_F { r, g, b, a }
    }
}

pub(crate) fn level(level: stats::Level) -> Colour {
    match level {
        stats::Level::Good => SAGE,
        stats::Level::Warn => WARN,
        stats::Level::Bad => BAD,
    }
}
