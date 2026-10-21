mod font_fallbacks;
mod font_features;
mod inline;
mod font_variations;
mod line;
mod line_break;
mod line_layout;
mod line_truncation;
mod line_wrapper;
mod text_wrap;
mod text_box;

pub use font_fallbacks::*;
pub use font_features::*;
pub use inline::*;
pub use font_variations::*;
pub use line::*;
pub use line_layout::*;
pub(crate) use line_truncation::EndTruncation;
pub use line_wrapper::*;
pub use text_wrap::*;
pub use text_box::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Bounds, DevicePixels, ExtraDecoration, Hsla, OverlineStyle, Pixels, PlatformTextSystem, Point, Result,
    SharedString, Size, StrikethroughStyle, TextRenderingMode, TextStyle, UnderlineStyle, px,
};
use anyhow::{Context as _, anyhow};
use collections::FxHashMap;
use core::fmt;
use derive_more::{Add, Deref, FromStr, Sub};
use itertools::Itertools;
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use smallvec::{SmallVec, smallvec};
use std::{
    borrow::Cow,
    cmp,
    fmt::{Debug, Display, Formatter},
    hash::{Hash, Hasher},
    ops::{Deref, DerefMut, Range},
    sync::Arc,
};

/// An opaque identifier for a specific font.
#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
#[repr(C)]
pub struct FontId(pub usize);

/// An opaque identifier for a specific font family.
#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub struct FontFamilyId(pub usize);

/// Number of subpixel glyph variants along the X axis.
pub const SUBPIXEL_VARIANTS_X: u8 = 4;

/// Number of subpixel glyph variants along the Y axis.
pub const SUBPIXEL_VARIANTS_Y: u8 = 1;

/// The GPUI text rendering sub system.
pub struct TextSystem {
    platform_text_system: Arc<dyn PlatformTextSystem>,
    font_ids_by_font: RwLock<FxHashMap<Font, Result<FontId>>>,
    font_metrics: RwLock<FxHashMap<FontId, FontMetrics>>,
    raster_bounds: RwLock<FxHashMap<RenderGlyphParams, Bounds<DevicePixels>>>,
    wrapper_pool: Mutex<FxHashMap<FontIdWithSize, Vec<LineWrapper>>>,
    font_runs_pool: Mutex<Vec<Vec<FontRun>>>,
    fallback_font_stack: SmallVec<[Font; 2]>,
    /// Counts the times fonts were added, which can change what text in a family that did not
    /// resolve before looks like.
    font_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl TextSystem {
    /// Create a new TextSystem with the given platform text system.
    pub fn new(platform_text_system: Arc<dyn PlatformTextSystem>) -> Self {
        TextSystem {
            platform_text_system,
            font_metrics: RwLock::default(),
            raster_bounds: RwLock::default(),
            font_ids_by_font: RwLock::default(),
            wrapper_pool: Mutex::default(),
            font_runs_pool: Mutex::default(),
            font_generation: Arc::default(),
            fallback_font_stack: smallvec![
                // TODO: Remove this when Linux have implemented setting fallbacks.
                font(".ZedMono"),
                font(".ZedSans"),
                font("Helvetica"),
                font("Segoe UI"),     // Windows
                font("Ubuntu"),       // Gnome (Ubuntu)
                font("Adwaita Sans"), // Gnome 47
                font("Cantarell"),    // Gnome
                font("Noto Sans"),    // KDE
                font("DejaVu Sans"),
                font("Arial"), // macOS, Windows
            ],
        }
    }

    /// Get a list of all available font names from the operating system.
    pub fn all_font_names(&self) -> Vec<String> {
        let mut names = self.platform_text_system.all_font_names();
        names.extend(
            self.fallback_font_stack
                .iter()
                .map(|font| font.family.to_string()),
        );
        names.push(".SystemUIFont".to_string());
        names.sort_unstable();
        names.dedup();
        names
    }

    /// How many times fonts were added to the text system.
    pub(crate) fn font_generation(&self) -> u64 {
        self.font_generation
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Add a font's data to the text system.
    pub fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.platform_text_system.add_fonts(fonts)?;
        self.font_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A family that was requested before its font was added is cached as an
        // error, and would never resolve. Resolved families stay valid.
        self.font_ids_by_font
            .write()
            .retain(|_, font_id| font_id.is_ok());
        Ok(())
    }

    /// Get the FontId for the configure font family and style.
    fn font_id(&self, font: &Font) -> Result<FontId> {
        fn clone_font_id_result(font_id: &Result<FontId>) -> Result<FontId> {
            match font_id {
                Ok(font_id) => Ok(*font_id),
                Err(err) => Err(anyhow!("{err}")),
            }
        }

        let font_id = self
            .font_ids_by_font
            .read()
            .get(font)
            .map(clone_font_id_result);
        if let Some(font_id) = font_id {
            font_id
        } else {
            let font_id = self.platform_text_system.font_id(font);
            self.font_ids_by_font
                .write()
                .insert(font.clone(), clone_font_id_result(&font_id));
            font_id
        }
    }

    /// Get the Font for the Font Id.
    pub fn get_font_for_id(&self, id: FontId) -> Option<Font> {
        let lock = self.font_ids_by_font.read();
        lock.iter()
            .filter_map(|(font, result)| match result {
                Ok(font_id) if *font_id == id => Some(font.clone()),
                _ => None,
            })
            .next()
    }

    /// Resolves the specified font, falling back to the default font stack if
    /// the font fails to load.
    ///
    /// # Panics
    ///
    /// Panics if the font and none of the fallbacks can be resolved.
    pub fn resolve_font(&self, font: &Font) -> FontId {
        if let Ok(font_id) = self.font_id(font) {
            return font_id;
        }
        for fallback in &self.fallback_font_stack {
            if let Ok(font_id) = self.font_id(fallback) {
                return font_id;
            }
        }

        panic!(
            "failed to resolve font '{}' or any of the fallbacks: {}",
            font.family,
            self.fallback_font_stack
                .iter()
                .map(|fallback| &fallback.family)
                .join(", ")
        );
    }

    /// Prewarm any system font caches needed to shape text.
    ///
    /// This may be expensive, so callers should generally invoke it on a
    /// background executor. Missing entries are still populated on demand by
    /// the normal shaping path.
    pub fn prewarm_fonts(&self, fonts: &[Font]) {
        let mut font_ids = SmallVec::<[FontId; 8]>::new();
        for font in fonts {
            let font_id = self.resolve_font(font);
            if !font_ids.contains(&font_id) {
                font_ids.push(font_id);
            }
        }
        self.platform_text_system.prewarm_fonts(&font_ids);
    }

    /// Get the bounding box for the given font and font size.
    /// A font's bounding box is the smallest rectangle that could enclose all glyphs
    /// in the font. superimposed over one another.
    pub fn bounding_box(&self, font_id: FontId, font_size: Pixels) -> Bounds<Pixels> {
        self.read_metrics(font_id, |metrics| metrics.bounding_box(font_size))
    }

    /// Get the typographic bounds for the given character, in the given font and size.
    pub fn typographic_bounds(
        &self,
        font_id: FontId,
        font_size: Pixels,
        character: char,
    ) -> Result<Bounds<Pixels>> {
        let glyph_id = self
            .platform_text_system
            .glyph_for_char(font_id, character)
            .with_context(|| format!("glyph not found for character '{character}'"))?;
        let bounds = self
            .platform_text_system
            .typographic_bounds(font_id, glyph_id)?;
        Ok(self.read_metrics(font_id, |metrics| {
            (bounds / metrics.units_per_em as f32 * font_size.0).map(px)
        }))
    }

    /// Get the advance width for the given character, in the given font and size.
    pub fn advance(&self, font_id: FontId, font_size: Pixels, ch: char) -> Result<Size<Pixels>> {
        let glyph_id = self
            .platform_text_system
            .glyph_for_char(font_id, ch)
            .with_context(|| format!("glyph not found for character '{ch}'"))?;
        let result = self.platform_text_system.advance(font_id, glyph_id)?
            / self.units_per_em(font_id) as f32;

        Ok(result * font_size)
    }

    // Consider removing this?
    /// Returns the shaped layout width of for the given character, in the given font and size.
    pub fn layout_width(&self, font_id: FontId, font_size: Pixels, ch: char) -> Pixels {
        let mut buffer = [0; 4];
        let buffer = ch.encode_utf8(&mut buffer);
        // Only the width is read, which the scale does not change.
        self.platform_text_system
            .layout_line(
                buffer,
                font_size,
                &[FontRun {
                    len: buffer.len(),
                    font_id,
                    font_size: None,
                    line_height: None,
                }],
                1.,
            )
            .width
    }

    /// Returns the width of an `em`.
    ///
    /// Uses the width of the `m` character in the given font and size.
    pub fn em_width(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        Ok(self.typographic_bounds(font_id, font_size, 'm')?.size.width)
    }

    /// Returns the advance width of an `em`.
    ///
    /// Uses the advance width of the `m` character in the given font and size.
    pub fn em_advance(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        Ok(self.advance(font_id, font_size, 'm')?.width)
    }

    /// Returns the width of an `ch`.
    ///
    /// Uses the width of the `0` character in the given font and size.
    pub fn ch_width(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        Ok(self.typographic_bounds(font_id, font_size, '0')?.size.width)
    }

    /// Returns the advance width of an `ch`.
    ///
    /// Uses the advance width of the `0` character in the given font and size.
    pub fn ch_advance(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        Ok(self.advance(font_id, font_size, '0')?.width)
    }

    /// Get the number of font size units per 'em square',
    /// Per MDN: "an abstract square whose height is the intended distance between
    /// lines of type in the same type size"
    pub fn units_per_em(&self, font_id: FontId) -> u32 {
        self.read_metrics(font_id, |metrics| metrics.units_per_em)
    }

    /// Get the height of a capital letter in the given font and size.
    pub fn cap_height(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.read_metrics(font_id, |metrics| metrics.cap_height(font_size))
    }

    /// Get the height of the x character in the given font and size.
    pub fn x_height(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.read_metrics(font_id, |metrics| metrics.x_height(font_size))
    }

    /// Get the recommended distance from the baseline for the given font
    pub fn ascent(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.read_metrics(font_id, |metrics| metrics.ascent(font_size))
    }

    /// Get the recommended distance below the baseline for the given font,
    /// in single spaced text.
    pub fn descent(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.read_metrics(font_id, |metrics| metrics.descent(font_size))
    }

    /// The content area of an inline box in the given font and size, as Chrome measures it
    /// on a display of `scale_factor` device pixels to a pixel.
    pub fn line_box(&self, font_id: FontId, font_size: Pixels, scale_factor: f32) -> LineBox {
        self.read_metrics(font_id, |metrics| metrics.line_box(font_size, scale_factor))
    }

    /// The line height `style` asks for, in pixels, laid out in 1/64 of a device pixel as
    /// [`LineHeight::to_laid_out_pixels`] does: `normal` is the height of its font on a display
    /// of `scale_factor` device pixels to a pixel.
    pub fn line_height(&self, style: &TextStyle, rem_size: Pixels, scale_factor: f32) -> Pixels {
        let font_size = style.font_size.to_pixels(rem_size);
        style
            .line_height
            .to_laid_out_pixels(style.font_size, rem_size, scale_factor, || {
                self.line_box(self.resolve_font(&style.font()), font_size, scale_factor)
                    .normal_line_height()
            })
    }

    /// Get the recommended baseline offset for the given font and line height.
    pub fn baseline_offset(
        &self,
        font_id: FontId,
        font_size: Pixels,
        line_height: Pixels,
        scale_factor: f32,
    ) -> Pixels {
        self.line_box(font_id, font_size, scale_factor)
            .extent(line_height, scale_factor)
            .0
    }

    /// The underline thickness the font reports for `font_size`, zero when it has none.
    pub fn underline_thickness(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.read_metrics(font_id, |metrics| metrics.underline_thickness(font_size))
    }

    fn read_metrics<T>(&self, font_id: FontId, read: impl FnOnce(&FontMetrics) -> T) -> T {
        let lock = self.font_metrics.upgradable_read();

        if let Some(metrics) = lock.get(&font_id) {
            read(metrics)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let metrics = lock
                .entry(font_id)
                .or_insert_with(|| self.platform_text_system.font_metrics(font_id));
            read(metrics)
        }
    }

    /// Returns a handle to a line wrapper, for the given font and font size.
    pub fn line_wrapper(self: &Arc<Self>, font: Font, font_size: Pixels) -> LineWrapperHandle {
        let lock = &mut self.wrapper_pool.lock();
        let font_id = self.resolve_font(&font);
        let wrappers = lock
            .entry(FontIdWithSize { font_id, font_size })
            .or_default();
        let wrapper = wrappers
            .pop()
            .unwrap_or_else(|| LineWrapper::new(font_id, font_size, self.clone()));

        LineWrapperHandle {
            wrapper: Some(wrapper),
            text_system: self.clone(),
        }
    }

    /// Get the rasterized size and location of a specific, rendered glyph.
    pub(crate) fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let raster_bounds = self.raster_bounds.upgradable_read();
        if let Some(bounds) = raster_bounds.get(params) {
            Ok(*bounds)
        } else {
            let mut raster_bounds = RwLockUpgradableReadGuard::upgrade(raster_bounds);
            let bounds = self.platform_text_system.glyph_raster_bounds(params)?;
            raster_bounds.insert(params.clone(), bounds);
            Ok(bounds)
        }
    }

    pub(crate) fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let raster_bounds = self.raster_bounds(params)?;
        self.platform_text_system
            .rasterize_glyph(params, raster_bounds)
    }

    /// Returns the bucket (0 to 7) of the text luminance a glyph painted in the given color is
    /// rasterised for, the table that turns coverage into what the blend needs.
    pub(crate) fn glyph_luminance_bucket_for_color(&self, color: Hsla) -> u8 {
        self.platform_text_system.glyph_luminance_bucket_for_color(color)
    }

    /// Returns the text rendering mode recommended by the platform for the given font and size.
    /// The return value will never be [`TextRenderingMode::PlatformDefault`].
    pub(crate) fn recommended_rendering_mode(
        &self,
        font_id: FontId,
        font_size: Pixels,
    ) -> TextRenderingMode {
        self.platform_text_system
            .recommended_rendering_mode(font_id, font_size)
    }
}

/// The GPUI text layout subsystem.
#[derive(Deref)]
pub struct WindowTextSystem {
    line_layout_cache: LineLayoutCache,
    #[deref]
    text_system: Arc<TextSystem>,
}

impl WindowTextSystem {
    /// Create a new WindowTextSystem with the given TextSystem.
    pub fn new(text_system: Arc<TextSystem>) -> Self {
        Self {
            line_layout_cache: LineLayoutCache::new(
                text_system.platform_text_system.clone(),
                text_system.font_generation.clone(),
            ),
            text_system,
        }
    }

    /// Sets the display scale lines are shaped for: the window's, which Chrome zooms its fonts
    /// to before it rounds their metrics. Layouts shaped for another scale stay where they are,
    /// under their own key.
    pub(crate) fn set_scale_factor(&self, scale_factor: f32) {
        self.line_layout_cache.set_scale_factor(scale_factor);
    }

    /// What the line layout cache has shaped and found since the window was made.
    pub fn text_counters(&self) -> TextCounters {
        self.line_layout_cache.counters()
    }

    pub(crate) fn layout_index(&self) -> LineLayoutIndex {
        self.line_layout_cache.layout_index()
    }

    pub(crate) fn reuse_layouts(&self, index: Range<LineLayoutIndex>) {
        self.line_layout_cache.reuse_layouts(index)
    }

    pub(crate) fn truncate_layouts(&self, index: LineLayoutIndex) {
        self.line_layout_cache.truncate_layouts(index)
    }

    /// Shape the given line, at the given font_size, for painting to the screen.
    /// Subsets of the line can be styled independently with the `runs` parameter.
    ///
    /// Note that this method can only shape a single line of text. It will panic
    /// if the text contains newlines. If you need to shape multiple lines of text,
    /// use [`Self::shape_text`] instead.
    pub fn shape_line(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        force_width: Option<Pixels>,
    ) -> ShapedLine {
        debug_assert!(
            text.find('\n').is_none(),
            "text argument should not contain newlines"
        );

        let mut decoration_runs = SmallVec::<[DecorationRun; 32]>::new();
        for run in runs {
            if let Some(last_run) = decoration_runs.last_mut()
                && last_run.color == run.color
                && last_run.font_smoothing == run.font_smoothing
                && last_run.underline == run.underline
                && last_run.strikethrough == run.strikethrough
                && last_run.overline == run.overline
                && last_run.extra_decorations == run.extra_decorations
                && last_run.background_color == run.background_color
                && last_run.group == run.group
            {
                last_run.len += run.len as u32;
                continue;
            }
            decoration_runs.push(DecorationRun {
                len: run.len as u32,
                color: run.color,
                font_smoothing: run.font_smoothing,
                background_color: run.background_color,
                underline: run.underline,
                strikethrough: run.strikethrough,
                overline: run.overline,
                extra_decorations: run.extra_decorations.clone(),
                group: run.group,
            });
        }

        let layout = self.layout_line(&text, font_size, runs, force_width);

        ShapedLine {
            layout,
            text,
            decoration_runs,
        }
    }

    /// Shape the given line using a caller-provided content hash as the cache key.
    ///
    /// This enables cache hits without materializing a contiguous `SharedString` for the text.
    /// If the cache misses, `materialize_text` is invoked to produce the `SharedString` for shaping.
    ///
    /// Contract (caller enforced):
    /// - Same `text_hash` implies identical text content (collision risk accepted by caller).
    /// - `text_len` should be the UTF-8 byte length of the text (helps reduce accidental collisions).
    ///
    /// Like [`Self::shape_line`], this must be used only for single-line text (no `\n`).
    pub fn shape_line_by_hash(
        &self,
        text_hash: u64,
        text_len: usize,
        font_size: Pixels,
        runs: &[TextRun],
        force_width: Option<Pixels>,
        materialize_text: impl FnOnce() -> SharedString,
    ) -> ShapedLine {
        let mut decoration_runs = SmallVec::<[DecorationRun; 32]>::new();
        for run in runs {
            if let Some(last_run) = decoration_runs.last_mut()
                && last_run.color == run.color
                && last_run.font_smoothing == run.font_smoothing
                && last_run.underline == run.underline
                && last_run.strikethrough == run.strikethrough
                && last_run.overline == run.overline
                && last_run.extra_decorations == run.extra_decorations
                && last_run.background_color == run.background_color
                && last_run.group == run.group
            {
                last_run.len += run.len as u32;
                continue;
            }
            decoration_runs.push(DecorationRun {
                len: run.len as u32,
                color: run.color,
                font_smoothing: run.font_smoothing,
                background_color: run.background_color,
                underline: run.underline,
                strikethrough: run.strikethrough,
                overline: run.overline,
                extra_decorations: run.extra_decorations.clone(),
                group: run.group,
            });
        }

        let mut used_force_width = force_width;
        let layout = self.layout_line_by_hash(
            text_hash,
            text_len,
            font_size,
            runs,
            used_force_width,
            || {
                let text = materialize_text();
                debug_assert!(
                    text.find('\n').is_none(),
                    "text argument should not contain newlines"
                );
                text
            },
        );

        // We only materialize actual text on cache miss; on hit we avoid allocations.
        // Since `ShapedLine` carries a `SharedString`, use an empty placeholder for hits.
        // NOTE: Callers must not rely on `ShapedLine.text` for content when using this API.
        let text: SharedString = SharedString::new_static("");

        ShapedLine {
            layout,
            text,
            decoration_runs,
        }
    }

    /// Shape a multi line string of text, at the given font_size, for painting to the screen.
    /// Subsets of the text can be styled independently with the `runs` parameter.
    /// If `wrap_width` is provided, the line breaks will be adjusted to fit within the given width.
    pub fn shape_text(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        wrap_width: Option<Pixels>,
        line_clamp: Option<usize>,
    ) -> Result<SmallVec<[WrappedLine; 1]>> {
        self.shape_text_with_options(
            text,
            font_size,
            runs,
            wrap_width,
            line_clamp,
            TextLayoutOptions::default(),
        )
    }

    /// Like [`Self::shape_text`], laying the text out as `options` say.
    pub fn shape_text_with_options(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        wrap_width: Option<Pixels>,
        line_clamp: Option<usize>,
        options: TextLayoutOptions,
    ) -> Result<SmallVec<[WrappedLine; 1]>> {
        self.shape_text_with_insets(text, font_size, runs, wrap_width, line_clamp, options, &[])
    }

    /// Like [`Self::shape_text_with_options`], with `insets` around ranges of the
    /// text. Their ranges are byte ranges of `text`, newlines included.
    pub fn shape_text_with_insets(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        wrap_width: Option<Pixels>,
        line_clamp: Option<usize>,
        options: TextLayoutOptions,
        insets: &[TextInset],
    ) -> Result<SmallVec<[WrappedLine; 1]>> {
        self.shape_text_inline(
            text,
            font_size,
            runs,
            wrap_width,
            line_clamp,
            options,
            insets,
            &[],
            Pixels::ZERO,
        )
    }

    /// Like [`Self::shape_text_with_insets`], with the `inline` boxes of the text: atomic
    /// inlines such as an `inline-block`, which take the place of a U+FFFC character each,
    /// and the vertical alignment of boxes on their line. The ranges are byte ranges of
    /// `text`, newlines included. `line_height` is the one the lines are measured and
    /// painted with, which decides where `middle` and the like put a box.
    pub fn shape_text_inline(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        wrap_width: Option<Pixels>,
        line_clamp: Option<usize>,
        options: TextLayoutOptions,
        insets: &[TextInset],
        inline: &[InlineBox],
        line_height: Pixels,
    ) -> Result<SmallVec<[WrappedLine; 1]>> {
        let mut runs = runs.iter().filter(|run| run.len > 0).cloned().peekable();
        let mut font_runs = self.font_runs_pool.lock().pop().unwrap_or_default();

        let mut lines = SmallVec::new();
        let mut max_wrap_lines = line_clamp;
        let mut wrapped_lines = 0;

        let mut process_line = |line_text: SharedString, line_start, line_end| {
            font_runs.clear();

            let mut decoration_runs = <Vec<DecorationRun>>::with_capacity(32);
            let mut run_start = line_start;
            while run_start < line_end {
                let Some(run) = runs.peek_mut() else {
                    log::warn!("`TextRun`s do not cover the entire to be shaped text");
                    break;
                };

                let run_len_within_line = cmp::min(line_end - run_start, run.len);

                let decoration_changed =
                    push_decoration(&mut decoration_runs, run, run_len_within_line);

                let font_id = self.resolve_font(&run.font);
                if let Some(font_run) = font_runs.last_mut()
                    && font_id == font_run.font_id
                    && run.font_size == font_run.font_size
                    && run.line_height == font_run.line_height
                    && !decoration_changed
                {
                    font_run.len += run_len_within_line;
                } else {
                    font_runs.push(FontRun {
                        len: run_len_within_line,
                        font_id,
                        font_size: run.font_size,
                        line_height: run.line_height,
                    });
                }

                // Preserve the remainder of the run for the next line
                run.len -= run_len_within_line;
                if run.len == 0 {
                    runs.next();
                }
                run_start += run_len_within_line;
            }

            let line_insets = insets_on_line(insets, line_start..line_end);
            for inset in &line_insets {
                split_font_run_at(&mut font_runs, inset.range.start);
                split_font_run_at(&mut font_runs, inset.range.end);
            }
            let line_inline = self.inline_on_line(inline, line_start..line_end);
            for spec in line_inline.iter().filter(|spec| spec.atomic().is_some()) {
                split_font_run_at(&mut font_runs, spec.range.start);
                split_font_run_at(&mut font_runs, spec.range.end);
            }

            let layout = self.line_layout_cache.layout_wrapped_line(
                &line_text,
                font_size,
                &font_runs,
                wrap_width,
                max_wrap_lines.map(|max| max.saturating_sub(wrapped_lines)),
                options,
                &line_insets,
                &line_inline,
                line_height,
            );
            wrapped_lines += layout.wrap_boundaries.len();

            lines.push(WrappedLine {
                layout,
                decoration_runs,
                text: line_text,
            });

            // Skip `\n` character.
            if let Some(run) = runs.peek_mut() {
                run.len -= 1;
                if run.len == 0 {
                    runs.next();
                }
            }
        };

        let mut split_lines = text.split('\n');

        // Special case single lines to prevent allocating a sharedstring
        if let Some(first_line) = split_lines.next()
            && let Some(second_line) = split_lines.next()
        {
            let mut line_start = 0;
            process_line(
                SharedString::new(first_line),
                line_start,
                line_start + first_line.len(),
            );
            line_start += first_line.len() + '\n'.len_utf8();
            process_line(
                SharedString::new(second_line),
                line_start,
                line_start + second_line.len(),
            );
            for line_text in split_lines {
                line_start += line_text.len() + '\n'.len_utf8();
                process_line(
                    SharedString::new(line_text),
                    line_start,
                    line_start + line_text.len(),
                );
            }
        } else {
            let end = text.len();
            process_line(text, 0, end);
        }

        self.font_runs_pool.lock().push(font_runs);

        Ok(lines)
    }

    /// The CSS `min-content` width of `text`: the width of its widest line when it
    /// breaks at every place `options` let it, which is the narrowest it goes without
    /// overflowing. It is the text laid out at a wrap width of nothing, so it follows
    /// the same rules for breaking as the text does, and counts the same tab stops,
    /// hanging spaces and `insets`.
    pub fn min_content_width(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        options: TextLayoutOptions,
        insets: &[TextInset],
    ) -> Result<Pixels> {
        self.min_content_width_inline(text, font_size, runs, options, insets, &[], Pixels::ZERO)
    }

    /// Like [`Self::min_content_width`], with the `inline` boxes of the text, which
    /// [`Self::shape_text_inline`] takes. An atomic inline is as wide as the advance it
    /// is given, so that is its min-content contribution.
    pub fn min_content_width_inline(
        &self,
        text: SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        options: TextLayoutOptions,
        insets: &[TextInset],
        inline: &[InlineBox],
        line_height: Pixels,
    ) -> Result<Pixels> {
        let lines = self.shape_text_inline(
            text,
            font_size,
            runs,
            Some(px(0.)),
            None,
            options.for_min_content(),
            insets,
            inline,
            line_height,
        )?;
        Ok(lines
            .iter()
            .flat_map(|line| line.line_widths())
            .fold(px(0.), Pixels::max))
    }

    /// The `inline` boxes of a text, as they apply to the line of it at `line`, with
    /// their fonts resolved. A box that continues past an end of the line is cut to it,
    /// and one with no part on it is dropped, with the boxes inside it.
    fn inline_on_line(&self, inline: &[InlineBox], line: Range<usize>) -> Vec<InlineSpec> {
        let mut specs: Vec<InlineSpec> = Vec::new();
        // Where each box of `inline` is in `specs`, once it has a spec.
        let mut spec_ix: Vec<Option<usize>> = Vec::with_capacity(inline.len());
        for inline_box in inline {
            let start = inline_box.range.start.max(line.start);
            let end = inline_box.range.end.min(line.end);
            let parent = inline_box
                .parent
                .and_then(|parent| spec_ix.get(parent).copied().flatten());
            let on_line = start < end && (inline_box.parent.is_none() || parent.is_some());
            if !on_line {
                spec_ix.push(None);
                continue;
            }
            let content = match &inline_box.content {
                InlineContent::Text { font, font_size } => SpecContent::Text {
                    font_id: self.resolve_font(font),
                    font_size: *font_size,
                },
                InlineContent::Atomic(extent) => {
                    debug_assert_eq!(
                        end - start,
                        OBJECT_REPLACEMENT_CHARACTER.len_utf8(),
                        "an atomic inline takes the place of one U+FFFC"
                    );
                    SpecContent::Atomic(*extent)
                }
            };
            spec_ix.push(Some(specs.len()));
            specs.push(InlineSpec {
                range: start - line.start..end - line.start,
                parent,
                align: inline_box.align,
                line_height: inline_box.line_height,
                wraps: inline_box.wraps,
                key: inline_box.key,
                content,
            });
        }
        specs
    }

    pub(crate) fn finish_frame(&self) {
        self.line_layout_cache.finish_frame()
    }

    /// Layout the given line of text, at the given font_size.
    /// Subsets of the line can be styled independently with the `runs` parameter.
    /// Generally, you should prefer to use [`Self::shape_line`] instead, which
    /// can be painted directly.
    pub fn layout_line(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[TextRun],
        force_width: Option<Pixels>,
    ) -> Arc<LineLayout> {
        let mut last_run = None::<&TextRun>;
        let mut font_runs = self.font_runs_pool.lock().pop().unwrap_or_default();
        font_runs.clear();

        for run in runs.iter() {
            let decoration_changed = if let Some(last_run) = last_run
                && last_run.color == run.color
                && last_run.font_smoothing == run.font_smoothing
                && last_run.underline == run.underline
                && last_run.strikethrough == run.strikethrough
                && last_run.overline == run.overline
                && last_run.extra_decorations == run.extra_decorations
            // we do not consider differing background color relevant, as it does not affect glyphs
            // && last_run.background_color == run.background_color
            {
                false
            } else {
                last_run = Some(run);
                true
            };

            let font_id = self.resolve_font(&run.font);
            if let Some(font_run) = font_runs.last_mut()
                && font_id == font_run.font_id
                && run.font_size == font_run.font_size
                && run.line_height == font_run.line_height
                && !decoration_changed
            {
                font_run.len += run.len;
            } else {
                font_runs.push(FontRun {
                    len: run.len,
                    font_id,
                    font_size: run.font_size,
                    line_height: run.line_height,
                });
            }
        }

        let layout = self.line_layout_cache.layout_line(
            &SharedString::new(text),
            font_size,
            &font_runs,
            force_width,
            None,
        );

        self.font_runs_pool.lock().push(font_runs);

        layout
    }

    /// Returns the shaped layout width of for the given character, in the given font and size.
    pub fn layout_width(&self, font_id: FontId, font_size: Pixels, ch: char) -> Pixels {
        let mut buffer = [0; 4];
        let buffer: &_ = ch.encode_utf8(&mut buffer);
        self.line_layout_cache
            .layout_line(
                buffer,
                font_size,
                &[FontRun {
                    len: buffer.len(),
                    font_id,
                    font_size: None,
                    line_height: None,
                }],
                None,
                None,
            )
            .width
    }

    /// Returns the shaped layout width of an `em`.
    pub fn em_layout_width(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.layout_width(font_id, font_size, 'm')
    }

    /// Probe the line layout cache using a caller-provided content hash, without allocating.
    ///
    /// Returns `Some(layout)` if the layout is already cached in either the current frame
    /// or the previous frame. Returns `None` if it is not cached.
    ///
    /// Contract (caller enforced):
    /// - Same `text_hash` implies identical text content (collision risk accepted by caller).
    /// - `text_len` should be the UTF-8 byte length of the text (helps reduce accidental collisions).
    pub fn try_layout_line_by_hash(
        &self,
        text_hash: u64,
        text_len: usize,
        font_size: Pixels,
        runs: &[TextRun],
        force_width: Option<Pixels>,
    ) -> Option<Arc<LineLayout>> {
        let mut last_run = None::<&TextRun>;
        let mut font_runs = self.font_runs_pool.lock().pop().unwrap_or_default();
        font_runs.clear();

        for run in runs.iter() {
            let decoration_changed = if let Some(last_run) = last_run
                && last_run.color == run.color
                && last_run.font_smoothing == run.font_smoothing
                && last_run.underline == run.underline
                && last_run.strikethrough == run.strikethrough
                && last_run.overline == run.overline
                && last_run.extra_decorations == run.extra_decorations
            // we do not consider differing background color relevant, as it does not affect glyphs
            // && last_run.background_color == run.background_color
            {
                false
            } else {
                last_run = Some(run);
                true
            };

            let font_id = self.resolve_font(&run.font);
            if let Some(font_run) = font_runs.last_mut()
                && font_id == font_run.font_id
                && run.font_size == font_run.font_size
                && run.line_height == font_run.line_height
                && !decoration_changed
            {
                font_run.len += run.len;
            } else {
                font_runs.push(FontRun {
                    len: run.len,
                    font_id,
                    font_size: run.font_size,
                    line_height: run.line_height,
                });
            }
        }

        let layout = self.line_layout_cache.try_layout_line_by_hash(
            text_hash,
            text_len,
            font_size,
            &font_runs,
            force_width,
        );

        self.font_runs_pool.lock().push(font_runs);

        layout
    }

    /// Layout the given line of text using a caller-provided content hash as the cache key.
    ///
    /// This enables cache hits without materializing a contiguous `SharedString` for the text.
    /// If the cache misses, `materialize_text` is invoked to produce the `SharedString` for shaping.
    ///
    /// Contract (caller enforced):
    /// - Same `text_hash` implies identical text content (collision risk accepted by caller).
    /// - `text_len` should be the UTF-8 byte length of the text (helps reduce accidental collisions).
    pub fn layout_line_by_hash(
        &self,
        text_hash: u64,
        text_len: usize,
        font_size: Pixels,
        runs: &[TextRun],
        force_width: Option<Pixels>,
        materialize_text: impl FnOnce() -> SharedString,
    ) -> Arc<LineLayout> {
        let mut last_run = None::<&TextRun>;
        let mut font_runs = self.font_runs_pool.lock().pop().unwrap_or_default();
        font_runs.clear();

        for run in runs.iter() {
            let decoration_changed = if let Some(last_run) = last_run
                && last_run.color == run.color
                && last_run.font_smoothing == run.font_smoothing
                && last_run.underline == run.underline
                && last_run.strikethrough == run.strikethrough
                && last_run.overline == run.overline
                && last_run.extra_decorations == run.extra_decorations
            // we do not consider differing background color relevant, as it does not affect glyphs
            // && last_run.background_color == run.background_color
            {
                false
            } else {
                last_run = Some(run);
                true
            };

            let font_id = self.resolve_font(&run.font);
            if let Some(font_run) = font_runs.last_mut()
                && font_id == font_run.font_id
                && run.font_size == font_run.font_size
                && run.line_height == font_run.line_height
                && !decoration_changed
            {
                font_run.len += run.len;
            } else {
                font_runs.push(FontRun {
                    len: run.len,
                    font_id,
                    font_size: run.font_size,
                    line_height: run.line_height,
                });
            }
        }

        let layout = self.line_layout_cache.layout_line_by_hash(
            text_hash,
            text_len,
            font_size,
            &font_runs,
            force_width,
            materialize_text,
        );

        self.font_runs_pool.lock().push(font_runs);

        layout
    }
}

/// The insets of a text, as they apply to the line of it at `line`, in bytes of
/// the line. An inset that continues past an end of the line keeps only the
/// advance at the end that is on it.
fn insets_on_line(insets: &[TextInset], line: Range<usize>) -> Vec<TextInset> {
    insets
        .iter()
        .filter_map(|inset| {
            let starts_here = (line.start..line.end).contains(&inset.range.start);
            let ends_here = inset.range.end > line.start && inset.range.end <= line.end;
            let leading = if starts_here {
                inset.leading
            } else {
                Pixels::ZERO
            };
            let trailing = if ends_here {
                inset.trailing
            } else {
                Pixels::ZERO
            };
            let start = inset.range.start.max(line.start);
            let end = inset.range.end.min(line.end);
            (start < end && (leading != Pixels::ZERO || trailing != Pixels::ZERO)).then(|| {
                TextInset {
                    range: start - line.start..end - line.start,
                    leading,
                    trailing,
                }
            })
        })
        .collect()
}

/// Splits the font run that `at` falls inside, so that no run crosses it. Shaping
/// does not join the glyphs of two runs, which keeps a ligature from crossing the
/// end of an inset.
fn split_font_run_at(font_runs: &mut Vec<FontRun>, at: usize) {
    let mut run_start = 0;
    for ix in 0..font_runs.len() {
        let run = font_runs[ix];
        if at > run_start && at < run_start + run.len {
            font_runs[ix].len = at - run_start;
            font_runs.insert(
                ix + 1,
                FontRun {
                    len: run_start + run.len - at,
                    font_id: run.font_id,
                    font_size: run.font_size,
                    line_height: run.line_height,
                },
            );
            return;
        }
        run_start += run.len;
    }
}

#[derive(Hash, Eq, PartialEq)]
struct FontIdWithSize {
    font_id: FontId,
    font_size: Pixels,
}

/// A handle into the text system, which can be used to compute the wrapped layout of text
pub struct LineWrapperHandle {
    wrapper: Option<LineWrapper>,
    text_system: Arc<TextSystem>,
}

impl Drop for LineWrapperHandle {
    fn drop(&mut self) {
        let mut state = self.text_system.wrapper_pool.lock();
        let wrapper = self.wrapper.take().unwrap();
        state
            .get_mut(&FontIdWithSize {
                font_id: wrapper.font_id,
                font_size: wrapper.font_size,
            })
            .unwrap()
            .push(wrapper);
    }
}

impl Deref for LineWrapperHandle {
    type Target = LineWrapper;

    fn deref(&self) -> &Self::Target {
        self.wrapper.as_ref().unwrap()
    }
}

impl DerefMut for LineWrapperHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.wrapper.as_mut().unwrap()
    }
}

/// The degree of blackness or stroke thickness of a font. This value ranges from 100.0 to 900.0,
/// with 400.0 as normal.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize, Add, Sub, FromStr)]
#[serde(transparent)]
pub struct FontWeight(pub f32);

impl Display for FontWeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<f32> for FontWeight {
    fn from(weight: f32) -> Self {
        FontWeight(weight)
    }
}

impl Default for FontWeight {
    #[inline]
    fn default() -> FontWeight {
        FontWeight::NORMAL
    }
}

impl Hash for FontWeight {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u32(u32::from_be_bytes(self.0.to_be_bytes()));
    }
}

impl Eq for FontWeight {}

impl FontWeight {
    /// Thin weight (100), the thinnest value.
    pub const THIN: FontWeight = FontWeight(100.0);
    /// Extra light weight (200).
    pub const EXTRA_LIGHT: FontWeight = FontWeight(200.0);
    /// Light weight (300).
    pub const LIGHT: FontWeight = FontWeight(300.0);
    /// Normal (400).
    pub const NORMAL: FontWeight = FontWeight(400.0);
    /// Medium weight (500, higher than normal).
    pub const MEDIUM: FontWeight = FontWeight(500.0);
    /// Semibold weight (600).
    pub const SEMIBOLD: FontWeight = FontWeight(600.0);
    /// Bold weight (700).
    pub const BOLD: FontWeight = FontWeight(700.0);
    /// Extra-bold weight (800).
    pub const EXTRA_BOLD: FontWeight = FontWeight(800.0);
    /// Black weight (900), the thickest value.
    pub const BLACK: FontWeight = FontWeight(900.0);

    /// All of the font weights, in order from thinnest to thickest.
    pub const ALL: [FontWeight; 9] = [
        Self::THIN,
        Self::EXTRA_LIGHT,
        Self::LIGHT,
        Self::NORMAL,
        Self::MEDIUM,
        Self::SEMIBOLD,
        Self::BOLD,
        Self::EXTRA_BOLD,
        Self::BLACK,
    ];
}

impl schemars::JsonSchema for FontWeight {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FontWeight".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        use schemars::json_schema;
        json_schema!({
            "type": "number",
            "minimum": Self::THIN,
            "maximum": Self::BLACK,
            "default": Self::default(),
            "description": "Font weight value between 100 (thin) and 900 (black)"
        })
    }
}

/// Allows italic or oblique faces to be selected.
#[derive(Clone, Copy, Eq, PartialEq, Debug, Hash, Default, Serialize, Deserialize, JsonSchema)]
pub enum FontStyle {
    /// A face that is neither italic not obliqued.
    #[default]
    Normal,
    /// A form that is generally cursive in nature.
    Italic,
    /// A typically-sloped version of the regular face.
    Oblique,
}

impl Display for FontStyle {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

/// How a glyph is rasterised, as CSS `-webkit-font-smoothing` on macOS.
#[derive(Clone, Copy, Eq, PartialEq, Debug, Hash, Default, Serialize, Deserialize, JsonSchema)]
pub enum FontSmoothing {
    /// The platform's font smoothing: on macOS, CoreGraphics smoothing, which thickens glyph
    /// strokes (more for light text on a dark ground), unless the user turned it off.
    #[default]
    Auto,
    /// Plain greyscale coverage with no smoothing, the same ink for dark text and light text.
    Antialiased,
}

/// A styled run of text, for use in [`crate::TextLayout`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct TextRun {
    /// A number of utf8 bytes
    pub len: usize,
    /// The font to use for this run.
    pub font: Font,
    /// The color
    pub color: Hsla,
    /// How the glyphs of this run are rasterised. It changes how they are painted and not
    /// what they measure.
    pub font_smoothing: FontSmoothing,
    /// The background color (if any)
    pub background_color: Option<Hsla>,
    /// The underline style (if any)
    pub underline: Option<UnderlineStyle>,
    /// The strikethrough style (if any)
    pub strikethrough: Option<StrikethroughStyle>,
    /// The font size of this run. `None` is the size the line is shaped at.
    pub font_size: Option<Pixels>,
    /// How much this run contributes to the height of its line box.
    /// `None` is the line height the line is painted with.
    pub line_height: Option<Pixels>,
    /// The overline style (if any)
    pub overline: Option<OverlineStyle>,
    /// The lines drawn besides `underline`, `strikethrough` and `overline`.
    pub extra_decorations: Vec<ExtraDecoration>,
    /// The paint group the run is in, `0` for none. A group is painted on its own, as a unit
    /// (see [`TextLayout::paint_group`](crate::TextLayout::paint_group)): painting a layout
    /// with [`TextLayout::paint`](crate::TextLayout::paint) leaves the runs of every group out.
    /// CSS composites an inline box with an `opacity` below one that way.
    pub group: u64,
}

impl TextRun {
    /// Whether `self` is painted with the same decoration as `other`, which lets a line merge
    /// their [`DecorationRun`]s and shape their text as one font run.
    fn same_decoration(&self, other: &Self) -> bool {
        self.color == other.color
            && self.font_smoothing == other.font_smoothing
            && self.underline == other.underline
            && self.strikethrough == other.strikethrough
            && self.overline == other.overline
            && self.extra_decorations == other.extra_decorations
            && self.background_color == other.background_color
            && self.group == other.group
    }

    /// Hashes what shaping and breaking the text read of `runs`, which is not what the text is
    /// painted with: two lists of runs with equal hashes make the same lines, which differ only
    /// in their [`DecorationRun`]s. Where the decoration changes is part of it, since a font
    /// run is shaped on its own and the text breaks between two of them.
    pub(crate) fn hash_measured(runs: &[TextRun], hasher: &mut impl Hasher) {
        let mut previous: Option<&TextRun> = None;
        for run in runs.iter().filter(|run| run.len > 0) {
            run.len.hash(hasher);
            run.font.hash(hasher);
            run.font_size.hash(hasher);
            run.line_height.hash(hasher);
            previous
                .is_none_or(|previous| !previous.same_decoration(run))
                .hash(hasher);
            previous = Some(run);
        }
    }

    /// Hashes what [`Self::hash_measured`] leaves out.
    pub(crate) fn hash_painted(runs: &[TextRun], hasher: &mut impl Hasher) {
        for run in runs.iter().filter(|run| run.len > 0) {
            run.color.hash(hasher);
            run.font_smoothing.hash(hasher);
            run.background_color.hash(hasher);
            run.underline.hash(hasher);
            run.strikethrough.hash(hasher);
            run.overline.hash(hasher);
            run.extra_decorations.hash(hasher);
            run.group.hash(hasher);
        }
    }
}

/// Adds `len` bytes of `run` to the end of `decoration_runs`, to the last one when it is
/// decorated the same. Returns whether it started a decoration run.
fn push_decoration(decoration_runs: &mut Vec<DecorationRun>, run: &TextRun, len: usize) -> bool {
    if let Some(last_run) = decoration_runs.last_mut()
        && last_run.color == run.color
                && last_run.font_smoothing == run.font_smoothing
        && last_run.underline == run.underline
        && last_run.strikethrough == run.strikethrough
        && last_run.overline == run.overline
        && last_run.extra_decorations == run.extra_decorations
        && last_run.background_color == run.background_color
        && last_run.group == run.group
    {
        last_run.len += len as u32;
        return false;
    }
    decoration_runs.push(DecorationRun {
        len: len as u32,
        color: run.color,
        font_smoothing: run.font_smoothing,
        background_color: run.background_color,
        underline: run.underline,
        strikethrough: run.strikethrough,
        overline: run.overline,
        extra_decorations: run.extra_decorations.clone(),
        group: run.group,
    });
    true
}

/// `lines`, shaped from the text `runs` cover and from runs that differ from these only in how
/// they are painted, decorated by `runs`. The lines share their layouts with `lines`.
pub(crate) fn redecorate_lines(
    lines: &[WrappedLine],
    runs: &[TextRun],
) -> SmallVec<[WrappedLine; 1]> {
    let mut runs = runs.iter().filter(|run| run.len > 0).map(|run| (run, run.len)).peekable();
    lines
        .iter()
        .map(|line| {
            let mut decoration_runs = Vec::new();
            let mut remaining = line.text.len();
            while remaining > 0 {
                let Some((run, run_len)) = runs.peek_mut() else {
                    break;
                };
                let len = cmp::min(remaining, *run_len);
                push_decoration(&mut decoration_runs, run, len);
                *run_len -= len;
                remaining -= len;
                if *run_len == 0 {
                    runs.next();
                }
            }
            // The newline between two lines is covered by the runs too.
            if let Some((_, run_len)) = runs.peek_mut() {
                *run_len -= 1;
                if *run_len == 0 {
                    runs.next();
                }
            }
            WrappedLine {
                layout: line.layout.clone(),
                text: line.text.clone(),
                decoration_runs,
            }
        })
        .collect()
}

#[cfg(all(target_os = "macos", test))]
impl TextRun {
    fn with_len(&self, len: usize) -> Self {
        let mut this = self.clone();
        this.len = len;
        this
    }
}

/// An identifier for a specific glyph, as returned by [`WindowTextSystem::layout_line`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[repr(C)]
pub struct GlyphId(pub u32);

/// Parameters for rendering a glyph, used as cache keys for raster bounds.
///
/// This struct identifies a specific glyph rendering configuration including
/// font, size, subpixel positioning, and scale factor. It's used to look up
/// cached raster bounds and sprite atlas entries.
#[derive(Clone, Debug, PartialEq)]
#[expect(missing_docs)]
pub struct RenderGlyphParams {
    pub font_id: FontId,
    pub glyph_id: GlyphId,
    pub font_size: Pixels,
    pub subpixel_variant: Point<u8>,
    pub scale_factor: f32,
    pub is_emoji: bool,
    pub subpixel_rendering: bool,
    pub luminance_bucket: u8,
    pub font_smoothing: FontSmoothing,
}

impl Eq for RenderGlyphParams {}

impl Hash for RenderGlyphParams {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.font_id.0.hash(state);
        self.glyph_id.0.hash(state);
        self.font_size.0.to_bits().hash(state);
        self.subpixel_variant.hash(state);
        self.scale_factor.to_bits().hash(state);
        self.is_emoji.hash(state);
        self.subpixel_rendering.hash(state);
        self.luminance_bucket.hash(state);
        self.font_smoothing.hash(state);
    }
}

/// The configuration details for identifying a specific font.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Font {
    /// The font family name.
    ///
    /// The special name ".SystemUIFont" is used to identify the system UI font, which varies based on platform.
    pub family: SharedString,

    /// The font features to use.
    pub features: FontFeatures,

    /// The variation axes of a variable font to set.
    pub variations: FontVariations,

    /// The fallbacks fonts to use.
    pub fallbacks: Option<FontFallbacks>,

    /// The font weight.
    pub weight: FontWeight,

    /// The font style.
    pub style: FontStyle,
}

impl Default for Font {
    fn default() -> Self {
        font(".SystemUIFont")
    }
}

/// Get a [`Font`] for a given name.
pub fn font(family: impl Into<SharedString>) -> Font {
    Font {
        family: family.into(),
        features: FontFeatures::default(),
        variations: FontVariations::default(),
        weight: FontWeight::default(),
        style: FontStyle::default(),
        fallbacks: None,
    }
}

impl Font {
    /// Set this Font to be bold
    pub fn bold(mut self) -> Self {
        self.weight = FontWeight::BOLD;
        self
    }

    /// Set this Font to be italic
    pub fn italic(mut self) -> Self {
        self.style = FontStyle::Italic;
        self
    }
}

/// A struct for storing font metrics.
/// It is used to define the measurements of a typeface.
#[derive(Clone, Copy, Debug)]
pub struct FontMetrics {
    /// The number of font units that make up the "em square",
    /// a scalable grid for determining the size of a typeface.
    pub units_per_em: u32,

    /// The vertical distance from the baseline of the font to the top of the glyph covers.
    pub ascent: f32,

    /// The vertical distance from the baseline of the font to the bottom of the glyph covers.
    pub descent: f32,

    /// The recommended additional space to add between lines of type.
    pub line_gap: f32,

    /// The suggested position of the underline.
    pub underline_position: f32,

    /// The suggested thickness of the underline.
    pub underline_thickness: f32,

    /// The height of a capital letter measured from the baseline of the font.
    pub cap_height: f32,

    /// The height of a lowercase x.
    pub x_height: f32,

    /// The outer limits of the area that the font covers.
    /// Corresponds to the xMin / xMax / yMin / yMax values in the OpenType `head` table
    pub bounding_box: Bounds<f32>,

    /// Whether the rounded ascent of the font is made taller by 15% of its content area,
    /// as Chrome does on macOS for Times, Helvetica and Courier, whose own metrics are
    /// tighter than the fonts they stand in for. See [`Self::line_box`].
    pub widens_ascent: bool,
}

/// The content area of the inline box of a font at a size, and the leading its line gap
/// adds to `line-height: normal`, as Chrome measures them: the whole device pixel each is
/// rounded to, so every line of the font is as tall as the next whatever the size. The
/// lengths are in pixels, which are a device pixel at scale 1 and a half at scale 2.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineBox {
    /// The distance from the baseline up to the top of the content area.
    pub ascent: Pixels,
    /// The distance from the baseline down to the bottom of the content area, positive.
    pub descent: Pixels,
    /// The font's line gap.
    pub line_gap: Pixels,
}

impl LineBox {
    /// The height of a line with `line-height: normal`.
    pub fn normal_line_height(&self) -> Pixels {
        self.ascent + self.descent + self.line_gap
    }

    /// How far above the baseline an inline box with this content area reaches in a line
    /// of `line_height`, and how far below it, on a display of `scale_factor`.
    pub fn extent(&self, line_height: Pixels, scale_factor: f32) -> (Pixels, Pixels) {
        extent_in_line(self.ascent, self.descent, line_height, scale_factor)
    }
}

/// How far above the baseline an inline box reaches in a line of `line_height`, and how
/// far below it, for a content area of `ascent` and `descent`: the line height is shared
/// around the content area as half-leading, whose share above the box is rounded down to
/// a whole device pixel (`scale_factor` of them to a pixel), as Chrome does.
pub fn extent_in_line(
    ascent: Pixels,
    descent: Pixels,
    line_height: Pixels,
    scale_factor: f32,
) -> (Pixels, Pixels) {
    let half_leading = ((line_height - ascent - descent) * scale_factor / 2.).floor() / scale_factor;
    let above = ascent + half_leading;
    (above, line_height - above)
}

impl FontMetrics {
    /// The content area of an inline box in this font at `font_size`, on a display of
    /// `scale_factor` device pixels to a pixel: Chrome zooms the font to the device before it
    /// measures it, so the metrics are rounded in device pixels.
    pub fn line_box(&self, font_size: Pixels, scale_factor: f32) -> LineBox {
        let device_size = font_size * scale_factor;
        let round = |length: Pixels| (length.0 + 0.5).floor();
        let mut ascent = round(self.ascent(device_size));
        let descent = round(-self.descent(device_size));
        if self.widens_ascent {
            ascent += ((ascent + descent) * 0.15 + 0.5).floor();
        }
        LineBox {
            ascent: px(ascent / scale_factor),
            descent: px(descent / scale_factor),
            line_gap: px(round(self.line_gap(device_size)) / scale_factor),
        }
    }

    /// Returns the vertical distance from the baseline of the font to the top of the glyph covers in pixels.
    pub fn ascent(&self, font_size: Pixels) -> Pixels {
        Pixels((self.ascent / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the vertical distance from the baseline of the font to the bottom of the glyph covers in pixels.
    pub fn descent(&self, font_size: Pixels) -> Pixels {
        Pixels((self.descent / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the recommended additional space to add between lines of type in pixels.
    pub fn line_gap(&self, font_size: Pixels) -> Pixels {
        Pixels((self.line_gap / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the suggested position of the underline in pixels.
    pub fn underline_position(&self, font_size: Pixels) -> Pixels {
        Pixels((self.underline_position / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the suggested thickness of the underline in pixels.
    pub fn underline_thickness(&self, font_size: Pixels) -> Pixels {
        Pixels((self.underline_thickness / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the height of a capital letter measured from the baseline of the font in pixels.
    pub fn cap_height(&self, font_size: Pixels) -> Pixels {
        Pixels((self.cap_height / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the height of a lowercase x in pixels.
    pub fn x_height(&self, font_size: Pixels) -> Pixels {
        Pixels((self.x_height / self.units_per_em as f32) * font_size.0)
    }

    /// Returns the outer limits of the area that the font covers in pixels.
    pub fn bounding_box(&self, font_size: Pixels) -> Bounds<Pixels> {
        (self.bounding_box / self.units_per_em as f32 * font_size.0).map(px)
    }
}

/// Maps well-known virtual font names to their concrete equivalents.
#[allow(unused)]
pub fn font_name_with_fallbacks<'a>(name: &'a str, system: &'a str) -> &'a str {
    // Note: the "Zed Plex" fonts were deprecated as we are not allowed to use "Plex"
    // in a derived font name. They are essentially indistinguishable from IBM Plex/Lilex,
    // and so retained here for backward compatibility.
    match name {
        ".SystemUIFont" => system,
        ".ZedSans" | "Zed Plex Sans" => "IBM Plex Sans",
        ".ZedMono" | "Zed Plex Mono" => "Lilex",
        _ => name,
    }
}

/// Like [`font_name_with_fallbacks`] but accepts and returns [`SharedString`] references.
#[allow(unused)]
pub fn font_name_with_fallbacks_shared<'a>(
    name: &'a SharedString,
    system: &'a SharedString,
) -> &'a SharedString {
    // Note: the "Zed Plex" fonts were deprecated as we are not allowed to use "Plex"
    // in a derived font name. They are essentially indistinguishable from IBM Plex/Lilex,
    // and so retained here for backward compatibility.
    match name.as_str() {
        ".SystemUIFont" => system,
        ".ZedSans" | "Zed Plex Sans" => const { &SharedString::new_static("IBM Plex Sans") },
        ".ZedMono" | "Zed Plex Mono" => const { &SharedString::new_static("Lilex") },
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inset(range: Range<usize>, leading: f32, trailing: f32) -> TextInset {
        TextInset {
            range,
            leading: px(leading),
            trailing: px(trailing),
        }
    }

    #[test]
    fn test_an_inset_across_a_newline_keeps_each_end_on_its_own_line() {
        // "ab\ncd\nef": the inset covers "b\ncd\ne".
        let insets = [inset(1..7, 5., 7.)];
        assert_eq!(insets_on_line(&insets, 0..2), vec![inset(1..2, 5., 0.)]);
        assert!(insets_on_line(&insets, 3..5).is_empty());
        assert_eq!(insets_on_line(&insets, 6..8), vec![inset(0..1, 0., 7.)]);
    }

    #[test]
    fn test_an_inset_ending_where_a_line_ends_is_on_that_line() {
        let insets = [inset(0..2, 3., 4.)];
        assert_eq!(insets_on_line(&insets, 0..2), vec![inset(0..2, 3., 4.)]);
        // Starting at the newline there is nothing of it on the line before.
        let insets = [inset(2..4, 3., 4.)];
        assert!(insets_on_line(&insets, 0..2).is_empty());
        assert_eq!(insets_on_line(&insets, 3..5), vec![inset(0..1, 0., 4.)]);
    }

    #[test]
    fn test_insets_that_add_nothing_are_dropped() {
        assert!(insets_on_line(&[inset(0..2, 0., 0.)], 0..4).is_empty());
    }

    fn font_runs(lens: &[usize]) -> Vec<FontRun> {
        lens.iter()
            .map(|len| FontRun {
                len: *len,
                font_id: FontId(0),
                font_size: None,
                line_height: None,
            })
            .collect()
    }

    fn lens(font_runs: &[FontRun]) -> Vec<usize> {
        font_runs.iter().map(|run| run.len).collect()
    }

    #[test]
    fn test_a_font_run_splits_where_an_inset_begins_or_ends() {
        let mut runs = font_runs(&[4, 6]);
        split_font_run_at(&mut runs, 2);
        assert_eq!(lens(&runs), vec![2, 2, 6]);
        split_font_run_at(&mut runs, 7);
        assert_eq!(lens(&runs), vec![2, 2, 3, 3]);
        // On a boundary, or at an end, there is nothing to split.
        for at in [0, 2, 4, 7, 10] {
            split_font_run_at(&mut runs, at);
        }
        assert_eq!(lens(&runs), vec![2, 2, 3, 3]);
    }

    fn hash_of(value: &impl Hash) -> u64 {
        use std::hash::{DefaultHasher, Hasher as _};
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn test_glyphs_rasterised_both_ways_do_not_share_a_cache_key() {
        let glyph = |font_smoothing| RenderGlyphParams {
            font_id: FontId(0),
            glyph_id: GlyphId(7),
            font_size: px(14.),
            subpixel_variant: Default::default(),
            scale_factor: 1.,
            is_emoji: false,
            subpixel_rendering: false,
            luminance_bucket: 0,
            font_smoothing,
        };
        let (auto, antialiased) = (glyph(FontSmoothing::Auto), glyph(FontSmoothing::Antialiased));
        assert_eq!(auto, glyph(FontSmoothing::Auto));
        assert_eq!(hash_of(&auto), hash_of(&glyph(FontSmoothing::Auto)));
        assert_ne!(auto, antialiased);
        assert_ne!(hash_of(&auto), hash_of(&antialiased));
    }

    #[test]
    fn test_runs_differing_in_font_smoothing_are_painted_differently_and_measure_alike() {
        let smoothed = |font_smoothing| TextRun {
            len: 3,
            font_smoothing,
            ..Default::default()
        };
        let painted = |runs: &[TextRun]| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            TextRun::hash_painted(runs, &mut hasher);
            hasher.finish()
        };
        let measured = |runs: &[TextRun]| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            TextRun::hash_measured(runs, &mut hasher);
            hasher.finish()
        };
        let (auto, antialiased) = ([smoothed(FontSmoothing::Auto)], [smoothed(FontSmoothing::Antialiased)]);
        assert_ne!(painted(&auto), painted(&antialiased));
        assert_eq!(measured(&auto), measured(&antialiased));
    }

    #[test]
    fn test_a_split_font_run_keeps_its_font_size_and_line_height() {
        let mut runs = vec![FontRun {
            len: 4,
            font_id: FontId(0),
            font_size: Some(px(32.)),
            line_height: Some(px(40.)),
        }];
        split_font_run_at(&mut runs, 1);
        assert_eq!(runs.len(), 2);
        for run in &runs {
            assert_eq!(run.font_size, Some(px(32.)));
            assert_eq!(run.line_height, Some(px(40.)));
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod insets_tests {
    use super::*;
    use crate::{TestAppContext, font, px};

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: font("Menlo"),
            font_size: None,
            line_height: None,
            ..Default::default()
        }
    }

    fn inset(range: Range<usize>, leading: f32, trailing: f32) -> TextInset {
        TextInset {
            range,
            leading: px(leading),
            trailing: px(trailing),
        }
    }

    fn shape(
        text_system: &WindowTextSystem,
        text: &str,
        wrap_width: Option<Pixels>,
        insets: &[TextInset],
    ) -> SmallVec<[WrappedLine; 1]> {
        text_system
            .shape_text_with_insets(
                text.to_string().into(),
                px(16.),
                &[run(text.len())],
                wrap_width,
                None,
                TextLayoutOptions::default(),
                insets,
            )
            .unwrap()
    }

    fn measured_hash(runs: &[TextRun]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        TextRun::hash_measured(runs, &mut hasher);
        hasher.finish()
    }

    fn colored(len: usize, color: Hsla) -> TextRun {
        TextRun { color, ..run(len) }
    }

    #[test]
    fn test_runs_measure_alike_when_only_their_paint_differs() {
        let (red, blue) = (crate::red(), crate::blue());
        let measured = measured_hash(&[colored(3, red), colored(4, blue)]);
        assert_eq!(measured, measured_hash(&[colored(3, blue), colored(4, red)]));
        assert_eq!(
            measured,
            measured_hash(&[TextRun { underline: Some(Default::default()), ..colored(3, red) }, colored(4, red)])
        );
        // Runs of one decoration are shaped as one font run, and two of them are not.
        assert_ne!(measured, measured_hash(&[colored(3, red), colored(4, red)]));
        assert_ne!(measured, measured_hash(&[colored(4, red), colored(3, blue)]));
        let bold = TextRun {
            font: font("Menlo").bold(),
            ..colored(3, red)
        };
        assert_ne!(measured, measured_hash(&[bold, colored(4, blue)]));
        let small = TextRun {
            font_size: Some(px(8.)),
            ..colored(3, red)
        };
        assert_ne!(measured, measured_hash(&[small, colored(4, blue)]));
    }

    #[crate::test]
    fn test_lines_decorated_again_follow_the_new_runs_across_newlines(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let (red, blue, green) = (crate::red(), crate::blue(), crate::green());
            let text: SharedString = "abc\nde\nfgh".into();
            let lines = text_system
                .shape_text(text, px(16.), &[colored(10, red)], None, None)
                .unwrap();
            let runs = [colored(2, blue), colored(5, green), colored(3, blue)];
            let lines = redecorate_lines(&lines, &runs);
            let decorations: Vec<Vec<(u32, Hsla)>> = lines
                .iter()
                .map(|line| line.decoration_runs.iter().map(|run| (run.len, run.color)).collect())
                .collect();
            assert_eq!(
                decorations,
                vec![
                    vec![(2, blue), (1, green)],
                    vec![(2, green)],
                    vec![(3, blue)],
                ]
            );
        });
    }

    #[crate::test]
    fn test_frames_that_look_nothing_up_keep_the_layouts_warm(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            shape(&text_system, "a label", Some(px(100.)), &[]);
            text_system.finish_frame();
            let shaped = text_system.text_counters();
            assert!(shaped.shapes > 0);

            // A frame that replays everything shapes and looks up nothing.
            for _ in 0..4 {
                text_system.finish_frame();
            }
            shape(&text_system, "a label", Some(px(100.)), &[]);
            let again = text_system.text_counters();
            assert_eq!(again.shapes, shaped.shapes);
            assert_eq!(again.wrapped_layouts, shaped.wrapped_layouts);
            assert_eq!(again.hits, shaped.hits + 1);

            // A frame that looks something else up ages it as before.
            text_system.finish_frame();
            shape(&text_system, "another label", None, &[]);
            text_system.finish_frame();
            text_system.finish_frame();
            shape(&text_system, "a label", Some(px(100.)), &[]);
            assert!(text_system.text_counters().shapes > again.shapes);
        });
    }

    #[crate::test]
    fn test_a_line_survives_the_frames_that_shape_other_text_in_the_shape_store(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let first = shape(&text_system, "a label", Some(px(100.)), &[]);
            let unwrapped = text_system.layout_line("a plain line", px(16.), &[run(12)], None);
            text_system.finish_frame();
            for frame in 0..30 {
                shape(&text_system, &format!("other text {frame}"), None, &[]);
                text_system.finish_frame();
            }
            let before = text_system.text_counters();

            let again = shape(&text_system, "a label", Some(px(100.)), &[]);
            let counters = text_system.text_counters();
            assert_eq!(counters.shapes, before.shapes);
            assert_eq!(counters.wrapped_layouts, before.wrapped_layouts);
            assert_eq!(counters.hits, before.hits);
            assert_eq!(counters.shape_cache_hits, before.shape_cache_hits + 1);
            assert!(Arc::ptr_eq(&first[0].layout, &again[0].layout));

            let unwrapped_again =
                text_system.layout_line("a plain line", px(16.), &[run(12)], None);
            let counters_after = text_system.text_counters();
            assert_eq!(counters_after.shapes, counters.shapes);
            assert_eq!(counters_after.shape_cache_hits, counters.shape_cache_hits + 1);
            assert!(Arc::ptr_eq(&unwrapped, &unwrapped_again));

            // The hit is in this frame like a shape would be: the frame after finds it as a hit.
            text_system.finish_frame();
            shape(&text_system, "a label", Some(px(100.)), &[]);
            let next = text_system.text_counters();
            assert_eq!(next.hits, counters_after.hits + 1);
            assert_eq!(next.shape_cache_hits, counters_after.shape_cache_hits);
        });
    }

    #[crate::test]
    fn test_shaped_text_is_as_wide_as_its_insets_add(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let plain = shape(&text_system, "abcd", None, &[]);
            let inset = shape(&text_system, "abcd", None, &[inset(1..3, 5., 7.)]);
            assert_eq!(inset[0].layout.width(), plain[0].layout.width() + px(12.));
            let plain_layout = &plain[0].layout.unwrapped_layout;
            let inset_layout = &inset[0].layout.unwrapped_layout;
            assert_eq!(inset_layout.x_for_index(1), plain_layout.x_for_index(1));
            assert_eq!(
                inset_layout.x_for_index(2),
                plain_layout.x_for_index(2) + px(5.)
            );
            assert_eq!(
                inset_layout.x_for_index(3),
                plain_layout.x_for_index(3) + px(12.)
            );
        });
    }

    #[crate::test]
    fn test_insets_are_part_of_what_a_layout_is_cached_by(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let a = shape(&text_system, "abcd", None, &[inset(1..3, 5., 7.)]);
            let again = shape(&text_system, "abcd", None, &[inset(1..3, 5., 7.)]);
            let wider = shape(&text_system, "abcd", None, &[inset(1..3, 5., 9.)]);
            let elsewhere = shape(&text_system, "abcd", None, &[inset(0..2, 5., 7.)]);
            let none = shape(&text_system, "abcd", None, &[]);
            assert!(Arc::ptr_eq(&a[0].layout, &again[0].layout));
            for other in [&wider, &elsewhere, &none] {
                assert!(!Arc::ptr_eq(&a[0].layout, &other[0].layout));
            }
            assert_eq!(wider[0].layout.width(), a[0].layout.width() + px(2.));
        });
    }

    fn sized_run(len: usize, font_size: Option<Pixels>, line_height: Option<Pixels>) -> TextRun {
        TextRun {
            font_size,
            line_height,
            ..run(len)
        }
    }

    #[crate::test]
    fn test_the_size_and_line_height_of_a_text_run_reach_the_shaped_runs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let lines = text_system
                .shape_text(
                    "aabbcc".into(),
                    px(16.),
                    &[
                        sized_run(2, None, None),
                        sized_run(2, Some(px(32.)), Some(px(40.))),
                        sized_run(2, Some(px(32.)), Some(px(40.))),
                    ],
                    None,
                    None,
                )
                .unwrap();
            let runs = &lines[0].layout.unwrapped_layout.runs;
            assert_eq!(runs.len(), 2);
            assert_eq!(runs[0].font_size, px(16.));
            assert_eq!(runs[0].line_height, None);
            assert_eq!(runs[1].font_size, px(32.));
            assert_eq!(runs[1].line_height, Some(px(40.)));
            assert_eq!(runs[1].glyphs.len(), 4);
        });
    }

    #[crate::test]
    fn test_runs_of_one_font_and_different_sizes_are_not_merged(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let layout = |runs: &[TextRun]| text_system.layout_line("abcd", px(16.), runs, None);
            let same = layout(&[sized_run(2, None, None), sized_run(2, None, None)]);
            let sized = layout(&[sized_run(2, None, None), sized_run(2, Some(px(32.)), None)]);
            let tall = layout(&[
                sized_run(2, None, Some(px(20.))),
                sized_run(2, None, Some(px(30.))),
            ]);
            assert_eq!(same.runs.len(), 1);
            assert_eq!(sized.runs.len(), 2);
            assert_eq!(tall.runs.len(), 2);
        });
    }

    #[crate::test]
    fn test_the_size_of_a_run_is_part_of_what_a_layout_is_cached_by(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let layout = |size: Option<Pixels>| {
                text_system.layout_line("abcd", px(16.), &[sized_run(4, size, None)], None)
            };
            let line_size = layout(None);
            let again = layout(None);
            let doubled = layout(Some(px(32.)));
            assert!(Arc::ptr_eq(&line_size, &again));
            assert!(!Arc::ptr_eq(&line_size, &doubled));
            assert!((f32::from(doubled.width) - 2. * f32::from(line_size.width)).abs() < 0.01);

            let by_hash = |size: Option<Pixels>| {
                text_system.layout_line_by_hash(
                    7,
                    4,
                    px(16.),
                    &[sized_run(4, size, None)],
                    None,
                    || "abcd".into(),
                )
            };
            assert!(
                (f32::from(by_hash(Some(px(32.))).width) - 2. * f32::from(by_hash(None).width))
                    .abs()
                    < 0.01
            );
        });
    }

    #[crate::test]
    fn test_an_inset_across_a_newline_has_each_end_on_its_own_line(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let lines = shape(&text_system, "ab\ncd\nef", None, &[inset(1..7, 5., 7.)]);
            assert_eq!(lines.len(), 3);
            let ends = |line: &WrappedLine| {
                line.layout
                    .insets
                    .iter()
                    .map(|inset| (inset.index, inset.leading, inset.trailing))
                    .collect::<Vec<_>>()
            };
            assert_eq!(ends(&lines[0]), vec![(1, px(5.), px(0.))]);
            assert!(ends(&lines[1]).is_empty());
            assert_eq!(ends(&lines[2]), vec![(0, px(0.), px(7.))]);
            let plain = shape(&text_system, "ab\ncd\nef", None, &[]);
            assert_eq!(lines[0].layout.width(), plain[0].layout.width() + px(5.));
            assert_eq!(lines[1].layout.width(), plain[1].layout.width());
            assert_eq!(lines[2].layout.width(), plain[2].layout.width() + px(7.));
        });
    }

    #[crate::test]
    fn test_wrapping_counts_the_insets(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let space = cx
                .text_system()
                .advance(cx.text_system().resolve_font(&font("Menlo")), px(16.), ' ')
                .unwrap()
                .width;
            // "aaa bbb" is 7 columns, and fits in 8.
            let wrap_width = Some(space * 8.);
            assert!(
                shape(&text_system, "aaa bbb", wrap_width, &[])[0]
                    .layout
                    .wrap_boundaries()
                    .is_empty()
            );
            // The padding after bbb does not, so bbb moves down.
            let lines = shape(
                &text_system,
                "aaa bbb",
                wrap_width,
                &[inset(4..7, 0., f32::from(space) * 2.)],
            );
            let line = &lines[0].layout;
            assert_eq!(line.wrap_boundaries().len(), 1);
            let boundary = line.wrap_boundaries()[0];
            assert_eq!(
                line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index,
                4
            );
        });
    }
}

#[cfg(test)]
mod layout_unit_tests;

#[cfg(test)]
mod line_box_tests {
    use super::*;
    use crate::{AbsoluteLength, LineHeight};

    /// Menlo's hhea metrics, in units of 2048.
    fn menlo() -> FontMetrics {
        FontMetrics {
            units_per_em: 2048,
            ascent: 1901.,
            descent: -483.,
            line_gap: 0.,
            underline_position: 0.,
            underline_thickness: 0.,
            cap_height: 0.,
            x_height: 0.,
            bounding_box: Bounds::default(),
            widens_ascent: false,
        }
    }

    /// Helvetica's, whose ascent Chrome on macOS widens.
    fn helvetica() -> FontMetrics {
        FontMetrics {
            ascent: 1577.,
            descent: -471.,
            widens_ascent: true,
            ..menlo()
        }
    }

    fn content(metrics: &FontMetrics, size: f32) -> (f32, f32, f32) {
        content_at(metrics, size, 1.)
    }

    fn content_at(metrics: &FontMetrics, size: f32, scale_factor: f32) -> (f32, f32, f32) {
        let line_box = metrics.line_box(px(size), scale_factor);
        (
            line_box.ascent.0,
            line_box.descent.0,
            line_box.normal_line_height().0,
        )
    }

    /// The `.text` and `normal` heights of `menlo.400.*` in Chrome (`line-box-oracle.json` of gpuix).
    #[test]
    fn test_the_ascent_and_descent_are_each_rounded_to_a_pixel() {
        assert_eq!(content(&menlo(), 16.), (15., 4., 19.));
        assert_eq!(content(&menlo(), 20.), (19., 5., 24.));
        assert_eq!(content(&menlo(), 24.), (22., 6., 28.));
    }

    /// The ascent and the descent are rounded in device pixels, as Chrome zooms the font to
    /// the display before it measures it: Menlo 20px is 19 and 5 CSS pixels at scale 1, and
    /// 37 and 9 device pixels at scale 2 (18.5 and 4.5).
    #[test]
    fn test_the_ascent_and_descent_are_each_rounded_to_a_device_pixel() {
        assert_eq!(content_at(&menlo(), 16., 2.), (15., 4., 19.));
        assert_eq!(content_at(&menlo(), 20., 2.), (18.5, 4.5, 23.));
        assert_eq!(content_at(&menlo(), 24., 2.), (22.5, 5.5, 28.));
        assert_eq!(content_at(&menlo(), 20., 1.), (19., 5., 24.));
    }

    /// `helvetica.400.*`: the ascent is widened by 15% of the content area, rounded.
    #[test]
    fn test_the_ascent_of_a_widened_font_grows_by_a_sixth_of_the_content() {
        assert_eq!(content(&helvetica(), 12.), (11., 3., 14.));
        assert_eq!(content(&helvetica(), 20.), (18., 5., 23.));
        assert_eq!(content(&helvetica(), 24.), (22., 6., 28.));
        let unwidened = FontMetrics {
            widens_ascent: false,
            ..helvetica()
        };
        assert_eq!(content(&unwidened, 24.), (18., 6., 24.));
    }

    /// The 15% is of the content area in device pixels, rounded there: Helvetica 20px
    /// gives 31 + 9 device pixels and 6 more, where at scale 1 it gives 15 + 5 and 3.
    #[test]
    fn test_the_widening_is_rounded_in_device_pixels() {
        assert_eq!(content_at(&helvetica(), 20., 2.), (18.5, 4.5, 23.));
        assert_eq!(content_at(&helvetica(), 12., 2.), (11., 3., 14.));
    }

    #[test]
    fn test_the_line_gap_is_rounded_into_normal() {
        let gapped = FontMetrics {
            line_gap: 41.,
            ..menlo()
        };
        // 41 / 2048 of 24px is 0.48px, 41 / 2048 of 40px is 0.8px.
        assert_eq!(content(&gapped, 24.), (22., 6., 28.));
        assert_eq!(content(&gapped, 40.), (37., 9., 47.));
    }

    #[test]
    fn test_half_leading_is_floored_above_the_text() {
        // Menlo at 16px in a line of 20px: 1px over, so the ascent has none and the descent 1.
        assert_eq!(extent_in_line(px(15.), px(4.), px(20.), 1.), (px(15.), px(5.)));
        // 24px: 5px over, 2px above and 3 below.
        assert_eq!(extent_in_line(px(15.), px(4.), px(24.), 1.), (px(17.), px(7.)));
        // A line shorter than the text has a negative half-leading, floored toward the top:
        // `menlo.400.16.1` is 16px tall and its text starts 2px above it.
        assert_eq!(extent_in_line(px(15.), px(4.), px(16.), 1.), (px(13.), px(3.)));
        // A line height that is not a whole pixel gives the descent the rest.
        assert_eq!(
            extent_in_line(px(15.), px(4.), px(20.390625), 1.),
            (px(15.), px(5.390625))
        );
    }

    /// The half-leading is floored in device pixels, so at scale 2 a line 5 CSS pixels
    /// (10 device pixels) over the text puts 2.5 of them above it, and the 24px line around
    /// Menlo 16px has its baseline at 17.5 (`atomic-trim-baseline-oracle.json`: 17.5).
    #[test]
    fn test_half_leading_is_floored_to_a_device_pixel() {
        assert_eq!(extent_in_line(px(15.), px(4.), px(24.), 2.), (px(17.5), px(6.5)));
        // 1 CSS pixel over is 2 device pixels: one above, at scale 1 none.
        assert_eq!(extent_in_line(px(15.), px(4.), px(20.), 2.), (px(15.5), px(4.5)));
        // 0.75 CSS pixel over is 1.5 device pixels: 0.75 floors to none.
        assert_eq!(extent_in_line(px(15.), px(4.), px(19.75), 2.), (px(15.), px(4.75)));
        // Below the text: -1.5 CSS pixels is -3 device pixels, -1.5 floors to -2.
        assert_eq!(extent_in_line(px(15.), px(4.), px(16.), 2.), (px(13.5), px(2.5)));
        assert_eq!(extent_in_line(px(15.), px(4.), px(24.), 4.), (px(17.5), px(6.5)));
    }

    #[test]
    fn test_a_line_height_is_laid_out_in_sixty_fourths_of_a_device_pixel_as_chrome_does() {
        // Headless Chrome (`getBoundingClientRect().height` of a block of one line, Helvetica Neue):
        // a multiple of the font size is cut, a length and a percentage are rounded, in the device
        // pixels of the display (`--force-device-scale-factor`).
        let lay_out = |line_height: LineHeight, size: f32, scale: f32| {
            line_height
                .to_laid_out_pixels(px(size).into(), px(16.), scale, || unreachable!())
                .0
        };
        let number = |value: f32| LineHeight::from(crate::relative(value));
        assert_eq!(lay_out(number(1.2), 12., 1.), 14.390625);
        assert_eq!(lay_out(number(1.2), 12., 2.), 14.3984375);
        assert_eq!(lay_out(number(1.15), 13., 1.), 14.9375);
        assert_eq!(lay_out(number(1.15), 13., 2.), 14.9453125);
        assert_eq!(lay_out(number(1.3), 15.5, 2.), 20.1484375);
        assert_eq!(lay_out(number(1.2), 13., 2.), 15.59375);
        // 14.95px is 956.8 sixty-fourths at scale 1: rounded up, where a number is cut.
        assert_eq!(lay_out(px(14.95).into(), 13., 1.), 14.953125);
        assert_eq!(lay_out(px(14.95).into(), 13., 2.), 14.953125);
        assert_eq!(lay_out(px(15.3).into(), 13., 2.), 15.296875);
        // A percentage computes to a length: 115% of 13px is 14.95px, 120% is 15.6px.
        assert_eq!(lay_out(px(13. * 1.15).into(), 13., 1.), 14.953125);
        assert_eq!(lay_out(px(13. * 1.2).into(), 13., 2.), 15.6015625);
        assert_eq!(lay_out(px(24.).into(), 13., 2.), 24.);
        assert_eq!(lay_out(px(0.001).into(), 13., 1.), 0.);
        assert_eq!(
            LineHeight::Normal.to_laid_out_pixels(px(11.).into(), px(16.), 2., || px(13.5)),
            px(13.5)
        );
    }

    #[test]
    fn test_normal_asks_the_font_and_a_length_does_not() {
        let size: AbsoluteLength = px(20.).into();
        assert_eq!(LineHeight::Normal.to_pixels(size, px(16.), || px(23.)), px(23.));
        assert_eq!(
            LineHeight::from(crate::relative(1.5)).to_pixels(size, px(16.), || unreachable!()),
            px(30.)
        );
        assert_eq!(
            LineHeight::from(px(24.)).to_pixels(size, px(16.), || unreachable!()),
            px(24.)
        );
        assert_eq!(TextStyle::default().line_height, LineHeight::Normal);
    }
}

