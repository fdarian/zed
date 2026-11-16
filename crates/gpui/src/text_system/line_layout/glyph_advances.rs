//! What the platform answers about a glyph, kept for the app rather than for one line or one window.
//!
//! Every answer here is a pure function of its key (the scale factor is part of it, so no key
//! belongs to a window), and a new string asks again for the glyphs the strings before it asked for.

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
pub(crate) struct GlyphAdvances {
    font_generation: u64,
    nominal: FxHashMap<NominalKey, Option<f64>>,
    alone: FxHashMap<AloneKey, f64>,
}

impl GlyphAdvances {
    /// The font generation the answers here belong to.
    pub(super) fn font_generation(&self) -> u64 {
        self.font_generation
    }

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

    /// Keeps `advance` unless the fonts changed since `font_generation`, the generation it was
    /// computed under: the answer is for fonts that are gone.
    pub(super) fn insert_alone(
        &mut self,
        font_generation: u64,
        font_id: FontId,
        glyph: GlyphId,
        letter: char,
        font_size: Pixels,
        scale_factor: f32,
        advance: f64,
    ) {
        if self.font_generation != font_generation {
            return;
        }
        if self.alone.len() >= GLYPH_ADVANCES_LIMIT {
            self.alone.clear();
        }
        self.alone.insert(
            (font_id, glyph, letter, font_size.0.to_bits(), scale_factor.to_bits()),
            advance,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bounds, NoopTextSystem, RenderGlyphParams, Size, Font, FontMetrics, FontRun, LineLayout, TextRenderingMode, px};
    use std::{
        borrow::Cow,
        sync::atomic::{AtomicUsize, Ordering},
    };

    /// A platform that counts the advances it was asked for.
    struct CountingPlatform {
        noop: NoopTextSystem,
        advance_calls: AtomicUsize,
    }

    impl PlatformTextSystem for CountingPlatform {
        fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> anyhow::Result<()> {
            self.noop.add_fonts(fonts)
        }
        fn all_font_names(&self) -> Vec<String> {
            self.noop.all_font_names()
        }
        fn font_id(&self, descriptor: &Font) -> anyhow::Result<FontId> {
            self.noop.font_id(descriptor)
        }
        fn font_metrics(&self, font_id: FontId) -> FontMetrics {
            self.noop.font_metrics(font_id)
        }
        fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Bounds<f32>> {
            self.noop.typographic_bounds(font_id, glyph_id)
        }
        fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Size<f32>> {
            self.advance_calls.fetch_add(1, Ordering::Relaxed);
            self.noop.advance(font_id, glyph_id)
        }
        fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
            self.noop.glyph_for_char(font_id, ch)
        }
        fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> anyhow::Result<Bounds<crate::DevicePixels>> {
            self.noop.glyph_raster_bounds(params)
        }
        fn rasterize_glyph(
            &self,
            params: &RenderGlyphParams,
            raster_bounds: Bounds<crate::DevicePixels>,
        ) -> anyhow::Result<(Size<crate::DevicePixels>, Vec<u8>)> {
            self.noop.rasterize_glyph(params, raster_bounds)
        }
        fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun], scale_factor: f32) -> LineLayout {
            self.noop.layout_line(text, font_size, runs, scale_factor)
        }
        fn recommended_rendering_mode(&self, font_id: FontId, font_size: Pixels) -> TextRenderingMode {
            self.noop.recommended_rendering_mode(font_id, font_size)
        }
    }

    #[test]
    fn a_second_lookup_of_a_glyph_does_not_ask_the_platform() {
        let platform = CountingPlatform { noop: NoopTextSystem::new(), advance_calls: AtomicUsize::new(0) };
        let mut advances = GlyphAdvances::default();
        let first = advances.nominal(&platform, FontId(1), GlyphId(7), px(16.), 2.);
        let second = advances.nominal(&platform, FontId(1), GlyphId(7), px(16.), 2.);
        assert_eq!(first, second);
        assert_eq!(platform.advance_calls.load(Ordering::Relaxed), 1);
        // The scale factor is part of the key.
        advances.nominal(&platform, FontId(1), GlyphId(7), px(16.), 1.);
        assert_eq!(platform.advance_calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn an_alone_advance_computed_before_the_fonts_changed_is_dropped() {
        let mut advances = GlyphAdvances::default();
        advances.forget_if_fonts_changed(1);
        let stale = advances.font_generation();
        advances.forget_if_fonts_changed(2);
        advances.insert_alone(stale, FontId(1), GlyphId(7), 'a', px(16.), 2., 9.);
        assert_eq!(advances.alone(FontId(1), GlyphId(7), 'a', px(16.), 2.), None);

        advances.insert_alone(2, FontId(1), GlyphId(7), 'a', px(16.), 2., 9.);
        assert_eq!(advances.alone(FontId(1), GlyphId(7), 'a', px(16.), 2.), Some(9.));
    }
}
