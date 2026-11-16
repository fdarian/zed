//! What the platform answers about a glyph, kept for the window rather than for one line.
//!
//! Every answer here is a pure function of its key, and a new string asks again for the glyphs
//! the strings before it asked for.

use crate::{FontId, GlyphId, PlatformTextSystem, Pixels};
use collections::FxHashMap;
use gpui_util::ResultExt as _;

/// What either map may hold before it is emptied.
const GLYPH_ADVANCES_LIMIT: usize = 16384;

/// `(font, glyph, font size bits, scale factor bits)`.
type NominalKey = (FontId, GlyphId, u32, u32);
/// `(font, glyph, letter, font size bits, scale factor bits)`.
type AloneKey = (FontId, GlyphId, char, u32, u32);

#[derive(Default)]
pub(super) struct GlyphAdvances {
    font_generation: u64,
    nominal: FxHashMap<NominalKey, Option<f64>>,
    alone: FxHashMap<AloneKey, f64>,
}

impl GlyphAdvances {
    pub(super) fn forget_if_fonts_changed(&mut self, font_generation: u64) {
        if self.font_generation != font_generation {
            self.font_generation = font_generation;
            self.nominal.clear();
            self.alone.clear();
        }
    }

    /// The advance of `glyph` in logical pixels, which is `None` if the platform has none.
    pub(super) fn nominal(
        &mut self,
        platform_text_system: &dyn PlatformTextSystem,
        font_id: FontId,
        glyph: GlyphId,
        font_size: Pixels,
        scale_factor: f32,
    ) -> Option<f64> {
        let key = (font_id, glyph, font_size.0.to_bits(), scale_factor.to_bits());
        if let Some(advance) = self.nominal.get(&key) {
            return *advance;
        }
        let advance = platform_text_system
            .glyph_advance(font_id, glyph, font_size, scale_factor)
            .log_err();
        if self.nominal.len() >= GLYPH_ADVANCES_LIMIT {
            self.nominal.clear();
        }
        self.nominal.insert(key, advance);
        advance
    }

    pub(super) fn alone(
        &self,
        font_id: FontId,
        glyph: GlyphId,
        letter: char,
        font_size: Pixels,
        scale_factor: f32,
    ) -> Option<f64> {
        self.alone
            .get(&(font_id, glyph, letter, font_size.0.to_bits(), scale_factor.to_bits()))
            .copied()
    }

    pub(super) fn insert_alone(
        &mut self,
        font_id: FontId,
        glyph: GlyphId,
        letter: char,
        font_size: Pixels,
        scale_factor: f32,
        advance: f64,
    ) {
        if self.alone.len() >= GLYPH_ADVANCES_LIMIT {
            self.alone.clear();
        }
        self.alone.insert(
            (font_id, glyph, letter, font_size.0.to_bits(), scale_factor.to_bits()),
            advance,
        );
    }
}
