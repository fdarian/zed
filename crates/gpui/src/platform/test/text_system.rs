use crate::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId, LineLayout, NoopTextSystem,
    Pixels, PlatformTextSystem, RenderGlyphParams, ShapedGlyph, ShapedRun, Size, px,
};
use std::borrow::Cow;

/// A [`NoopTextSystem`] with glyphs that rasterize to something, so
/// that painting a glyph leaves a sprite in the scene.
pub(crate) struct RasterizingTextSystem(NoopTextSystem, bool);

impl RasterizingTextSystem {
    pub(crate) fn new() -> Self {
        Self(NoopTextSystem::new(), false)
    }

    /// A text system whose glyphs are drawn above the baseline, inside the box that the font says
    /// a glyph is in, as ink is: a painter that skips the glyphs whose box is outside the content
    /// mask then skips only glyphs that are not visible.
    pub(crate) fn with_ink_in_the_box() -> Self {
        Self(NoopTextSystem::new(), true)
    }
}

impl PlatformTextSystem for RasterizingTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> anyhow::Result<()> {
        self.0.add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        self.0.all_font_names()
    }

    fn font_id(&self, descriptor: &Font) -> anyhow::Result<FontId> {
        self.0.font_id(descriptor)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.0.font_metrics(font_id)
    }

    fn typographic_bounds(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
    ) -> anyhow::Result<Bounds<f32>> {
        self.0.typographic_bounds(font_id, glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Size<f32>> {
        self.0.advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> anyhow::Result<Bounds<DevicePixels>> {
        let above = if self.1 {
            (params.font_size.0 * params.scale_factor * 0.5) as i32
        } else {
            0
        };
        Ok(Bounds {
            origin: crate::point(DevicePixels(0), DevicePixels(-above)),
            size: crate::size(DevicePixels(1), DevicePixels(1)),
        })
    }

    fn rasterize_glyph(
        &self,
        _: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> anyhow::Result<(Size<DevicePixels>, Vec<u8>)> {
        Ok((raster_bounds.size, vec![0; 1]))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(
        &self,
        font_id: FontId,
        font_size: Pixels,
    ) -> crate::TextRenderingMode {
        self.0.recommended_rendering_mode(font_id, font_size)
    }
}

/// A text system with the metrics of a fixed-pitch font of nice round numbers, for tests
/// of how text is laid out on a line: every character advances `advance` ems, the font
/// rises `ascent` ems above the baseline and falls `descent` ems below it.
pub(crate) struct FixedMetricsTextSystem {
    noop: NoopTextSystem,
    pub advance: f32,
    pub ascent: f32,
    pub descent: f32,
    pub x_height: f32,
    pub cap_height: f32,
}

impl FixedMetricsTextSystem {
    /// The metrics of Menlo at 20px in Chrome: 12.04px characters, an ascent of 19px and a
    /// descent of 5px, which Chrome rounds to whole pixels, so other sizes differ.
    pub(crate) fn menlo() -> Self {
        Self {
            noop: NoopTextSystem::new(),
            advance: 1233. / 2048.,
            ascent: 0.95,
            descent: 0.25,
            x_height: 1120. / 2048.,
            cap_height: 1493. / 2048.,
        }
    }
}

impl PlatformTextSystem for FixedMetricsTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> anyhow::Result<()> {
        self.noop.add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        self.noop.all_font_names()
    }

    fn font_id(&self, descriptor: &Font) -> anyhow::Result<FontId> {
        self.noop.font_id(descriptor)
    }

    fn font_metrics(&self, _font_id: FontId) -> FontMetrics {
        FontMetrics {
            units_per_em: 1000,
            ascent: self.ascent * 1000.,
            descent: -self.descent * 1000.,
            x_height: self.x_height * 1000.,
            cap_height: self.cap_height * 1000.,
            ..self.noop.font_metrics(FontId(0))
        }
    }

    fn typographic_bounds(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
    ) -> anyhow::Result<Bounds<f32>> {
        self.noop.typographic_bounds(font_id, glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Size<f32>> {
        self.noop.advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.noop.glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, _: &RenderGlyphParams) -> anyhow::Result<Bounds<DevicePixels>> {
        Ok(Bounds {
            origin: Default::default(),
            size: crate::size(DevicePixels(1), DevicePixels(1)),
        })
    }

    fn rasterize_glyph(
        &self,
        _: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> anyhow::Result<(Size<DevicePixels>, Vec<u8>)> {
        Ok((raster_bounds.size, vec![0; 1]))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut x = px(0.);
        let mut shaped: Vec<ShapedRun> = Vec::new();
        let mut run_end = 0;
        let mut font_runs = runs.iter();
        let mut font_run = None;
        for (ix, _) in text.char_indices() {
            while ix >= run_end {
                let next = font_runs.next().expect("the font runs cover the text");
                run_end += next.len;
                font_run = Some(next);
            }
            let font_run = font_run.expect("the loop above sets a font run");
            let run_size = font_run.font_size.unwrap_or(font_size);
            let new_run = shaped.last().is_none_or(|last| {
                last.font_size != run_size || last.line_height != font_run.line_height
            });
            if new_run {
                shaped.push(ShapedRun {
                    font_id: FontId(0),
                    font_size: run_size,
                    ascent: self.font_metrics(FontId(0)).line_box(run_size).ascent,
                    descent: self.font_metrics(FontId(0)).line_box(run_size).descent,
                    line_height: font_run.line_height,
                    glyphs: Vec::new(),
                    inline: Default::default(),
                });
            }
            shaped
                .last_mut()
                .expect("a run was just pushed")
                .glyphs
                .push(ShapedGlyph {
                    id: GlyphId(1),
                    position: crate::point(x, px(0.)),
                    index: ix,
                    is_emoji: false,
                });
            x += run_size * self.advance;
        }
        LineLayout {
            font_size,
            width: x,
            ascent: self.font_metrics(FontId(0)).line_box(font_size).ascent,
            descent: self.font_metrics(FontId(0)).line_box(font_size).descent,
            runs: shaped,
            len: text.len(),
        }
    }

    fn recommended_rendering_mode(
        &self,
        font_id: FontId,
        font_size: Pixels,
    ) -> crate::TextRenderingMode {
        self.noop.recommended_rendering_mode(font_id, font_size)
    }
}
