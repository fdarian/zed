use crate::{FontId, GlyphId, Pixels, PlatformTextSystem, Point, SharedString, Size, point, px};
use collections::FxHashMap;
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::{
    borrow::Borrow,
    cell::RefCell,
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

use super::LineWrapper;
use gpui_util::ResultExt as _;

/// How far apart the tab stops are, as in CSS `tab-size`. Tab stops are measured
/// from the start of each wrapped line.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum TabSize {
    /// This many times the advance of a space in the font of the tab.
    Spaces(f32),
    /// A fixed distance.
    Length(Pixels),
}

// A `TabSize` is never NaN, so the bitwise comparison agrees with `PartialEq`
// everywhere except on `-0.0`, which no one writes.
impl Eq for TabSize {}

impl Hash for TabSize {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Spaces(spaces) => {
                0u8.hash(state);
                spaces.to_bits().hash(state);
            }
            Self::Length(length) => {
                1u8.hash(state);
                length.hash(state);
            }
        }
    }
}

/// The distance between the tab stops, for the font a tab is shaped in. `None`
/// when it cannot be known, and the tab then keeps the advance it was shaped with.
type TabStopFor<'a> = &'a dyn Fn(FontId) -> Option<Pixels>;

/// Closer than this to a tab stop counts as being on it, so float error in a
/// glyph position does not make a tab one pixel wide.
const TAB_STOP_TOLERANCE: f32 = 0.01;

/// How far a tab at `position` advances, to the next tab stop after it.
fn advance_to_tab_stop(position: Pixels, stop: Pixels) -> Pixels {
    let stop = f32::from(stop);
    if stop <= 0. {
        return px(0.);
    }
    let advance = stop - f32::from(position).rem_euclid(stop);
    px(if advance < TAB_STOP_TOLERANCE {
        advance + stop
    } else {
        advance
    })
}

/// A laid out and styled line of text
#[derive(Default, Debug)]
pub struct LineLayout {
    /// The font size for this line
    pub font_size: Pixels,
    /// The width of the line
    pub width: Pixels,
    /// The ascent of the line
    pub ascent: Pixels,
    /// The descent of the line
    pub descent: Pixels,
    /// The shaped runs that make up this line
    pub runs: Vec<ShapedRun>,
    /// The length of the line in utf-8 bytes
    pub len: usize,
}

/// A run of text that has been shaped .
#[derive(Debug, Clone)]
pub struct ShapedRun {
    /// The font id for this run
    pub font_id: FontId,
    /// The glyphs that make up this run
    pub glyphs: Vec<ShapedGlyph>,
}

/// A single glyph, ready to paint.
#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    /// The ID for this glyph, as determined by the text system.
    pub id: GlyphId,

    /// The position of this glyph in its containing line.
    pub position: Point<Pixels>,

    /// The index of this glyph in the original text.
    pub index: usize,

    /// Whether this glyph is an emoji
    pub is_emoji: bool,
}

impl LineLayout {
    /// The index for the character at the given x coordinate
    pub fn index_for_x(&self, x: Pixels) -> Option<usize> {
        if x >= self.width {
            None
        } else {
            for run in self.runs.iter().rev() {
                for glyph in run.glyphs.iter().rev() {
                    if glyph.position.x <= x {
                        return Some(glyph.index);
                    }
                }
            }
            Some(0)
        }
    }

    /// closest_index_for_x returns the character boundary closest to the given x coordinate
    /// (e.g. to handle aligning up/down arrow keys)
    pub fn closest_index_for_x(&self, x: Pixels) -> usize {
        let mut prev_index = 0;
        let mut prev_x = px(0.);

        for run in self.runs.iter() {
            for glyph in run.glyphs.iter() {
                if glyph.position.x >= x {
                    if glyph.position.x - x < x - prev_x {
                        return glyph.index;
                    } else {
                        return prev_index;
                    }
                }
                prev_index = glyph.index;
                prev_x = glyph.position.x;
            }
        }

        if self.len == 1 {
            if x > self.width / 2. {
                return 1;
            } else {
                return 0;
            }
        }

        self.len
    }

    /// The x position of the character at the given index
    pub fn x_for_index(&self, index: usize) -> Pixels {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return glyph.position.x;
                }
            }
        }
        self.width
    }

    /// The corresponding Font at the given index
    pub fn font_id_for_index(&self, index: usize) -> Option<FontId> {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return Some(run.font_id);
                }
            }
        }
        None
    }

    /// Split this layout at a byte index, returning `(prefix, suffix)`.
    ///
    /// - `prefix` contains glyphs for bytes `[0, byte_index)` with original positions.
    ///   Its width equals the x-advance up to the split point.
    /// - `suffix` contains glyphs for bytes `[byte_index, len)` with positions
    ///   shifted left so the first glyph starts at x=0, and byte indices rebased to 0.
    /// - `font_size`, `ascent`, and `descent` are copied to both halves.
    pub fn split_at(&self, byte_index: usize) -> (LineLayout, LineLayout) {
        let x_offset = self.x_for_index(byte_index);

        // Partition glyph runs. A single run may contribute glyphs to both halves.
        let mut left_runs = Vec::new();
        let mut right_runs = Vec::new();

        for run in &self.runs {
            let split_pos = run.glyphs.partition_point(|g| g.index < byte_index);

            if split_pos > 0 {
                left_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: run.glyphs[..split_pos].to_vec(),
                });
            }

            if split_pos < run.glyphs.len() {
                let right_glyphs = run.glyphs[split_pos..]
                    .iter()
                    .map(|g| ShapedGlyph {
                        id: g.id,
                        position: point(g.position.x - x_offset, g.position.y),
                        index: g.index - byte_index,
                        is_emoji: g.is_emoji,
                    })
                    .collect();
                right_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: right_glyphs,
                });
            }
        }

        let left = LineLayout {
            font_size: self.font_size,
            width: x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: left_runs,
            len: byte_index,
        };

        let right = LineLayout {
            font_size: self.font_size,
            width: self.width - x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: right_runs,
            len: self.len - byte_index,
        };

        (left, right)
    }

    /// With a `tab_stop`, also returns the x of every glyph, and then of the end
    /// of the line, after each tab character has advanced to its tab stop. The
    /// stops are measured from the start of the wrapped line the tab is on, so
    /// where the lines break decides where the tabs end.
    fn compute_wrap_boundaries(
        &self,
        text: &str,
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        tab_stop: Option<TabStopFor<'_>>,
    ) -> (SmallVec<[WrapBoundary; 1]>, Option<Vec<Pixels>>) {
        let mut boundaries = SmallVec::new();
        let mut first_non_whitespace_ix = None;
        let mut last_candidate_ix = None;
        let mut last_boundary = WrapBoundary {
            run_ix: 0,
            glyph_ix: 0,
        };
        let mut last_boundary_x = px(0.);
        let mut prev_ch = '\0';
        let mut positions = GlyphPositions::new(self, text, tab_stop);
        let mut placed = 0;

        for ix in 0..positions.glyphs.len() {
            positions.place(ix + 1, last_boundary_x);
            placed = ix + 1;
            let boundary = positions.glyphs[ix].boundary;
            let ch = positions.glyphs[ix].wrap_char;
            if ch == '\n' {
                continue;
            }

            // Here is very similar to `LineWrapper::wrap_line` to determine text wrapping,
            // but there are some differences, so we have to duplicate the code here.
            if LineWrapper::is_word_char(ch) {
                if prev_ch == ' ' && ch != ' ' && first_non_whitespace_ix.is_some() {
                    last_candidate_ix = Some(ix);
                }
            } else if ch != ' ' && first_non_whitespace_ix.is_some() {
                last_candidate_ix = Some(ix);
            }

            if ch != ' ' && first_non_whitespace_ix.is_none() {
                first_non_whitespace_ix = Some(boundary);
            }

            let width = positions.x(ix + 1) - last_boundary_x;

            if wrap_width.is_some_and(|wrap_width| width > wrap_width) && boundary > last_boundary {
                // When used line_clamp, we should limit the number of lines.
                if let Some(max_lines) = max_lines
                    && boundaries.len() >= max_lines.saturating_sub(1)
                {
                    break;
                }

                let line_start_ix = last_candidate_ix.take().unwrap_or(ix);
                last_boundary = positions.glyphs[line_start_ix].boundary;
                last_boundary_x = positions.x(line_start_ix);
                boundaries.push(last_boundary);
                // The tab stops of what moved to the new line count from its start.
                for moved_ix in line_start_ix + 1..=ix + 1 {
                    positions.place(moved_ix, last_boundary_x);
                }
            }
            prev_ch = ch;
        }

        for ix in placed + 1..=positions.glyphs.len() {
            positions.place(ix, last_boundary_x);
        }

        (boundaries, positions.into_tabbed_x())
    }

    /// This layout with the glyphs at `x` and the line `x`'s last entry wide, as
    /// computed by [`Self::compute_wrap_boundaries`].
    fn with_x_positions(&self, x: &[Pixels]) -> LineLayout {
        let mut x_iter = x.iter();
        let runs = self
            .runs
            .iter()
            .map(|run| ShapedRun {
                font_id: run.font_id,
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|glyph| ShapedGlyph {
                        position: point(*x_iter.next().expect("one x per glyph"), glyph.position.y),
                        ..glyph.clone()
                    })
                    .collect(),
            })
            .collect();
        LineLayout {
            font_size: self.font_size,
            width: *x.last().expect("the end of the line has an x"),
            ascent: self.ascent,
            descent: self.descent,
            runs,
            len: self.len,
        }
    }
}

struct WrapGlyph {
    boundary: WrapBoundary,
    /// The character at the glyph's index, as the wrapping rules see it.
    wrap_char: char,
    is_tab: bool,
    font_id: FontId,
    shaped_x: Pixels,
}

/// The x of each glyph of a line and then of its end. They are the shaped
/// ones, unless the line has tab stops.
struct GlyphPositions<'a> {
    glyphs: Vec<WrapGlyph>,
    shaped_width: Pixels,
    tabbed: Option<TabbedPositions<'a>>,
}

struct TabbedPositions<'a> {
    tab_stop: TabStopFor<'a>,
    /// Final x per glyph, then the end of the line.
    x: Vec<Pixels>,
    /// What the tabs before each glyph added to its shaped x.
    shift: Vec<Pixels>,
}

impl<'a> GlyphPositions<'a> {
    fn new(layout: &LineLayout, text: &str, tab_stop: Option<TabStopFor<'a>>) -> Self {
        let glyphs: Vec<WrapGlyph> = layout
            .runs
            .iter()
            .enumerate()
            .flat_map(|(run_ix, run)| {
                run.glyphs.iter().enumerate().map(move |(glyph_ix, glyph)| {
                    let character = text[glyph.index..].chars().next().unwrap();
                    WrapGlyph {
                        boundary: WrapBoundary { run_ix, glyph_ix },
                        // A tab is white space that may break, like the space it is
                        // when the text is not laid out with tab stops.
                        wrap_char: if tab_stop.is_some() && character == '\t' {
                            ' '
                        } else {
                            character
                        },
                        is_tab: character == '\t',
                        font_id: run.font_id,
                        shaped_x: glyph.position.x,
                    }
                })
            })
            .collect();
        let tabbed = tab_stop.map(|tab_stop| {
            let mut x: Vec<Pixels> = glyphs.iter().map(|glyph| glyph.shaped_x).collect();
            x.push(layout.width);
            TabbedPositions {
                tab_stop,
                shift: vec![px(0.); x.len()],
                x,
            }
        });
        Self {
            glyphs,
            shaped_width: layout.width,
            tabbed,
        }
    }

    fn shaped_x(&self, ix: usize) -> Pixels {
        self.glyphs
            .get(ix)
            .map_or(self.shaped_width, |glyph| glyph.shaped_x)
    }

    fn x(&self, ix: usize) -> Pixels {
        match &self.tabbed {
            Some(tabbed) => tabbed.x[ix],
            None => self.shaped_x(ix),
        }
    }

    /// Positions glyph `ix`, or the end of the line when it is past the last
    /// glyph, on a line that starts at `line_start_x`. The glyph before it must
    /// be placed already.
    fn place(&mut self, ix: usize, line_start_x: Pixels) {
        let Some(tabbed) = self.tabbed.as_mut() else {
            return;
        };
        if ix == 0 {
            return;
        }
        let previous = &self.glyphs[ix - 1];
        let shaped_x = self
            .glyphs
            .get(ix)
            .map_or(self.shaped_width, |glyph| glyph.shaped_x);
        let stop = previous
            .is_tab
            .then(|| (tabbed.tab_stop)(previous.font_id))
            .flatten();
        if let Some(stop) = stop {
            let previous_x = tabbed.x[ix - 1];
            tabbed.x[ix] = previous_x + advance_to_tab_stop(previous_x - line_start_x, stop);
            tabbed.shift[ix] = tabbed.x[ix] - shaped_x;
        } else {
            tabbed.shift[ix] = tabbed.shift[ix - 1];
            tabbed.x[ix] = shaped_x + tabbed.shift[ix];
        }
    }

    fn into_tabbed_x(self) -> Option<Vec<Pixels>> {
        self.tabbed.map(|tabbed| tabbed.x)
    }
}

/// A line of text that has been wrapped to fit a given width
#[derive(Default, Debug)]
pub struct WrappedLineLayout {
    /// The line layout, pre-wrapping.
    pub unwrapped_layout: Arc<LineLayout>,

    /// The boundaries at which the line was wrapped
    pub wrap_boundaries: SmallVec<[WrapBoundary; 1]>,

    /// The width of the line, if it was wrapped
    pub wrap_width: Option<Pixels>,
}

/// A boundary at which a line was wrapped
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WrapBoundary {
    /// The index in the run just before the line was wrapped
    pub run_ix: usize,
    /// The index of the glyph just before the line was wrapped
    pub glyph_ix: usize,
}

impl WrappedLineLayout {
    /// The length of the underlying text, in utf8 bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.unwrapped_layout.len
    }

    /// The width of this line, in pixels, whether or not it was wrapped.
    pub fn width(&self) -> Pixels {
        self.wrap_width
            .unwrap_or(Pixels::MAX)
            .min(self.unwrapped_layout.width)
    }

    /// The size of the whole wrapped text, for the given line_height.
    /// can span multiple lines if there are multiple wrap boundaries.
    pub fn size(&self, line_height: Pixels) -> Size<Pixels> {
        Size {
            width: self.width(),
            height: line_height * (self.wrap_boundaries.len() + 1),
        }
    }

    /// The ascent of a line in this layout
    pub fn ascent(&self) -> Pixels {
        self.unwrapped_layout.ascent
    }

    /// The descent of a line in this layout
    pub fn descent(&self) -> Pixels {
        self.unwrapped_layout.descent
    }

    /// The wrap boundaries in this layout
    pub fn wrap_boundaries(&self) -> &[WrapBoundary] {
        &self.wrap_boundaries
    }

    /// The font size of this layout
    pub fn font_size(&self) -> Pixels {
        self.unwrapped_layout.font_size
    }

    /// The runs in this layout, sans wrapping
    pub fn runs(&self) -> &[ShapedRun] {
        &self.unwrapped_layout.runs
    }

    /// The index corresponding to a given position in this layout for the given line height.
    ///
    /// See also [`Self::closest_index_for_position`].
    pub fn index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, false)
    }

    /// The closest index to a given position in this layout for the given line height.
    ///
    /// Closest means the character boundary closest to the given position.
    ///
    /// See also [`LineLayout::closest_index_for_x`].
    pub fn closest_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, true)
    }

    fn _index_for_position(
        &self,
        mut position: Point<Pixels>,
        line_height: Pixels,
        closest: bool,
    ) -> Result<usize, usize> {
        let wrapped_line_ix = (position.y / line_height) as usize;

        let wrapped_line_start_index;
        let wrapped_line_start_x;
        if wrapped_line_ix > 0 {
            let Some(line_start_boundary) = self.wrap_boundaries.get(wrapped_line_ix - 1) else {
                return Err(0);
            };
            let run = &self.unwrapped_layout.runs[line_start_boundary.run_ix];
            let glyph = &run.glyphs[line_start_boundary.glyph_ix];
            wrapped_line_start_index = glyph.index;
            wrapped_line_start_x = glyph.position.x;
        } else {
            wrapped_line_start_index = 0;
            wrapped_line_start_x = Pixels::ZERO;
        };

        let wrapped_line_end_index;
        let wrapped_line_end_x;
        if wrapped_line_ix < self.wrap_boundaries.len() {
            let next_wrap_boundary_ix = wrapped_line_ix;
            let next_wrap_boundary = self.wrap_boundaries[next_wrap_boundary_ix];
            let run = &self.unwrapped_layout.runs[next_wrap_boundary.run_ix];
            let glyph = &run.glyphs[next_wrap_boundary.glyph_ix];
            wrapped_line_end_index = glyph.index;
            wrapped_line_end_x = glyph.position.x;
        } else {
            wrapped_line_end_index = self.unwrapped_layout.len;
            wrapped_line_end_x = self.unwrapped_layout.width;
        };

        let mut position_in_unwrapped_line = position;
        position_in_unwrapped_line.x += wrapped_line_start_x;
        if position_in_unwrapped_line.x < wrapped_line_start_x {
            Err(wrapped_line_start_index)
        } else if position_in_unwrapped_line.x >= wrapped_line_end_x {
            Err(wrapped_line_end_index)
        } else {
            if closest {
                Ok(self
                    .unwrapped_layout
                    .closest_index_for_x(position_in_unwrapped_line.x))
            } else {
                Ok(self
                    .unwrapped_layout
                    .index_for_x(position_in_unwrapped_line.x)
                    .unwrap())
            }
        }
    }

    /// Returns the pixel position for the given byte index.
    pub fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
        let mut line_start_ix = 0;
        let mut line_end_indices = self
            .wrap_boundaries
            .iter()
            .map(|wrap_boundary| {
                let run = &self.unwrapped_layout.runs[wrap_boundary.run_ix];
                let glyph = &run.glyphs[wrap_boundary.glyph_ix];
                glyph.index
            })
            .chain([self.len()])
            .enumerate();
        for (ix, line_end_ix) in line_end_indices {
            let line_y = ix as f32 * line_height;
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_start_ix = line_end_ix;
                continue;
            } else {
                let line_start_x = self.unwrapped_layout.x_for_index(line_start_ix);
                let x = self.unwrapped_layout.x_for_index(index) - line_start_x;
                return Some(point(x, line_y));
            }
        }

        None
    }
}

pub(crate) struct LineLayoutCache {
    previous_frame: Mutex<FrameCache>,
    current_frame: RwLock<FrameCache>,
    platform_text_system: Arc<dyn PlatformTextSystem>,
}

#[derive(Default)]
struct FrameCache {
    lines: FxHashMap<Arc<CacheKey>, Arc<LineLayout>>,
    wrapped_lines: FxHashMap<Arc<CacheKey>, Arc<WrappedLineLayout>>,
    used_lines: Vec<Arc<CacheKey>>,
    used_wrapped_lines: Vec<Arc<CacheKey>>,

    // Content-addressable caches keyed by caller-provided text hash + layout params.
    // These allow cache hits without materializing a contiguous `SharedString`.
    //
    // IMPORTANT: To support allocation-free lookups, we store these maps using a key type
    // (`HashedCacheKeyRef`) that can be computed without building a contiguous `&str`/`SharedString`.
    // On miss, we allocate once and store under an owned `HashedCacheKey`.
    lines_by_hash: FxHashMap<Arc<HashedCacheKey>, Arc<LineLayout>>,
    wrapped_lines_by_hash: FxHashMap<Arc<HashedCacheKey>, Arc<WrappedLineLayout>>,
    used_lines_by_hash: Vec<Arc<HashedCacheKey>>,
    used_wrapped_lines_by_hash: Vec<Arc<HashedCacheKey>>,
}

#[derive(Clone, Default)]
pub(crate) struct LineLayoutIndex {
    lines_index: usize,
    wrapped_lines_index: usize,
    lines_by_hash_index: usize,
    wrapped_lines_by_hash_index: usize,
}

impl LineLayoutCache {
    pub fn new(platform_text_system: Arc<dyn PlatformTextSystem>) -> Self {
        Self {
            previous_frame: Mutex::default(),
            current_frame: RwLock::default(),
            platform_text_system,
        }
    }

    pub fn layout_index(&self) -> LineLayoutIndex {
        let frame = self.current_frame.read();
        LineLayoutIndex {
            lines_index: frame.used_lines.len(),
            wrapped_lines_index: frame.used_wrapped_lines.len(),
            lines_by_hash_index: frame.used_lines_by_hash.len(),
            wrapped_lines_by_hash_index: frame.used_wrapped_lines_by_hash.len(),
        }
    }

    pub fn reuse_layouts(&self, range: Range<LineLayoutIndex>) {
        let mut previous_frame = &mut *self.previous_frame.lock();
        let mut current_frame = &mut *self.current_frame.write();

        for key in &previous_frame.used_lines[range.start.lines_index..range.end.lines_index] {
            if let Some((key, line)) = previous_frame.lines.remove_entry(key) {
                current_frame.lines.insert(key, line);
            }
            current_frame.used_lines.push(key.clone());
        }

        for key in &previous_frame.used_wrapped_lines
            [range.start.wrapped_lines_index..range.end.wrapped_lines_index]
        {
            if let Some((key, line)) = previous_frame.wrapped_lines.remove_entry(key) {
                current_frame.wrapped_lines.insert(key, line);
            }
            current_frame.used_wrapped_lines.push(key.clone());
        }

        for key in &previous_frame.used_lines_by_hash
            [range.start.lines_by_hash_index..range.end.lines_by_hash_index]
        {
            if let Some((key, line)) = previous_frame.lines_by_hash.remove_entry(key) {
                current_frame.lines_by_hash.insert(key, line);
            }
            current_frame.used_lines_by_hash.push(key.clone());
        }

        for key in &previous_frame.used_wrapped_lines_by_hash
            [range.start.wrapped_lines_by_hash_index..range.end.wrapped_lines_by_hash_index]
        {
            if let Some((key, line)) = previous_frame.wrapped_lines_by_hash.remove_entry(key) {
                current_frame.wrapped_lines_by_hash.insert(key, line);
            }
            current_frame.used_wrapped_lines_by_hash.push(key.clone());
        }
    }

    pub fn truncate_layouts(&self, index: LineLayoutIndex) {
        let mut current_frame = &mut *self.current_frame.write();
        current_frame.used_lines.truncate(index.lines_index);
        current_frame
            .used_wrapped_lines
            .truncate(index.wrapped_lines_index);
        current_frame
            .used_lines_by_hash
            .truncate(index.lines_by_hash_index);
        current_frame
            .used_wrapped_lines_by_hash
            .truncate(index.wrapped_lines_by_hash_index);
    }

    pub fn finish_frame(&self) {
        let mut prev_frame = self.previous_frame.lock();
        let mut curr_frame = self.current_frame.write();
        std::mem::swap(&mut *prev_frame, &mut *curr_frame);
        curr_frame.lines.clear();
        curr_frame.wrapped_lines.clear();
        curr_frame.used_lines.clear();
        curr_frame.used_wrapped_lines.clear();

        curr_frame.lines_by_hash.clear();
        curr_frame.wrapped_lines_by_hash.clear();
        curr_frame.used_lines_by_hash.clear();
        curr_frame.used_wrapped_lines_by_hash.clear();
    }

    pub fn layout_wrapped_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        tab_size: Option<TabSize>,
    ) -> Arc<WrappedLineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        // Text without a tab is laid out the same whatever its tab size, so it
        // must not take a cache entry of its own.
        let tab_size = tab_size.filter(|_| text.as_ref().contains('\t'));
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            wrap_width,
            force_width: None,
            tab_size,
        } as &dyn AsCacheKeyRef;

        let current_frame = self.current_frame.upgradable_read();
        if let Some(layout) = current_frame.wrapped_lines.get(key) {
            return layout.clone();
        }

        let previous_frame_entry = self.previous_frame.lock().wrapped_lines.remove_entry(key);
        if let Some((key, layout)) = previous_frame_entry {
            let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
            current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            current_frame.used_wrapped_lines.push(key);
            layout
        } else {
            drop(current_frame);
            let text = SharedString::from(text);
            let shaped_layout = self.layout_line::<&SharedString>(&text, font_size, runs, None);
            let (wrap_boundaries, unwrapped_layout) = if let Some(tab_size) = tab_size {
                let tab_stop = self.tab_stop_resolver(tab_size, font_size);
                let (wrap_boundaries, x) = shaped_layout.compute_wrap_boundaries(
                    text.as_ref(),
                    wrap_width,
                    max_lines,
                    Some(&tab_stop),
                );
                let x = x.expect("tab stops were asked for");
                (
                    wrap_boundaries,
                    Arc::new(shaped_layout.with_x_positions(&x)),
                )
            } else if let Some(wrap_width) = wrap_width {
                let (wrap_boundaries, _) = shaped_layout.compute_wrap_boundaries(
                    text.as_ref(),
                    Some(wrap_width),
                    max_lines,
                    None,
                );
                (wrap_boundaries, shaped_layout)
            } else {
                (SmallVec::new(), shaped_layout)
            };
            let layout = Arc::new(WrappedLineLayout {
                unwrapped_layout,
                wrap_boundaries,
                wrap_width,
            });
            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                wrap_width,
                force_width: None,
                tab_size,
            });

            let mut current_frame = self.current_frame.write();
            current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            current_frame.used_wrapped_lines.push(key);

            layout
        }
    }

    /// The distance between tab stops for each font a tab is shaped in. A
    /// space is as wide as its font says, not as the line around it.
    fn tab_stop_resolver(
        &self,
        tab_size: TabSize,
        font_size: Pixels,
    ) -> impl Fn(FontId) -> Option<Pixels> + '_ {
        let advances = RefCell::new(SmallVec::<[(FontId, Option<Pixels>); 2]>::new());
        move |font_id| match tab_size {
            TabSize::Length(length) => Some(length),
            TabSize::Spaces(spaces) => {
                let mut advances = advances.borrow_mut();
                let advance = match advances.iter().find(|(id, _)| *id == font_id) {
                    Some((_, advance)) => *advance,
                    None => {
                        let advance = self.space_advance(font_id, font_size);
                        advances.push((font_id, advance));
                        advance
                    }
                };
                advance.map(|advance| advance * spaces)
            }
        }
    }

    fn space_advance(&self, font_id: FontId, font_size: Pixels) -> Option<Pixels> {
        let glyph_id = self.platform_text_system.glyph_for_char(font_id, ' ');
        let advance = glyph_id.and_then(|glyph_id| {
            self.platform_text_system
                .advance(font_id, glyph_id)
                .log_err()
        });
        let Some(advance) = advance else {
            log::error!("font {font_id:?} has no space to measure tab stops with");
            return None;
        };
        let units_per_em = self.platform_text_system.font_metrics(font_id).units_per_em;
        Some(px(
            advance.width / units_per_em as f32 * f32::from(font_size)
        ))
    }

    pub fn layout_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
    ) -> Arc<LineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            wrap_width: None,
            force_width,
            tab_size: None,
        } as &dyn AsCacheKeyRef;

        let current_frame = self.current_frame.upgradable_read();
        if let Some(layout) = current_frame.lines.get(key) {
            return layout.clone();
        }

        let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
        if let Some((key, layout)) = self.previous_frame.lock().lines.remove_entry(key) {
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        } else {
            let text = SharedString::from(text);
            let mut layout = self
                .platform_text_system
                .layout_line(&text, font_size, runs);

            if let Some(force_width) = force_width {
                apply_force_width_to_layout(&mut layout, force_width);
            }

            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                wrap_width: None,
                force_width,
                tab_size: None,
            });
            let layout = Arc::new(layout);
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        }
    }

    /// Try to retrieve a previously-shaped line layout using a caller-provided content hash.
    ///
    /// This is a *non-allocating* cache probe: it does not materialize any text. If the layout
    /// is not already cached in either the current frame or previous frame, returns `None`.
    ///
    /// Contract (caller enforced):
    /// - Same `text_hash` implies identical text content (collision risk accepted by caller).
    /// - `text_len` should be the UTF-8 byte length of the text (helps reduce accidental collisions).
    pub fn try_layout_line_by_hash(
        &self,
        text_hash: u64,
        text_len: usize,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
    ) -> Option<Arc<LineLayout>> {
        let key_ref = HashedCacheKeyRef {
            text_hash,
            text_len,
            font_size,
            runs,
            wrap_width: None,
            force_width,
        };

        let current_frame = self.current_frame.read();
        if let Some((_, layout)) = current_frame.lines_by_hash.iter().find(|(key, _)| {
            HashedCacheKeyRef {
                text_hash: key.text_hash,
                text_len: key.text_len,
                font_size: key.font_size,
                runs: key.runs.as_slice(),
                wrap_width: key.wrap_width,
                force_width: key.force_width,
            } == key_ref
        }) {
            return Some(layout.clone());
        }

        let previous_frame = self.previous_frame.lock();
        if let Some((_, layout)) = previous_frame.lines_by_hash.iter().find(|(key, _)| {
            HashedCacheKeyRef {
                text_hash: key.text_hash,
                text_len: key.text_len,
                font_size: key.font_size,
                runs: key.runs.as_slice(),
                wrap_width: key.wrap_width,
                force_width: key.force_width,
            } == key_ref
        }) {
            return Some(layout.clone());
        }

        None
    }

    /// Layout a line of text using a caller-provided content hash as the cache key.
    ///
    /// This enables cache hits without materializing a contiguous `SharedString` for `text`.
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
        runs: &[FontRun],
        force_width: Option<Pixels>,
        materialize_text: impl FnOnce() -> SharedString,
    ) -> Arc<LineLayout> {
        let key_ref = HashedCacheKeyRef {
            text_hash,
            text_len,
            font_size,
            runs,
            wrap_width: None,
            force_width,
        };

        // Fast path: already cached (no allocation).
        let current_frame = self.current_frame.upgradable_read();
        if let Some((_, layout)) = current_frame.lines_by_hash.iter().find(|(key, _)| {
            HashedCacheKeyRef {
                text_hash: key.text_hash,
                text_len: key.text_len,
                font_size: key.font_size,
                runs: key.runs.as_slice(),
                wrap_width: key.wrap_width,
                force_width: key.force_width,
            } == key_ref
        }) {
            return layout.clone();
        }

        let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);

        // Try to reuse from previous frame without allocating; do a linear scan to find a matching key.
        // (We avoid `drain()` here because it would eagerly move all entries.)
        let mut previous_frame = self.previous_frame.lock();
        if let Some(existing_key) = previous_frame
            .used_lines_by_hash
            .iter()
            .find(|key| {
                HashedCacheKeyRef {
                    text_hash: key.text_hash,
                    text_len: key.text_len,
                    font_size: key.font_size,
                    runs: key.runs.as_slice(),
                    wrap_width: key.wrap_width,
                    force_width: key.force_width,
                } == key_ref
            })
            .cloned()
        {
            if let Some((key, layout)) = previous_frame.lines_by_hash.remove_entry(&existing_key) {
                current_frame
                    .lines_by_hash
                    .insert(key.clone(), layout.clone());
                current_frame.used_lines_by_hash.push(key);
                return layout;
            }
        }

        let text = materialize_text();
        let mut layout = self
            .platform_text_system
            .layout_line(&text, font_size, runs);

        if let Some(force_width) = force_width {
            apply_force_width_to_layout(&mut layout, force_width);
        }

        let key = Arc::new(HashedCacheKey {
            text_hash,
            text_len,
            font_size,
            runs: SmallVec::from(runs),
            wrap_width: None,
            force_width,
        });
        let layout = Arc::new(layout);
        current_frame
            .lines_by_hash
            .insert(key.clone(), layout.clone());
        current_frame.used_lines_by_hash.push(key);
        layout
    }
}

// Combining marks (e.g. Thai vowel signs, Arabic diacritics) are shaped by
// HarfBuzz at the same x position as their base character. The force-width
// loop must not advance the cell counter for these zero-advance glyphs,
// otherwise they get displaced into the next cell. We detect them by checking
// whether shaped x has advanced by at least half a cell beyond the last base.
fn apply_force_width_to_layout(layout: &mut LineLayout, force_width: Pixels) {
    let mut glyph_pos: usize = 0;
    // NEG_INFINITY ensures the first glyph is always classified as a base.
    let mut last_base_shaped_x = px(f32::NEG_INFINITY);
    let mut last_base_actual_x = px(0.);

    for run in layout.runs.iter_mut() {
        for glyph in run.glyphs.iter_mut() {
            let shaped_x = glyph.position.x;

            if shaped_x > last_base_shaped_x + force_width * 0.5 {
                let forced_x = glyph_pos * force_width;
                if (shaped_x - forced_x).abs() > px(1.) {
                    glyph.position.x = forced_x;
                }
                last_base_shaped_x = shaped_x;
                last_base_actual_x = glyph.position.x;
                glyph_pos += 1;
            } else {
                glyph.position.x = last_base_actual_x + (shaped_x - last_base_shaped_x);
            }
        }
    }
}

/// A run of text with a single font.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[expect(missing_docs)]
pub struct FontRun {
    pub len: usize,
    pub font_id: FontId,
}

trait AsCacheKeyRef {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_>;
}

#[derive(Clone, Debug, Eq)]
struct CacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
    tab_size: Option<TabSize>,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct CacheKeyRef<'a> {
    text: &'a str,
    font_size: Pixels,
    runs: &'a [FontRun],
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
    tab_size: Option<TabSize>,
}

#[derive(Clone, Debug)]
struct HashedCacheKey {
    text_hash: u64,
    text_len: usize,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
}

#[derive(Copy, Clone)]
struct HashedCacheKeyRef<'a> {
    text_hash: u64,
    text_len: usize,
    font_size: Pixels,
    runs: &'a [FontRun],
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
}

impl PartialEq for dyn AsCacheKeyRef + '_ {
    fn eq(&self, other: &dyn AsCacheKeyRef) -> bool {
        self.as_cache_key_ref() == other.as_cache_key_ref()
    }
}

impl PartialEq for HashedCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.text_hash == other.text_hash
            && self.text_len == other.text_len
            && self.font_size == other.font_size
            && self.runs.as_slice() == other.runs.as_slice()
            && self.wrap_width == other.wrap_width
            && self.force_width == other.force_width
    }
}

impl Eq for HashedCacheKey {}

impl Hash for HashedCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text_hash.hash(state);
        self.text_len.hash(state);
        self.font_size.hash(state);
        self.runs.as_slice().hash(state);
        self.wrap_width.hash(state);
        self.force_width.hash(state);
    }
}

impl PartialEq for HashedCacheKeyRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.text_hash == other.text_hash
            && self.text_len == other.text_len
            && self.font_size == other.font_size
            && self.runs == other.runs
            && self.wrap_width == other.wrap_width
            && self.force_width == other.force_width
    }
}

impl Eq for HashedCacheKeyRef<'_> {}

impl Hash for HashedCacheKeyRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text_hash.hash(state);
        self.text_len.hash(state);
        self.font_size.hash(state);
        self.runs.hash(state);
        self.wrap_width.hash(state);
        self.force_width.hash(state);
    }
}

impl Eq for dyn AsCacheKeyRef + '_ {}

impl Hash for dyn AsCacheKeyRef + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state)
    }
}

impl AsCacheKeyRef for CacheKey {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        CacheKeyRef {
            text: &self.text,
            font_size: self.font_size,
            runs: self.runs.as_slice(),
            wrap_width: self.wrap_width,
            force_width: self.force_width,
            tab_size: self.tab_size,
        }
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.as_cache_key_ref().eq(&other.as_cache_key_ref())
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state);
    }
}

impl<'a> Borrow<dyn AsCacheKeyRef + 'a> for Arc<CacheKey> {
    fn borrow(&self) -> &(dyn AsCacheKeyRef + 'a) {
        self.as_ref() as &dyn AsCacheKeyRef
    }
}

impl AsCacheKeyRef for CacheKeyRef<'_> {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GlyphId;

    fn glyph_at(x: f32, index: usize) -> ShapedGlyph {
        ShapedGlyph {
            id: GlyphId(0),
            position: point(px(x), px(0.)),
            index,
            is_emoji: false,
        }
    }

    fn make_layout(glyphs: Vec<ShapedGlyph>) -> LineLayout {
        LineLayout {
            font_size: px(16.),
            width: px(100.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: 0,
        }
    }

    fn glyph_x_positions(layout: &LineLayout) -> Vec<f32> {
        layout.runs[0]
            .glyphs
            .iter()
            .map(|g| f32::from(g.position.x))
            .collect()
    }

    #[test]
    fn test_force_width_latin_unchanged() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(8., 1), glyph_at(16., 2)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_marks_not_advanced() {
        let cell_width = px(8.);
        // Simulates Thai "กี" — base consonant at x=0, combining vowel also at x=0
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // ี (combining mark, same x)
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0.]);
    }

    #[test]
    fn test_force_width_base_after_combining_mark() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(0., 3), glyph_at(8., 6)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 8.]);
    }

    #[test]
    fn test_force_width_multiple_combining_marks() {
        let cell_width = px(8.);
        // Simulates "ก้" — base + vowel + tone mark (two combining marks stacked)
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // vowel (combining)
            glyph_at(0., 6), // tone mark (combining)
            glyph_at(8., 9), // next base
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 0., 8.]);
    }

    #[test]
    fn test_force_width_corrects_drifted_base_positions() {
        let cell_width = px(8.);
        // Font metrics don't perfectly match cell grid — glyphs drift >1px from cell boundary
        let mut layout = make_layout(vec![
            glyph_at(0.5, 0),  // within 1px tolerance, kept as-is
            glyph_at(10.2, 1), // >1px off from 8.0, corrected
            glyph_at(19.8, 2), // >1px off from 16.0, corrected
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_mark_after_within_tolerance_base() {
        let cell_width = px(8.);
        // Base glyph is within 1px of grid so it keeps its shaped position.
        // The combining mark must align to the base's actual position, not the grid slot.
        let mut layout = make_layout(vec![glyph_at(0.5, 0), glyph_at(0.5, 3)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 0.5]);
    }

    /// One glyph per character, `advance` apart, in one run.
    fn monospace_layout(text: &str, advance: f32) -> LineLayout {
        let glyphs = text
            .char_indices()
            .enumerate()
            .map(|(column, (index, _))| glyph_at(column as f32 * advance, index))
            .collect();
        LineLayout {
            width: px(text.chars().count() as f32 * advance),
            len: text.len(),
            ..make_layout(glyphs)
        }
    }

    /// `text` laid out as ten pixel wide characters, with a tab stop every `stop`
    /// pixels.
    fn tabbed_line(text: &str, wrap_width: Option<f32>, stop: f32) -> WrappedLineLayout {
        let shaped = monospace_layout(text, 10.);
        let tab_stop = |_: FontId| Some(px(stop));
        let (wrap_boundaries, x) =
            shaped.compute_wrap_boundaries(text, wrap_width.map(px), None, Some(&tab_stop));
        WrappedLineLayout {
            unwrapped_layout: Arc::new(shaped.with_x_positions(&x.unwrap())),
            wrap_boundaries,
            wrap_width: wrap_width.map(px),
        }
    }

    /// Where each character starts, within its wrapped line, and which line.
    fn columns(layout: &WrappedLineLayout, text: &str) -> Vec<(f32, usize)> {
        let line_height = px(20.);
        text.char_indices()
            .map(|(index, _)| {
                let position = layout.position_for_index(index, line_height).unwrap();
                (f32::from(position.x), (position.y / line_height) as usize)
            })
            .collect()
    }

    fn boundary_indices(layout: &WrappedLineLayout) -> Vec<usize> {
        layout
            .wrap_boundaries
            .iter()
            .map(|boundary| {
                layout.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
            })
            .collect()
    }

    #[test]
    fn test_tab_advances_to_the_next_stop_from_the_line_start() {
        let text = "ab\tc\tdefghijk\tl";
        let layout = tabbed_line(text, None, 40.);
        let xs: Vec<f32> = columns(&layout, text)
            .iter()
            .map(|column| column.0)
            .collect();
        // a b [tab to 40] c [tab to 80] d..k (80..160) [tab to 200] l
        assert_eq!(
            xs,
            vec![
                0., 10., 20., 40., 50., 80., 90., 100., 110., 120., 130., 140., 150., 160., 200.
            ]
        );
        assert_eq!(layout.unwrapped_layout.width, px(210.));
    }

    #[test]
    fn test_a_tab_on_a_stop_goes_to_the_next_one() {
        let text = "abcd\te";
        let layout = tabbed_line(text, None, 40.);
        assert_eq!(layout.position_for_index(5, px(20.)).unwrap().x, px(80.));
    }

    #[test]
    fn test_two_tabs_make_two_stops() {
        let text = "a\t\tb";
        let layout = tabbed_line(text, None, 40.);
        let xs: Vec<f32> = columns(&layout, text)
            .iter()
            .map(|column| column.0)
            .collect();
        assert_eq!(xs, vec![0., 10., 40., 80.]);
    }

    #[test]
    fn test_a_line_ending_in_a_tab_is_as_wide_as_the_tab() {
        assert_eq!(tabbed_line("ab\t", None, 40.).width(), px(40.));
        assert_eq!(tabbed_line("abcd\t", None, 40.).width(), px(80.));
        assert_eq!(tabbed_line("\t", None, 40.).width(), px(40.));
    }

    #[test]
    fn test_tab_stops_count_from_the_start_of_each_wrapped_line() {
        // "bb" does not fit after "aaaa " in 65px, so the second line is "bb\tc".
        // Its tab ends at 40px from the line start. Counted from the start of the
        // unwrapped line it would end at 80px, which is 30px on the second line.
        let text = "aaaa bb\tc";
        let layout = tabbed_line(text, Some(65.), 40.);
        assert_eq!(boundary_indices(&layout), vec![5]);
        let columns = columns(&layout, text);
        // The index of a boundary is the end of the line above.
        assert_eq!(columns[5], (50., 0));
        assert_eq!(columns[7], (20., 1));
        assert_eq!(columns[8], (40., 1));
        assert_eq!(layout.unwrapped_layout.width, px(100.));
    }

    #[test]
    fn test_a_tab_that_pushes_a_word_past_the_edge_moves_it_down() {
        // Without tab stops this is 8 characters, 80px, which fits in 90px.
        let text = "aa bb\tcc";
        let layout = tabbed_line(text, Some(90.), 80.);
        assert_eq!(boundary_indices(&layout), vec![6]);
        assert_eq!(layout.size(px(20.)).height, px(40.));
    }

    #[test]
    fn test_indices_and_positions_agree_across_a_tab() {
        let text = "ab\tcd";
        let line_height = px(20.);
        let layout = tabbed_line(text, None, 40.);
        for (index, _) in text.char_indices() {
            let position = layout.position_for_index(index, line_height).unwrap();
            let inside = point(position.x + px(1.), position.y);
            assert_eq!(layout.index_for_position(inside, line_height), Ok(index));
        }
        // The tab covers 20..40.
        for x in [20., 30., 39.] {
            assert_eq!(
                layout.index_for_position(point(px(x), px(0.)), line_height),
                Ok(2)
            );
        }
        assert_eq!(
            layout.closest_index_for_position(point(px(24.), px(0.)), line_height),
            Ok(2)
        );
        assert_eq!(
            layout.closest_index_for_position(point(px(36.), px(0.)), line_height),
            Ok(3)
        );
        // Past the end is the end.
        assert_eq!(
            layout.index_for_position(point(px(500.), px(0.)), line_height),
            Err(5)
        );
        assert_eq!(
            layout.position_for_index(5, line_height).unwrap().x,
            px(60.)
        );
    }

    #[test]
    fn test_indices_and_positions_agree_on_a_wrapped_line_with_a_tab() {
        let text = "aaaa bb\tcd";
        let line_height = px(20.);
        let layout = tabbed_line(text, Some(65.), 40.);
        assert_eq!(boundary_indices(&layout), vec![5]);
        for (index, _) in text.char_indices() {
            // The index of a boundary is the end of the line above, and the
            // start of the line below is found by position.
            let position = if index == 5 {
                point(px(0.), line_height)
            } else {
                layout.position_for_index(index, line_height).unwrap()
            };
            let inside = point(position.x + px(1.), position.y + px(1.));
            assert_eq!(layout.index_for_position(inside, line_height), Ok(index));
        }
        // The tab on the second line covers 20..40.
        assert_eq!(
            layout.index_for_position(point(px(30.), px(21.)), line_height),
            Ok(7)
        );
        assert_eq!(
            layout.index_for_position(point(px(500.), px(21.)), line_height),
            Err(text.len())
        );
        assert_eq!(
            layout.position_for_index(text.len(), line_height),
            Some(point(px(60.), line_height))
        );
    }

    #[test]
    fn test_text_without_a_tab_is_laid_out_as_shaped() {
        let text = "aaaa bb cc";
        let shaped = monospace_layout(text, 10.);
        let tab_stop = |_: FontId| Some(px(40.));
        let (with_stops, x) =
            shaped.compute_wrap_boundaries(text, Some(px(65.)), None, Some(&tab_stop));
        let (without_stops, none) = shaped.compute_wrap_boundaries(text, Some(px(65.)), None, None);
        assert_eq!(with_stops, without_stops);
        assert_eq!(none, None);
        let shaped_x: Vec<Pixels> = shaped.runs[0]
            .glyphs
            .iter()
            .map(|glyph| glyph.position.x)
            .collect();
        assert_eq!(&x.unwrap()[..text.len()], shaped_x.as_slice());
    }

    #[test]
    fn test_a_tab_stop_that_is_not_positive_makes_the_tab_empty() {
        let text = "a\tb";
        let layout = tabbed_line(text, None, 0.);
        assert_eq!(layout.position_for_index(2, px(20.)).unwrap().x, px(10.));
    }
}
