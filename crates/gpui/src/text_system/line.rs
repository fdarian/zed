use crate::{
    App, Bounds, DecorationThickness, DevicePixels, ExtraDecoration, FontId, GlyphInset, Half, Hsla, LineLayout, RowMetrics,
    OverlineStyle, Pixels, Point, RenderGlyphParams, Result, ShapedRun, SharedString,
    StrikethroughStyle, TextAlign, TextSystem, UnderlineStyle, Window, WrapBoundary,
    WrappedLineLayout, black, fill, point, px, size,
};
use derive_more::{Deref, DerefMut};
use smallvec::SmallVec;
use std::sync::Arc;

/// Pre-computed glyph data for efficient painting without per-glyph cache lookups.
///
/// This is produced by `ShapedLine::compute_glyph_raster_data` during prepaint
/// and consumed by `ShapedLine::paint_with_raster_data` during paint.
#[derive(Clone, Debug)]
pub struct GlyphRasterData {
    /// The raster bounds for each glyph, in paint order.
    pub bounds: Vec<Bounds<DevicePixels>>,
    /// The render params for each glyph (needed for sprite atlas lookup).
    pub params: Vec<RenderGlyphParams>,
}

/// Set the text decoration for a run of text.
#[derive(Debug, Clone)]
pub struct DecorationRun {
    /// The length of the run in utf-8 bytes.
    pub len: u32,

    /// The color for this run
    pub color: Hsla,

    /// The background color for this run
    pub background_color: Option<Hsla>,

    /// The underline style for this run
    pub underline: Option<UnderlineStyle>,

    /// The strikethrough style for this run
    pub strikethrough: Option<StrikethroughStyle>,

    /// The overline style for this run
    pub overline: Option<OverlineStyle>,

    /// The lines drawn besides the underline, strikethrough and overline
    pub extra_decorations: Vec<ExtraDecoration>,
}

/// A line of text that has been shaped and decorated.
#[derive(Clone, Default, Debug, Deref, DerefMut)]
pub struct ShapedLine {
    #[deref]
    #[deref_mut]
    pub(crate) layout: Arc<LineLayout>,
    /// The text that was shaped for this line.
    pub text: SharedString,
    pub(crate) decoration_runs: SmallVec<[DecorationRun; 32]>,
}

impl ShapedLine {
    /// The length of the line in utf-8 bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.layout.len
    }

    /// The width of the shaped line in pixels.
    ///
    /// This is the glyph advance width computed by the text shaping system and is useful for
    /// incrementally advancing a "pen" when painting multiple fragments on the same row.
    pub fn width(&self) -> Pixels {
        self.layout.width
    }

    /// Override the len, useful if you're rendering text a
    /// as text b (e.g. rendering invisibles).
    pub fn with_len(mut self, len: usize) -> Self {
        let layout = self.layout.as_ref();
        self.layout = Arc::new(LineLayout {
            font_size: layout.font_size,
            width: layout.width,
            ascent: layout.ascent,
            descent: layout.descent,
            runs: layout.runs.clone(),
            len,
            boxes: layout.boxes.clone(),
        });
        self
    }

    /// Paint the line of text to the window.
    pub fn paint(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        align_width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        paint_line(
            origin,
            &self.layout,
            line_height,
            align,
            align_width,
            &self.decoration_runs,
            &[],
            &[],
            &[],
            window,
            cx,
        )?;

        Ok(())
    }

    /// Paint the background of the line to the window.
    pub fn paint_background(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        align_width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        paint_line_background(
            origin,
            &self.layout,
            line_height,
            align,
            align_width,
            &self.decoration_runs,
            &[],
            &[],
            window,
            cx,
        )?;

        Ok(())
    }

    /// Split this shaped line at a byte index, returning `(prefix, suffix)`.
    ///
    /// - `prefix` contains glyphs for bytes `[0, byte_index)` with original positions.
    ///   Its width equals the x-advance up to the split point.
    /// - `suffix` contains glyphs for bytes `[byte_index, len)` with positions
    ///   shifted left so the first glyph starts at x=0, and byte indices rebased to 0.
    /// - Decoration runs are partitioned at the boundary; a run that straddles it is
    ///   split into two with adjusted lengths.
    /// - `font_size`, `ascent`, and `descent` are copied to both halves.
    pub fn split_at(&self, byte_index: usize) -> (ShapedLine, ShapedLine) {
        let (left_layout, right_layout) = self.layout.split_at(byte_index);

        // Partition decoration runs. A run straddling the boundary is split into two.
        let mut left_decorations = SmallVec::new();
        let mut right_decorations = SmallVec::new();
        let mut decoration_offset = 0u32;
        let split_point = byte_index as u32;

        for decoration in &self.decoration_runs {
            let run_end = decoration_offset + decoration.len;

            if run_end <= split_point {
                left_decorations.push(decoration.clone());
            } else if decoration_offset >= split_point {
                right_decorations.push(decoration.clone());
            } else {
                let left_len = split_point - decoration_offset;
                let right_len = run_end - split_point;
                left_decorations.push(DecorationRun {
                    len: left_len,
                    color: decoration.color,
                    background_color: decoration.background_color,
                    underline: decoration.underline,
                    strikethrough: decoration.strikethrough,
                    overline: decoration.overline,
                    extra_decorations: decoration.extra_decorations.clone(),
                });
                right_decorations.push(DecorationRun {
                    len: right_len,
                    color: decoration.color,
                    background_color: decoration.background_color,
                    underline: decoration.underline,
                    strikethrough: decoration.strikethrough,
                    overline: decoration.overline,
                    extra_decorations: decoration.extra_decorations.clone(),
                });
            }

            decoration_offset = run_end;
        }

        // Split text
        let left_text = if byte_index == self.text.len() {
            self.text.clone()
        } else {
            SharedString::new(&self.text[..byte_index])
        };
        let right_text = if byte_index == 0 {
            self.text.clone()
        } else {
            SharedString::new(&self.text[byte_index..])
        };

        let left = ShapedLine {
            layout: Arc::new(left_layout),
            text: left_text,
            decoration_runs: left_decorations,
        };

        let right = ShapedLine {
            layout: Arc::new(right_layout),
            text: right_text,
            decoration_runs: right_decorations,
        };

        (left, right)
    }
}

impl LineLayout {
    /// Paint this layout to the window, using the given decoration runs to color
    /// glyphs and draw underlines and strikethroughs.
    ///
    /// This is a lower-level alternative to [`ShapedLine::paint`] for callers that
    /// hold a bare layout and track decorations themselves.
    pub fn paint(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        align_width: Option<Pixels>,
        decoration_runs: &[DecorationRun],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        paint_line(
            origin,
            self,
            line_height,
            align,
            align_width,
            decoration_runs,
            &[],
            &[],
            &[],
            window,
            cx,
        )
    }

    /// Paint the background of this layout to the window, using the given
    /// decoration runs to determine background colors.
    ///
    /// This is a lower-level alternative to [`ShapedLine::paint_background`] for
    /// callers that hold a bare layout and track decorations themselves.
    pub fn paint_background(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        align_width: Option<Pixels>,
        decoration_runs: &[DecorationRun],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        paint_line_background(
            origin,
            self,
            line_height,
            align,
            align_width,
            decoration_runs,
            &[],
            &[],
            window,
            cx,
        )
    }
}

/// A line of text that has been shaped, decorated, and wrapped by the text layout system.
#[derive(Default, Debug, Deref, DerefMut)]
pub struct WrappedLine {
    #[deref]
    #[deref_mut]
    pub(crate) layout: Arc<WrappedLineLayout>,
    /// The text that was shaped for this line.
    pub text: SharedString,
    pub(crate) decoration_runs: Vec<DecorationRun>,
}

impl WrappedLine {
    /// The length of the underlying, unwrapped layout, in utf-8 bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.layout.len()
    }

    /// Paint this line of text to the window.
    pub fn paint(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        bounds: Option<Bounds<Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        let align_width = match bounds {
            Some(bounds) => Some(bounds.size.width),
            None => self.layout.wrap_width,
        };

        paint_line(
            origin,
            &self.layout.unwrapped_layout,
            line_height,
            align,
            align_width,
            &self.decoration_runs,
            &self.wrap_boundaries,
            &self.hanging_widths,
            &self.layout.insets,
            window,
            cx,
        )?;

        Ok(())
    }

    /// Where the atomic inlines of this line are in the window, as the top left corners
    /// of their margin boxes, when the line is painted at `origin` with `align` in
    /// `bounds`: the byte of each one's placeholder and its position.
    pub fn object_origins(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        bounds: Option<Bounds<Pixels>>,
    ) -> SmallVec<[(usize, Point<Pixels>); 2]> {
        let align_width = match bounds {
            Some(bounds) => bounds.size.width,
            None => self.layout.wrap_width.unwrap_or(self.layout.unwrapped_layout.width),
        };
        let layout = &self.layout.unwrapped_layout;
        self.layout
            .objects(line_height)
            .into_iter()
            .map(|object| {
                let row_start = match object.row_ix.checked_sub(1) {
                    Some(previous) => {
                        let boundary = self.wrap_boundaries[previous];
                        layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].position.x
                    }
                    None => Pixels::ZERO,
                };
                let row_x = aligned_origin_x(
                    origin,
                    align_width,
                    row_start,
                    &align,
                    layout,
                    self.wrap_boundaries.get(object.row_ix).as_ref(),
                    hanging_width(&self.hanging_widths, object.row_ix),
                );
                (object.index, point(row_x + object.x, origin.y + object.y))
            })
            .collect()
    }

    /// Paint the background of line of text to the window.
    pub fn paint_background(
        &self,
        origin: Point<Pixels>,
        line_height: Pixels,
        align: TextAlign,
        bounds: Option<Bounds<Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        let align_width = match bounds {
            Some(bounds) => Some(bounds.size.width),
            None => self.layout.wrap_width,
        };

        paint_line_background(
            origin,
            &self.layout.unwrapped_layout,
            line_height,
            align,
            align_width,
            &self.decoration_runs,
            &self.wrap_boundaries,
            &self.hanging_widths,
            window,
            cx,
        )?;

        Ok(())
    }
}

/// The thickness a decoration line has before painting snaps it. A length is `thickness`;
/// `from-font` is the font's own underline thickness when it has one, and the
/// `thickness` the style carries (the `auto` value) when it has none.
fn decoration_thickness(
    thickness: Pixels,
    kind: DecorationThickness,
    text_system: &TextSystem,
    font_id: FontId,
    font_size: Pixels,
) -> Pixels {
    if kind != DecorationThickness::FromFont {
        return thickness;
    }
    let font_thickness = text_system.underline_thickness(font_id, font_size);
    if font_thickness > px(0.) {
        font_thickness
    } else {
        thickness
    }
}

/// Paints `layout`. A glyph with an inset is painted after its leading inset, and
/// a decoration starts there when it begins on that glyph and ends before the
/// trailing inset of the glyph it ends after: the decoration of a CSS inline box
/// covers its text, not its padding.
///
/// Underlines and overlines are painted with the glyphs, below them. Strike-throughs
/// are painted over them, in a layer of their own after the last glyph.
fn paint_line(
    origin: Point<Pixels>,
    layout: &LineLayout,
    line_height: Pixels,
    align: TextAlign,
    align_width: Option<Pixels>,
    decoration_runs: &[DecorationRun],
    wrap_boundaries: &[WrapBoundary],
    hanging_widths: &[Pixels],
    insets: &[GlyphInset],
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let line_rows = layout.line_rows(wrap_boundaries, line_height);
    let rows = &line_rows.rows;
    let line_bounds = Bounds::new(
        origin,
        size(layout.width, rows.iter().map(|row| row.height).sum()),
    );
    window.paint_layer(line_bounds, |window| {
        let mut decoration_runs = decoration_runs.iter();
        let mut wraps = wrap_boundaries.iter().peekable();
        let mut line_ix = 0;
        let mut run_end = 0;
        let mut color = black();
        let mut open: Vec<OpenDecoration> = Vec::new();
        let mut finished: Vec<FinishedDecoration> = Vec::new();
        let text_system = cx.text_system().clone();
        let scale_factor = window.scale_factor();
        let mut glyph_origin = point(
            aligned_origin_x(
                origin,
                align_width.unwrap_or(layout.width),
                px(0.0),
                &align,
                layout,
                wraps.peek(),
                hanging_width(hanging_widths, 0),
            ),
            origin.y,
        );
        let mut prev_glyph_position = Point::default();
        let mut max_glyph_size = size(px(0.), px(0.));
        let mut first_glyph_x = origin.x;
        let mut glyph_insets = insets.iter().peekable();
        let mut glyph_count = 0;
        // The trailing inset of the glyph before the current one, which the
        // current glyph's position is after.
        let mut previous_trailing = Pixels::ZERO;
        for (run_ix, run) in layout.runs.iter().enumerate() {
            max_glyph_size = text_system.bounding_box(run.font_id, run.font_size).size;

            for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                glyph_origin.x += glyph.position.x - prev_glyph_position.x;
                if glyph_ix == 0 && run_ix == 0 {
                    first_glyph_x = glyph_origin.x;
                }
                let inset = glyph_insets.next_if(|inset| inset.glyph_ix == glyph_count);
                glyph_count += 1;
                let leading = inset.map_or(Pixels::ZERO, |inset| inset.leading);

                if wraps.peek() == Some(&&WrapBoundary { run_ix, glyph_ix }) {
                    wraps.next();
                    // The spaces that hang past the end of a row are not decorated.
                    let end_x = glyph_origin.x
                        - previous_trailing
                        - hanging_width(hanging_widths, line_ix);
                    let carried_on = glyph.index < run_end;
                    for line in std::mem::take(&mut open) {
                        finish_decoration_line(
                            &mut finished,
                            &line,
                            end_x,
                            glyph_origin.y,
                            max_glyph_size.width.half(),
                        );
                        if carried_on {
                            open.push(line);
                        }
                    }

                    glyph_origin.y += rows[line_ix].height;
                    line_ix += 1;
                    glyph_origin.x = aligned_origin_x(
                        origin,
                        align_width.unwrap_or(layout.width),
                        glyph.position.x,
                        &align,
                        layout,
                        wraps.peek(),
                        hanging_width(hanging_widths, line_ix),
                    );
                    for line in &mut open {
                        line.x = glyph_origin.x + leading;
                    }
                }
                prev_glyph_position = glyph.position;
                // Where this run sits in its row, which a vertical alignment moves.
                let run_baseline = rows[line_ix].baseline - line_rows.raise(run_ix, line_ix);
                let placement = DecorationPlacement {
                    layout,
                    rows,
                    run,
                    run_baseline,
                    row_ix: line_ix,
                    scale_factor,
                };

                if glyph.index >= run_end {
                    let mut style_run = decoration_runs.next();

                    // ignore style runs that apply to a partial glyph
                    while let Some(run) = style_run {
                        if glyph.index < run_end + (run.len as usize) {
                            break;
                        }
                        run_end += run.len as usize;
                        style_run = decoration_runs.next();
                    }

                    let wanted = style_run.map(wanted_lines).unwrap_or_default();
                    // A line that goes on across the runs is the same line, so that its
                    // pattern runs on; the others end at this glyph.
                    let mut unmatched: Vec<&ExtraDecoration> = wanted.iter().collect();
                    for line in std::mem::take(&mut open) {
                        match unmatched.iter().position(|source| **source == line.source) {
                            Some(found) => {
                                unmatched.remove(found);
                                open.push(line);
                            }
                            None => finish_decoration_line(
                                &mut finished,
                                &line,
                                glyph_origin.x - previous_trailing,
                                glyph_origin.y,
                                max_glyph_size.width.half(),
                            ),
                        }
                    }
                    if let Some(style_run) = style_run {
                        for source in unmatched {
                            let order = wanted
                                .iter()
                                .position(|line| line == source)
                                .unwrap_or_default();
                            open.push(open_decoration(
                                source,
                                order,
                                glyph_origin.x + leading,
                                style_run.color,
                                &placement,
                                &text_system,
                            ));
                        }
                        run_end += style_run.len as usize;
                        color = style_run.color;
                    } else {
                        run_end = layout.len;
                    }
                }

                // A line that a change of font, size or alignment moves ends where that
                // happens and goes on at its new place, as Chrome draws a line per fragment.
                for line in std::mem::take(&mut open) {
                    let top = placement.top(&line.style);
                    if top == line.top {
                        open.push(line);
                    } else if line.x >= glyph_origin.x + leading {
                        open.push(OpenDecoration { top, ..line });
                    } else {
                        finish_decoration_line(
                            &mut finished,
                            &line,
                            glyph_origin.x - previous_trailing,
                            glyph_origin.y,
                            max_glyph_size.width.half(),
                        );
                        open.push(OpenDecoration {
                            x: glyph_origin.x + leading,
                            top,
                            ..line
                        });
                    }
                }

                let painted_origin = point(glyph_origin.x + leading, glyph_origin.y);
                let max_glyph_bounds = Bounds {
                    origin: painted_origin,
                    size: max_glyph_size,
                };

                let content_mask = window.content_mask();
                // The box of the font starts at the top of the row, and a line taller than the font
                // puts the glyphs lower in it: a descender can show under the mask that the box is
                // over. A glyph that shows is painted, whatever mask a recording of it was made
                // under, so this only leaves out what is a font away from the mask.
                let reach = max_glyph_size.width.max(max_glyph_size.height);
                // The placeholder of an atomic inline is not a glyph: the element in
                // its place paints itself.
                if !run.inline.object && max_glyph_bounds.dilate(reach).intersects(&content_mask.bounds) {
                    let baseline_offset = point(px(0.0), run_baseline);
                    let vertical_offset = point(px(0.0), glyph.position.y);
                    if glyph.is_emoji {
                        window.paint_emoji(
                            painted_origin + baseline_offset + vertical_offset,
                            run.font_id,
                            glyph.id,
                            run.font_size,
                        )?;
                    } else {
                        window.paint_glyph(
                            painted_origin + baseline_offset + vertical_offset,
                            run.font_id,
                            glyph.id,
                            run.font_size,
                            color,
                        )?;
                    }
                }
                previous_trailing = inset.map_or(Pixels::ZERO, |inset| inset.trailing);
            }
        }

        let mut last_line_end_x = first_glyph_x + layout.width - previous_trailing;
        if let Some(boundary) = wrap_boundaries.last() {
            let run = &layout.runs[boundary.run_ix];
            let glyph = &run.glyphs[boundary.glyph_ix];
            last_line_end_x -= glyph.position.x;
        }
        for line in open {
            finish_decoration_line(
                &mut finished,
                &line,
                last_line_end_x,
                glyph_origin.y,
                max_glyph_size.width.half(),
            );
        }

        // The line of the innermost box is on top of those of the boxes around it.
        finished.sort_by_key(|line| line.order);
        for line in &finished {
            match &line.style {
                ExtraDecoration::Underline(style) => {
                    window.paint_underline(line.origin, line.width, style)
                }
                ExtraDecoration::Overline(style) => {
                    window.paint_overline(line.origin, line.width, style)
                }
                ExtraDecoration::Strikethrough(_) => {}
            }
        }
        if finished
            .iter()
            .any(|line| matches!(line.style, ExtraDecoration::Strikethrough(_)))
        {
            window.paint_layer(line_bounds, |window| {
                for line in &finished {
                    if let ExtraDecoration::Strikethrough(style) = &line.style {
                        window.paint_strikethrough(line.origin, line.width, style);
                    }
                }
            });
        }

        Ok(())
    })
}

fn paint_line_background(
    origin: Point<Pixels>,
    layout: &LineLayout,
    line_height: Pixels,
    align: TextAlign,
    align_width: Option<Pixels>,
    decoration_runs: &[DecorationRun],
    wrap_boundaries: &[WrapBoundary],
    hanging_widths: &[Pixels],
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let rows = layout.rows(wrap_boundaries, line_height);
    let line_bounds = Bounds::new(
        origin,
        size(layout.width, rows.iter().map(|row| row.height).sum()),
    );
    window.paint_layer(line_bounds, |window| {
        let mut decoration_runs = decoration_runs.iter();
        let mut wraps = wrap_boundaries.iter().peekable();
        let mut line_ix = 0;
        let mut run_end = 0;
        let mut current_background: Option<(Point<Pixels>, Hsla)> = None;
        let text_system = cx.text_system().clone();
        let mut glyph_origin = point(
            aligned_origin_x(
                origin,
                align_width.unwrap_or(layout.width),
                px(0.0),
                &align,
                layout,
                wraps.peek(),
                hanging_width(hanging_widths, 0),
            ),
            origin.y,
        );
        let mut prev_glyph_position = Point::default();
        let mut max_glyph_size = size(px(0.), px(0.));
        for (run_ix, run) in layout.runs.iter().enumerate() {
            max_glyph_size = text_system.bounding_box(run.font_id, run.font_size).size;

            for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                glyph_origin.x += glyph.position.x - prev_glyph_position.x;

                if wraps.peek() == Some(&&WrapBoundary { run_ix, glyph_ix }) {
                    wraps.next();
                    if let Some((background_origin, background_color)) = current_background.as_mut()
                    {
                        if glyph_origin.x == background_origin.x {
                            background_origin.x -= max_glyph_size.width.half()
                        }
                        window.paint_quad(fill(
                            Bounds {
                                origin: *background_origin,
                                size: size(
                                    glyph_origin.x - background_origin.x,
                                    rows[line_ix].height,
                                ),
                            },
                            *background_color,
                        ));
                        if glyph.index < run_end {
                            background_origin.x = origin.x;
                            background_origin.y += rows[line_ix].height;
                        } else {
                            current_background = None;
                        }
                    }

                    glyph_origin.y += rows[line_ix].height;
                    line_ix += 1;
                    glyph_origin.x = aligned_origin_x(
                        origin,
                        align_width.unwrap_or(layout.width),
                        glyph.position.x,
                        &align,
                        layout,
                        wraps.peek(),
                        hanging_width(hanging_widths, line_ix),
                    );
                }
                prev_glyph_position = glyph.position;

                let mut finished_background: Option<(Point<Pixels>, Hsla)> = None;
                if glyph.index >= run_end {
                    let mut style_run = decoration_runs.next();

                    // ignore style runs that apply to a partial glyph
                    while let Some(run) = style_run {
                        if glyph.index < run_end + (run.len as usize) {
                            break;
                        }
                        run_end += run.len as usize;
                        style_run = decoration_runs.next();
                    }

                    if let Some(style_run) = style_run {
                        if let Some((_, background_color)) = &mut current_background
                            && style_run.background_color.as_ref() != Some(background_color)
                        {
                            finished_background = current_background.take();
                        }
                        if let Some(run_background) = style_run.background_color {
                            current_background.get_or_insert((
                                point(glyph_origin.x, glyph_origin.y),
                                run_background,
                            ));
                        }
                        run_end += style_run.len as usize;
                    } else {
                        run_end = layout.len;
                        finished_background = current_background.take();
                    }
                }

                if let Some((mut background_origin, background_color)) = finished_background {
                    let mut width = glyph_origin.x - background_origin.x;
                    if background_origin.x == glyph_origin.x {
                        background_origin.x -= max_glyph_size.width.half();
                    };
                    window.paint_quad(fill(
                        Bounds {
                            origin: background_origin,
                            size: size(width, rows[line_ix].height),
                        },
                        background_color,
                    ));
                }
            }
        }

        let mut last_line_end_x = origin.x + layout.width;
        if let Some(boundary) = wrap_boundaries.last() {
            let run = &layout.runs[boundary.run_ix];
            let glyph = &run.glyphs[boundary.glyph_ix];
            last_line_end_x -= glyph.position.x;
        }

        if let Some((mut background_origin, background_color)) = current_background.take() {
            if last_line_end_x == background_origin.x {
                background_origin.x -= max_glyph_size.width.half()
            };
            window.paint_quad(fill(
                Bounds {
                    origin: background_origin,
                    size: size(last_line_end_x - background_origin.x, rows[line_ix].height),
                },
                background_color,
            ));
        }

        Ok(())
    })
}

/// A decoration line that has started and has not been painted yet.
struct OpenDecoration {
    x: Pixels,
    /// Where the style run that opened it listed it: the lines of the outer boxes first.
    order: usize,
    /// Below the top of the row it is on, so that it keeps its place when a vertical
    /// alignment moves the run it is drawn over.
    top: Pixels,
    /// The line with its colour and thickness resolved.
    style: ExtraDecoration,
    /// The line as the style run declared it, which a later run is compared with to see
    /// whether this is still the line it asks for.
    source: ExtraDecoration,
}

/// The lines a style run asks for, in the order they are painted: the lines of the boxes
/// around the innermost one come first, so that the innermost box's line is on top.
fn wanted_lines(style_run: &DecorationRun) -> SmallVec<[ExtraDecoration; 4]> {
    let mut lines: SmallVec<[ExtraDecoration; 4]> =
        style_run.extra_decorations.iter().copied().collect();
    lines.extend(style_run.underline.map(ExtraDecoration::Underline));
    lines.extend(style_run.strikethrough.map(ExtraDecoration::Strikethrough));
    lines.extend(style_run.overline.map(ExtraDecoration::Overline));
    lines
}

/// Where the lines drawn over a run go: the run, and the row it is in.
struct DecorationPlacement<'a> {
    layout: &'a LineLayout,
    rows: &'a [RowMetrics],
    run: &'a ShapedRun,
    /// The baseline of the run in its row, where a vertical alignment has moved it.
    run_baseline: Pixels,
    row_ix: usize,
    scale_factor: f32,
}

impl DecorationPlacement<'_> {
    fn ascent(&self) -> Pixels {
        if self.layout.is_styled(self.run) {
            self.run.ascent
        } else {
            self.layout.ascent
        }
    }

    /// The ascent of the font of the box `decorator`, which declared a line.
    fn decorator_ascent(&self, decorator: Option<u64>) -> Pixels {
        decorator
            .and_then(|key| self.layout.boxes.iter().find(|placed| placed.key == key))
            .map_or(self.layout.ascent, |placed| placed.ascent)
    }

    /// The top of the straight line of `line` below the top of its row, as Chrome puts it:
    ///
    /// - An underline is at the baseline of the box that declared it, whatever the text it is
    ///   drawn under is raised to, and `offset` is measured from there.
    /// - A strike-through is at two thirds of the ascent of the font of the box that declared
    ///   it, down from the top of the text it is drawn over (that text's own ascent, at its
    ///   own baseline), and centred on that.
    /// - An overline is on the top of the text it is drawn over, which the line grows up from.
    fn top(&self, line: &ExtraDecoration) -> Pixels {
        match line {
            ExtraDecoration::Underline(underline) => {
                self.rows[self.row_ix].baseline
                    - self.layout.box_raise(underline.decorator, self.row_ix, self.rows)
                    + underline_offset(underline.offset, underline.thickness, self.scale_factor)
            }
            ExtraDecoration::Strikethrough(strikethrough) => {
                self.run_baseline - self.ascent()
                    + self.decorator_ascent(strikethrough.decorator) * (2. / 3.)
                    - strikethrough.thickness * 0.5
            }
            ExtraDecoration::Overline(_) => self.run_baseline - self.ascent(),
        }
    }
}

/// `source` started at `x`, over the run `placement` says, with its colour and thickness
/// resolved against `run_color` and the font.
fn open_decoration(
    source: &ExtraDecoration,
    order: usize,
    x: Pixels,
    run_color: Hsla,
    placement: &DecorationPlacement,
    text_system: &TextSystem,
) -> OpenDecoration {
    let thickness = |thickness, kind| {
        decoration_thickness(
            thickness,
            kind,
            text_system,
            placement.run.font_id,
            placement.layout.font_size,
        )
    };
    let style = match source {
        ExtraDecoration::Underline(underline) => ExtraDecoration::Underline(UnderlineStyle {
            color: Some(underline.color.unwrap_or(run_color)),
            thickness: thickness(underline.thickness, underline.thickness_kind),
            ..*underline
        }),
        ExtraDecoration::Strikethrough(strikethrough) => {
            ExtraDecoration::Strikethrough(StrikethroughStyle {
                color: Some(strikethrough.color.unwrap_or(run_color)),
                thickness: thickness(strikethrough.thickness, strikethrough.thickness_kind),
                ..*strikethrough
            })
        }
        ExtraDecoration::Overline(overline) => ExtraDecoration::Overline(OverlineStyle {
            color: Some(overline.color.unwrap_or(run_color)),
            thickness: thickness(overline.thickness, overline.thickness_kind),
            ..*overline
        }),
    };
    OpenDecoration {
        x,
        order,
        top: placement.top(&style),
        style,
        source: *source,
    }
}

/// A decoration line that is complete: where it goes, and the order it is painted in.
struct FinishedDecoration {
    order: usize,
    origin: Point<Pixels>,
    width: Pixels,
    style: ExtraDecoration,
}

/// Ends `line` at `end_x`, in the row that starts at `row_top`.
fn finish_decoration_line(
    finished: &mut Vec<FinishedDecoration>,
    line: &OpenDecoration,
    end_x: Pixels,
    row_top: Pixels,
    half_glyph_width: Pixels,
) {
    let x = if line.x == end_x {
        line.x - half_glyph_width
    } else {
        line.x
    };
    finished.push(FinishedDecoration {
        order: line.order,
        origin: point(x, row_top + line.top),
        width: end_x - x,
        style: line.style,
    });
}

/// Where the top of an underline of `thickness` sits below the baseline, as Chrome
/// places it with `text-underline-position: auto`. An `auto` offset leaves a gap of
/// half the thickness, rounded up, and at least a device pixel; a length replaces
/// that gap and is rounded to the device pixel. The font's own underline position is
/// only for `text-underline-position: from-font`.
fn underline_offset(offset: Option<Pixels>, thickness: Pixels, scale_factor: f32) -> Pixels {
    let device_pixels = match offset {
        Some(offset) => (offset.0 * scale_factor).round(),
        None => (thickness.0 * scale_factor / 2.0).ceil().max(1.0),
    };
    px(device_pixels / scale_factor)
}

/// How wide the spaces that hang at the end of line `line_ix` are. A layout that
/// does not hang spaces has no entries, and nothing hangs.
fn hanging_width(hanging_widths: &[Pixels], line_ix: usize) -> Pixels {
    hanging_widths.get(line_ix).copied().unwrap_or(Pixels::ZERO)
}

fn aligned_origin_x(
    origin: Point<Pixels>,
    align_width: Pixels,
    last_glyph_x: Pixels,
    align: &TextAlign,
    layout: &LineLayout,
    wrap_boundary: Option<&&WrapBoundary>,
    hanging_width: Pixels,
) -> Pixels {
    let end_of_line = if let Some(WrapBoundary { run_ix, glyph_ix }) = wrap_boundary {
        layout.runs[*run_ix].glyphs[*glyph_ix].position.x
    } else {
        layout.width
    };

    let line_width = end_of_line - last_glyph_x - hanging_width;

    match align {
        TextAlign::Left => origin.x,
        TextAlign::Center => (origin.x * 2.0 + align_width - line_width) / 2.0,
        TextAlign::Right => origin.x + align_width - line_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FontId, GlyphId, ShapedGlyph, ShapedRun};

    #[test]
    fn an_auto_underline_offset_is_half_the_thickness_rounded_up_and_at_least_a_pixel() {
        for (thickness, expected) in [(0., 1.), (1., 1.), (1.6, 1.), (2., 1.), (2.2, 2.), (3., 2.), (5., 3.)] {
            assert_eq!(underline_offset(None, px(thickness), 1.), px(expected), "{thickness}");
        }
    }

    #[test]
    fn an_underline_offset_length_replaces_the_gap_and_rounds_to_the_device_pixel() {
        for (offset, expected) in [(4., 4.), (0., 0.), (-3., -3.), (4.4, 4.), (4.5, 5.), (-4.5, -5.)] {
            assert_eq!(underline_offset(Some(px(offset)), px(9.), 1.), px(expected), "{offset}");
        }
        assert_eq!(underline_offset(Some(px(4.3)), px(1.), 2.), px(4.5));
    }

    #[test]
    fn an_auto_underline_offset_counts_device_pixels() {
        assert_eq!(underline_offset(None, px(1.), 2.), px(0.5));
        assert_eq!(underline_offset(None, px(3.), 2.), px(1.5));
    }

    /// Helper: build a ShapedLine from glyph descriptors without the platform text system.
    /// Each glyph is described as (byte_index, x_position).
    fn make_shaped_line(
        text: &str,
        glyphs: &[(usize, f32)],
        width: f32,
        decorations: &[DecorationRun],
    ) -> ShapedLine {
        let shaped_glyphs: Vec<ShapedGlyph> = glyphs
            .iter()
            .map(|&(index, x)| ShapedGlyph {
                id: GlyphId(0),
                position: point(px(x), px(0.0)),
                index,
                is_emoji: false,
            })
            .collect();

        ShapedLine {
            layout: Arc::new(LineLayout {
                boxes: Vec::new(),
                font_size: px(16.0),
                width: px(width),
                ascent: px(12.0),
                descent: px(4.0),
                runs: vec![ShapedRun {
                    font_id: FontId(0),
                    font_size: px(16.0),
                    ascent: px(12.0),
                    descent: px(4.0),
                    line_height: None,
                    glyphs: shaped_glyphs,
                    inline: Default::default(),
                }],
                len: text.len(),
            }),
            text: SharedString::new(text),
            decoration_runs: SmallVec::from(decorations.to_vec()),
        }
    }

    #[test]
    fn test_split_at_invariants() {
        // Split "abcdef" at every possible byte index and verify structural invariants.
        let line = make_shaped_line(
            "abcdef",
            &[
                (0, 0.0),
                (1, 10.0),
                (2, 20.0),
                (3, 30.0),
                (4, 40.0),
                (5, 50.0),
            ],
            60.0,
            &[],
        );

        for i in 0..=6 {
            let (left, right) = line.split_at(i);

            assert_eq!(
                left.width() + right.width(),
                line.width(),
                "widths must sum at split={i}"
            );
            assert_eq!(
                left.len() + right.len(),
                line.len(),
                "lengths must sum at split={i}"
            );
            assert_eq!(
                format!("{}{}", left.text.as_ref(), right.text.as_ref()),
                "abcdef",
                "text must concatenate at split={i}"
            );
            assert_eq!(left.font_size, line.font_size, "font_size at split={i}");
            assert_eq!(right.ascent, line.ascent, "ascent at split={i}");
            assert_eq!(right.descent, line.descent, "descent at split={i}");
        }

        // Edge: split at 0 produces no left runs, full content on right
        let (left, right) = line.split_at(0);
        assert_eq!(left.runs.len(), 0);
        assert_eq!(right.runs[0].glyphs.len(), 6);

        // Edge: split at end produces full content on left, no right runs
        let (left, right) = line.split_at(6);
        assert_eq!(left.runs[0].glyphs.len(), 6);
        assert_eq!(right.runs.len(), 0);
    }

    #[test]
    fn test_split_at_glyph_rebasing() {
        // Two font runs (simulating a font fallback boundary at byte 3):
        //   run A (FontId 0): glyphs at bytes 0,1,2  positions 0,10,20
        //   run B (FontId 1): glyphs at bytes 3,4,5  positions 30,40,50
        // Successive splits simulate the incremental splitting done during wrap.
        let line = ShapedLine {
            layout: Arc::new(LineLayout {
                boxes: Vec::new(),
                font_size: px(16.0),
                width: px(60.0),
                ascent: px(12.0),
                descent: px(4.0),
                runs: vec![
                    ShapedRun {
                        font_id: FontId(0),
                        font_size: px(16.0),
                        ascent: px(12.0),
                        descent: px(4.0),
                        line_height: None,
                        glyphs: vec![
                            ShapedGlyph {
                                id: GlyphId(0),
                                position: point(px(0.0), px(0.0)),
                                index: 0,
                                is_emoji: false,
                            },
                            ShapedGlyph {
                                id: GlyphId(0),
                                position: point(px(10.0), px(0.0)),
                                index: 1,
                                is_emoji: false,
                            },
                            ShapedGlyph {
                                id: GlyphId(0),
                                position: point(px(20.0), px(0.0)),
                                index: 2,
                                is_emoji: false,
                            },
                        ],
                        inline: Default::default(),
                    },
                    ShapedRun {
                        font_id: FontId(1),
                        font_size: px(16.0),
                        ascent: px(12.0),
                        descent: px(4.0),
                        line_height: None,
                        glyphs: vec![
                            ShapedGlyph {
                                id: GlyphId(0),
                                position: point(px(30.0), px(0.0)),
                                index: 3,
                                is_emoji: false,
                            },
                            ShapedGlyph {
                                id: GlyphId(0),
                                position: point(px(40.0), px(0.0)),
                                index: 4,
                                is_emoji: false,
                            },
                            ShapedGlyph {
                                id: GlyphId(0),
                                position: point(px(50.0), px(0.0)),
                                index: 5,
                                is_emoji: false,
                            },
                        ],
                        inline: Default::default(),
                    },
                ],
                len: 6,
            }),
            text: "abcdef".into(),
            decoration_runs: SmallVec::new(),
        };

        // First split at byte 2 — mid-run in run A
        let (first, remainder) = line.split_at(2);
        assert_eq!(first.text.as_ref(), "ab");
        assert_eq!(first.runs.len(), 1);
        assert_eq!(first.runs[0].font_id, FontId(0));

        // Remainder "cdef" should have two runs: tail of A (1 glyph) + all of B (3 glyphs)
        assert_eq!(remainder.text.as_ref(), "cdef");
        assert_eq!(remainder.runs.len(), 2);
        assert_eq!(remainder.runs[0].font_id, FontId(0));
        assert_eq!(remainder.runs[0].glyphs.len(), 1);
        assert_eq!(remainder.runs[0].glyphs[0].index, 0);
        assert_eq!(remainder.runs[0].glyphs[0].position.x, px(0.0));
        assert_eq!(remainder.runs[1].font_id, FontId(1));
        assert_eq!(remainder.runs[1].glyphs[0].index, 1);
        assert_eq!(remainder.runs[1].glyphs[0].position.x, px(10.0));

        // Second split at byte 2 within remainder — crosses the run boundary
        let (second, final_part) = remainder.split_at(2);
        assert_eq!(second.text.as_ref(), "cd");
        assert_eq!(final_part.text.as_ref(), "ef");
        assert_eq!(final_part.runs[0].glyphs[0].index, 0);
        assert_eq!(final_part.runs[0].glyphs[0].position.x, px(0.0));

        // Widths must sum across all three pieces
        assert_eq!(
            first.width() + second.width() + final_part.width(),
            line.width()
        );
    }

    #[test]
    fn test_split_at_decorations() {
        // Three decoration runs: red [0..2), green [2..5), blue [5..6).
        // Split at byte 3 — red goes entirely left, green straddles, blue goes entirely right.
        let red = Hsla {
            h: 0.0,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let green = Hsla {
            h: 0.3,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let blue = Hsla {
            h: 0.6,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };

        let line = make_shaped_line(
            "abcdef",
            &[
                (0, 0.0),
                (1, 10.0),
                (2, 20.0),
                (3, 30.0),
                (4, 40.0),
                (5, 50.0),
            ],
            60.0,
            &[
                DecorationRun {
                    len: 2,
                    color: red,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                    overline: None,
                    extra_decorations: Vec::new(),
                },
                DecorationRun {
                    len: 3,
                    color: green,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                    overline: None,
                    extra_decorations: Vec::new(),
                },
                DecorationRun {
                    len: 1,
                    color: blue,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                    overline: None,
                    extra_decorations: Vec::new(),
                },
            ],
        );

        let (left, right) = line.split_at(3);

        // Left: red(2) + green(1) — green straddled, left portion has len 1
        assert_eq!(left.decoration_runs.len(), 2);
        assert_eq!(left.decoration_runs[0].len, 2);
        assert_eq!(left.decoration_runs[0].color, red);
        assert_eq!(left.decoration_runs[1].len, 1);
        assert_eq!(left.decoration_runs[1].color, green);

        // Right: green(2) + blue(1) — green straddled, right portion has len 2
        assert_eq!(right.decoration_runs.len(), 2);
        assert_eq!(right.decoration_runs[0].len, 2);
        assert_eq!(right.decoration_runs[0].color, green);
        assert_eq!(right.decoration_runs[1].len, 1);
        assert_eq!(right.decoration_runs[1].color, blue);
    }

    fn aligned(
        layout: &LineLayout,
        align: TextAlign,
        last_glyph_x: f32,
        boundary: Option<WrapBoundary>,
        hanging: f32,
    ) -> Pixels {
        aligned_origin_x(
            point(px(0.), px(0.)),
            px(100.),
            px(last_glyph_x),
            &align,
            layout,
            boundary.as_ref().as_ref(),
            px(hanging),
        )
    }

    #[test]
    fn test_hanging_spaces_are_left_out_of_the_aligned_width() {
        // "aaaa bbbb", wrapped after the space: the first line is 50 wide with a
        // space at its end, the second 40.
        let glyphs: Vec<(usize, f32)> = (0..9).map(|ix| (ix, ix as f32 * 10.)).collect();
        let line = make_shaped_line("aaaa bbbb", &glyphs, 90., &[]);
        let layout = &line.layout;
        let boundary = Some(WrapBoundary {
            run_ix: 0,
            glyph_ix: 5,
        });

        assert_eq!(
            aligned(layout, TextAlign::Center, 0., boundary, 10.),
            px(30.)
        );
        assert_eq!(
            aligned(layout, TextAlign::Right, 0., boundary, 10.),
            px(60.)
        );
        assert_eq!(aligned(layout, TextAlign::Left, 0., boundary, 10.), px(0.));
        // Counted in, as when nothing hangs.
        assert_eq!(
            aligned(layout, TextAlign::Center, 0., boundary, 0.),
            px(25.)
        );
        assert_eq!(aligned(layout, TextAlign::Right, 0., boundary, 0.), px(50.));
        // The last line has nothing hanging here.
        assert_eq!(aligned(layout, TextAlign::Center, 50., None, 0.), px(30.));
        assert_eq!(aligned(layout, TextAlign::Right, 50., None, 0.), px(60.));
    }

    #[test]
    fn test_a_line_that_hangs_nothing_has_no_hanging_width() {
        let widths = [px(10.), px(0.)];
        assert_eq!(hanging_width(&widths, 0), px(10.));
        assert_eq!(hanging_width(&widths, 1), px(0.));
        assert_eq!(hanging_width(&[], 0), px(0.));
    }

    mod paint {
        use super::*;
        use crate::{
            Context, IntoElement, ParentElement as _, RasterizingTextSystem, Render, RowMetrics,
            Styled as _, StyledText, TestApp, TextLayout, TextRun, WrappedLineLayout, blue, div,
            font, red,
        };
        use std::{cell::RefCell, rc::Rc};

        const LINE_HEIGHT: f32 = 30.;
        const FONT_SIZE: f32 = 10.;
        const BIG_FONT_SIZE: f32 = 20.;

        struct Paragraph {
            text: &'static str,
            runs: Vec<TextRun>,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for Paragraph {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let text = StyledText::new(self.text).with_runs(self.runs.clone());
                *self.layout.borrow_mut() = Some(text.layout().clone());
                div()
                    .w(px(60.))
                    .text_size(px(FONT_SIZE))
                    .line_height(px(LINE_HEIGHT))
                    .child(text)
            }
        }

        struct Painted {
            layout: Arc<WrappedLineLayout>,
            rows: Vec<RowMetrics>,
            /// The y of the top of each row, in logical pixels.
            row_tops: Vec<f32>,
            /// The y of each glyph's baseline, in logical pixels, ascending: the scene
            /// does not keep the sprites in the order of the text.
            glyphs: Vec<f32>,
            /// The y and the height of each quad.
            quads: Vec<(f32, f32)>,
            /// The y and the thickness of each underline and strikethrough.
            lines: Vec<(f32, f32)>,
        }

        impl Painted {
            fn baseline(&self, row: usize) -> f32 {
                self.row_tops[row] + f32::from(self.rows[row].baseline)
            }

            fn row_of(&self, index: usize) -> usize {
                self.layout
                    .wrap_boundaries()
                    .iter()
                    .filter(|boundary| {
                        self.layout.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index <= index
                    })
                    .count()
            }

            /// The baselines the glyphs of `0..len` should be painted at, ascending.
            fn expected_glyphs(&self, len: usize, baseline: impl Fn(usize) -> f32) -> Vec<f32> {
                let mut expected = (0..len).map(baseline).collect::<Vec<_>>();
                expected.sort_by(f32::total_cmp);
                expected
            }

            fn run(&self, index: usize) -> &ShapedRun {
                self.layout
                    .runs()
                    .iter()
                    .find(|run| run.glyphs.iter().any(|glyph| glyph.index == index))
                    .unwrap()
            }
        }

        fn plain(len: usize) -> TextRun {
            TextRun {
                len,
                font: font("Menlo"),
                color: red(),
                ..Default::default()
            }
        }

        fn big(len: usize, line_height: Option<f32>) -> TextRun {
            TextRun {
                font_size: Some(px(BIG_FONT_SIZE)),
                line_height: line_height.map(px),
                ..plain(len)
            }
        }

        fn underlined(run: TextRun) -> TextRun {
            TextRun {
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..run
            }
        }

        fn underlined_at(run: TextRun, thickness: Pixels, offset: Option<Pixels>) -> TextRun {
            TextRun {
                underline: Some(UnderlineStyle {
                    thickness,
                    offset,
                    ..Default::default()
                }),
                ..run
            }
        }

        fn struck(run: TextRun) -> TextRun {
            TextRun {
                strikethrough: Some(StrikethroughStyle {
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..run
            }
        }

        fn filled(run: TextRun) -> TextRun {
            TextRun {
                background_color: Some(blue()),
                ..run
            }
        }

        /// "aaaa " plain, "bbbb " big, "cc " plain and "dd" big: wrapped into a row of
        /// plain text, a row of big text and a row of both. The big runs have different
        /// line heights, so the last two rows are different heights.
        fn mixed(decorate: impl Fn(usize, TextRun) -> TextRun) -> Painted {
            let runs = vec![plain(5), big(5, Some(50.)), plain(3), big(2, Some(70.))];
            let painted = paint(
                "aaaa bbbb cc dd",
                runs.into_iter()
                    .enumerate()
                    .map(|(ix, run)| decorate(ix, run))
                    .collect(),
            );
            assert_eq!(painted.layout.wrap_boundaries().len(), 2);
            assert_eq!((painted.row_of(5), painted.row_of(10)), (1, 2));
            assert_ne!(painted.rows[1].height, painted.rows[2].height);
            assert_ne!(painted.rows[1].baseline, painted.rows[2].baseline);
            painted
        }

        fn paint(text: &'static str, runs: Vec<TextRun>) -> Painted {
            let mut app = TestApp::with_text_system(Arc::new(RasterizingTextSystem::new()));
            let layout = Rc::new(RefCell::new(None));
            let mut window = app.open_window({
                let layout = layout.clone();
                move |_, _| Paragraph { text, runs, layout }
            });
            window.draw();
            let layout = layout.borrow().clone().unwrap();
            let wrapped = layout.line_layouts()[0].clone();
            let rows = wrapped.rows(px(LINE_HEIGHT)).to_vec();
            let mut row_tops = Vec::new();
            let mut top = f32::from(layout.bounds().origin.y);
            for row in &rows {
                row_tops.push(top);
                top += f32::from(row.height);
            }
            window.update(|_, window, _| {
                let scale = window.scale_factor();
                let scene = &window.rendered_frame.scene;
                let mut glyphs = scene
                    .monochrome_sprites
                    .iter()
                    .map(|sprite| sprite.bounds.origin.y.0 / scale)
                    .collect::<Vec<_>>();
                glyphs.sort_by(f32::total_cmp);
                Painted {
                    layout: wrapped,
                    rows,
                    row_tops,
                    glyphs,
                    quads: scene
                        .quads
                        .iter()
                        .map(|quad| {
                            (
                                quad.bounds.origin.y.0 / scale,
                                quad.bounds.size.height.0 / scale,
                            )
                        })
                        .collect(),
                    lines: scene
                        .underlines
                        .iter()
                        .map(|line| (line.bounds.origin.y.0 / scale, line.thickness.0 / scale))
                        .collect(),
                }
            })
        }

        fn assert_close(actual: f32, expected: f32, context: &str) {
            // Positions snap to a device pixel.
            assert!(
                (actual - expected).abs() <= 1.,
                "{context}: {actual} != {expected}"
            );
        }

        fn assert_all_close(actual: &[f32], expected: &[f32], context: &str) {
            assert_eq!(actual.len(), expected.len(), "{context}");
            for (ix, (actual, expected)) in actual.iter().zip(expected).enumerate() {
                assert_close(*actual, *expected, &format!("{context} {ix}"));
            }
        }

        #[test]
        fn glyphs_of_one_row_share_a_baseline_whatever_their_size() {
            let painted = mixed(|_, run| run);
            let expected =
                painted.expected_glyphs(15, |index| painted.baseline(painted.row_of(index)));
            assert_all_close(&painted.glyphs, &expected, "glyph");
            // "cc " is plain and "dd" big, in the same row.
            let last_row = painted.baseline(2);
            let in_last_row = painted
                .glyphs
                .iter()
                .filter(|y| (**y - last_row).abs() <= 1.)
                .count();
            assert_eq!(in_last_row, 5);
        }

        #[test]
        fn a_row_starts_where_the_row_before_it_ends() {
            let painted = mixed(|_, run| run);
            let mut baselines = painted.glyphs.clone();
            baselines.dedup_by(|a, b| (*a - *b).abs() <= 1.);
            assert_eq!(baselines.len(), 3);
            let rows = &painted.rows;
            assert_close(
                baselines[1] - baselines[0],
                f32::from(rows[0].height - rows[0].baseline + rows[1].baseline),
                "second row",
            );
            assert_close(
                baselines[2] - baselines[1],
                f32::from(rows[1].height - rows[1].baseline + rows[2].baseline),
                "third row",
            );
        }

        #[test]
        fn a_background_fills_the_height_of_each_row() {
            let painted = mixed(|ix, run| if ix == 0 { run } else { filled(run) });
            assert_eq!(painted.quads.len(), 2);
            for (quad, row) in painted.quads.iter().zip([1, 2]) {
                assert_close(quad.0, painted.row_tops[row], "quad top");
                assert_close(quad.1, f32::from(painted.rows[row].height), "quad height");
            }
        }

        #[test]
        fn an_underline_on_big_text_sits_a_pixel_below_the_baseline_whatever_the_descent() {
            let painted = mixed(|ix, run| if ix == 0 { run } else { underlined(run) });
            assert_ne!(painted.run(5).descent, painted.layout.descent());
            assert_eq!(painted.lines.len(), 2);
            assert_close(
                painted.lines[0].0,
                painted.baseline(1) + 1.,
                "underline in its first row",
            );
        }

        #[test]
        fn a_wrapped_underline_keeps_its_offset_from_the_baseline() {
            let painted = mixed(|ix, run| if ix == 0 { run } else { underlined(run) });
            assert_eq!(painted.lines.len(), 2);
            assert_close(
                painted.lines[1].0,
                painted.baseline(2) + 1.,
                "underline in its second row",
            );
        }

        #[test]
        fn an_underline_on_plain_text_follows_the_baseline_of_a_taller_row() {
            let painted = mixed(|ix, run| if ix == 2 { underlined(run) } else { run });
            assert_eq!(painted.lines.len(), 1);
            assert_close(
                painted.lines[0].0,
                painted.baseline(2) + 1.,
                "underline of the plain run",
            );
        }

        #[test]
        fn an_underline_offset_is_measured_from_the_baseline_of_every_row() {
            for offset in [4., 0., -3.] {
                let painted = mixed(|ix, run| {
                    if ix == 0 { run } else { underlined_at(run, px(1.), Some(px(offset))) }
                });
                assert_eq!(painted.lines.len(), 2);
                for (line, row) in painted.lines.iter().zip([1, 2]) {
                    assert_close(line.0, painted.baseline(row) + offset, "underline");
                }
            }
        }

        #[test]
        fn an_auto_offset_leaves_a_gap_of_half_the_thickness() {
            for (thickness, gap) in [(1., 1.), (2., 1.), (3., 2.), (4., 2.)] {
                let painted = mixed(|ix, run| {
                    if ix == 2 { underlined_at(run, px(thickness), None) } else { run }
                });
                assert_close(painted.lines[0].0, painted.baseline(2) + gap, "underline");
            }
        }

        #[test]
        fn an_explicit_offset_ignores_the_thickness() {
            let painted = mixed(|ix, run| {
                if ix == 2 { underlined_at(run, px(6.), Some(px(2.))) } else { run }
            });
            assert_close(painted.lines[0].0, painted.baseline(2) + 2., "underline");
        }

        /// Chrome centres a strike-through two thirds of the way down from the top of the text, by
        /// the ascent of the font of the box that declared it: here the plain text of the line,
        /// whatever the text it is drawn over.
        #[test]
        fn a_strikethrough_on_big_text_goes_by_the_ascent_of_the_decorating_font() {
            let painted = mixed(|ix, run| if ix == 1 { struck(run) } else { run });
            assert_eq!(painted.lines.len(), 1);
            let big_ascent = f32::from(painted.run(5).ascent);
            let decorating_ascent = f32::from(painted.layout.ascent());
            assert_close(
                painted.lines[0].0,
                painted.baseline(1) - big_ascent + decorating_ascent * 2. / 3. - 0.5,
                "strikethrough",
            );
        }

        #[test]
        fn a_strikethrough_on_plain_text_follows_the_baseline_of_a_taller_row() {
            let painted = mixed(|ix, run| if ix == 2 { struck(run) } else { run });
            assert_eq!(painted.lines.len(), 1);
            let ascent = f32::from(painted.layout.ascent());
            assert_close(
                painted.lines[0].0,
                painted.baseline(2) - ascent / 3. - 0.5,
                "strikethrough",
            );
        }

        /// Without runs of their own size every row is the line height, with the
        /// baseline, underline and strikethrough where they always were.
        #[test]
        fn rows_of_a_line_without_sized_runs_are_painted_as_before() {
            let text = "aaaaaaaa bbbbbbbb cccccccc";
            let layout_of = |runs: Vec<TextRun>| paint(text, runs);

            let painted = layout_of(vec![plain(26)]);
            let ascent = f32::from(painted.layout.ascent());
            let descent = f32::from(painted.layout.descent());
            let padding_top = (LINE_HEIGHT - ascent - descent) / 2.;
            let top = painted.row_tops[0];
            assert_eq!(painted.rows.len(), 3);
            let expected = painted.expected_glyphs(26, |index| {
                top + painted.row_of(index) as f32 * LINE_HEIGHT + padding_top + ascent
            });
            assert_all_close(&painted.glyphs, &expected, "glyph");

            let underlined = layout_of(vec![underlined(plain(26))]);
            assert_eq!(underlined.lines.len(), 3);
            for (row, line) in underlined.lines.iter().enumerate() {
                assert_close(
                    line.0,
                    top + row as f32 * LINE_HEIGHT + padding_top + ascent + 1.,
                    "underline",
                );
            }

            let struck = layout_of(vec![struck(plain(26))]);
            assert_eq!(struck.lines.len(), 3);
            for (row, line) in struck.lines.iter().enumerate() {
                assert_close(
                    line.0,
                    top + row as f32 * LINE_HEIGHT + padding_top + ascent * 2. / 3. - 0.5,
                    "strikethrough",
                );
            }

            let filled = layout_of(vec![filled(plain(26))]);
            assert_eq!(filled.quads.len(), 3);
            for (row, quad) in filled.quads.iter().enumerate() {
                assert_close(quad.0, top + row as f32 * LINE_HEIGHT, "quad top");
                assert_close(quad.1, LINE_HEIGHT, "quad height");
            }
        }
    }
}
