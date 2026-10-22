use crate::{FontId, GlyphId, Pixels, PlatformTextSystem, Point, SharedString, Size, point, px};
use collections::FxHashMap;
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use smallvec::{SmallVec, smallvec};
use std::{
    borrow::Borrow,
    cell::RefCell,
    hash::{Hash, Hasher},
    iter,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

mod shape_store;
mod snap;

use self::shape_store::{SHAPE_STORE_BUDGET_BYTES, ShapeStore};
use self::snap::{SnapMarks, glyph_positions};
use super::line_break::LineBreaks;
use super::{
    InlineRun, InlineSpec, PlacedBox, RunWrap, extent_in_line, LineMetrics, LineWrapper, RunPlacement, TextWrapStyle,
    apply_inline,
    resolve_placements,
};
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

/// Where a line may break between letters, as in CSS `word-break`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum WordBreak {
    /// A line breaks where Chrome's does: after spaces and hyphens, between CJK
    /// characters, and at the other places the Unicode line breaking algorithm
    /// allows, as `line_break.rs` has them.
    #[default]
    Normal,
    /// A line may also break between any two letters, so it is filled to the
    /// edge even when a space earlier on it would do.
    BreakAll,
    /// There is no break between two letters or numbers, CJK characters included,
    /// which then behave like the letters of a word: only spaces and punctuation
    /// remain.
    KeepAll,
}

/// Whether a word with no place to break on its line breaks anyway, as in CSS
/// `overflow-wrap`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum OverflowWrap {
    /// A word that does not fit overflows the line. This is CSS's initial value,
    /// but not this type's default.
    Normal,
    /// A word that does not fit and has no earlier place to break on its line
    /// breaks at the character that overflows. This is what text has always done
    /// here, so it is the default.
    #[default]
    BreakWord,
    /// Lays the text out as `BreakWord` does.
    Anywhere,
}

/// What a line of text is laid out with, beyond its fonts and its wrap width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextLayoutOptions {
    /// Where a tab character ends. `None` leaves the tab to the font, which gives
    /// it no meaning.
    pub tab_size: Option<TabSize>,
    /// Whether the spaces and tabs at the end of a line hang, as in CSS. A hanging
    /// space is not measured to see whether the line fits, so it never wraps and
    /// never starts a line, and it is not counted when the line is aligned. Where
    /// a line wraps it hangs past the wrap width.
    pub hang_trailing_spaces: bool,
    /// Whether a line may break after every space and tab, as with CSS
    /// `white-space: break-spaces`. A space that does not fit starts the next line,
    /// so a run of spaces breaks between any two of them, and a space at the end
    /// of a line is measured and aligned like any other character. This
    /// overrides `hang_trailing_spaces`.
    pub break_spaces: bool,
    /// Where a line may break between letters.
    pub word_break: WordBreak,
    /// Whether a word that does not fit breaks anyway.
    pub overflow_wrap: OverflowWrap,
    /// Which breaks a wrapped line is given among the ones that fit.
    pub text_wrap_style: TextWrapStyle,
    /// The font of the element the text is in, whose ascent and descent are the strut of
    /// every line: the content area every line has, whatever its runs are set in. `None`
    /// leaves the strut to the font of the first run.
    pub strut_font: Option<FontId>,
    /// That a line may not break in the text, CSS `text-wrap-mode: nowrap`. It is what
    /// the text outside every [`InlineBox`](crate::InlineBox) has, and what a box that
    /// says nothing about it inherits; a box can say otherwise. A text with no wrap
    /// width does not break whatever this is.
    pub nowrap: bool,
}

impl TextLayoutOptions {
    /// Whether the spaces at the end of a line hang. The one place that decides
    /// how `hang_trailing_spaces` and `break_spaces` combine.
    fn spaces_hang(&self) -> bool {
        self.hang_trailing_spaces && !self.break_spaces
    }

    /// These options as the min-content width of a text sees them: CSS gives
    /// `overflow-wrap: break-word` no say in it, because it breaks a word only
    /// once the word does not fit, and `anywhere` breaks between any two letters.
    pub(crate) fn for_min_content(self) -> Self {
        Self {
            overflow_wrap: match self.overflow_wrap {
                OverflowWrap::Anywhere => OverflowWrap::Anywhere,
                OverflowWrap::Normal | OverflowWrap::BreakWord => OverflowWrap::Normal,
            },
            // The narrowest a text goes is where every place that may break does.
            text_wrap_style: TextWrapStyle::Auto,
            ..self
        }
    }
}

/// Whether a line may break between `before` and `after`, two characters that
/// are not both part of the same word, under `word-break: break-all`. Spaces
/// are left to the other rules, and a break stays out of the places where UAX
/// #14 never has one: before closing punctuation or a combining mark, after
/// opening punctuation, and on either side of a non-breaking glue character.
fn may_break_between(before: char, after: char) -> bool {
    let glue = |c| matches!(c, '\u{00A0}' | '\u{202F}' | '\u{2011}');
    let closing =
        |c: char| "!?)]},.:;\"'”’»…".contains(c) || ('\u{0300}'..='\u{036F}').contains(&c);
    let opening = |c| matches!(c, '(' | '[' | '{' | '“' | '‘' | '«');
    before != ' ' && !glue(before) && !glue(after) && !closing(after) && !opening(before)
}

/// Extra inline advance around a byte range of a line of text, as the padding,
/// border and margin of a CSS inline box with `box-decoration-break: slice` add:
/// `leading` before the range's first glyph and `trailing` after its last. Later
/// glyphs are pushed right by both, the line is as much wider, and wrapping and
/// every x to index mapping count them. Where a range crosses a soft wrap the
/// insets stay at its two ends, and where it crosses a `\n` each paragraph gets
/// the end that falls in it.
///
/// A range that starts or ends inside a glyph that spans more than one character,
/// such as a ligature, belongs to the glyphs that start inside it. Text is shaped
/// in separate font runs at the ends of a range, which stops a ligature from
/// crossing them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextInset {
    /// The bytes of the text this inset surrounds.
    pub range: Range<usize>,
    /// The advance added before the first glyph of the range.
    pub leading: Pixels,
    /// The advance added after the last glyph of the range.
    pub trailing: Pixels,
}

/// The insets a single glyph of a laid out line carries, summed over every
/// [`TextInset`] that starts or ends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphInset {
    /// Which glyph of the line this is, counting through all its runs.
    pub glyph_ix: usize,
    /// The index of the glyph in the text.
    pub index: usize,
    /// The advance between the glyph's position and the glyph itself.
    pub leading: Pixels,
    /// The advance between the end of the glyph and the position of the next.
    pub trailing: Pixels,
}

/// Whether the character hangs at the end of a line.
fn hangs(character: char) -> bool {
    matches!(character, ' ' | '\t')
}

/// The distance between the tab stops, for the font a tab is shaped in. `None`
/// when it cannot be known, and the tab then keeps the advance it was shaped with.
pub(super) type TabStopFor<'a> = &'a dyn Fn(FontId) -> Option<Pixels>;

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
#[derive(Debug)]
pub struct LineLayout {
    /// The font size for this line
    pub font_size: Pixels,
    /// The width of the line
    pub width: Pixels,
    /// The ascent of the line
    pub ascent: Pixels,
    /// The descent of the line
    pub descent: Pixels,
    /// The device pixels to a pixel of the display this line was shaped for. The ascent and
    /// descent of the line and of its runs are rounded to whole device pixels, and the
    /// half-leading of a row is floored to one, so the rows of the line are only as Chrome's
    /// at this scale.
    pub scale_factor: f32,
    /// The shaped runs that make up this line
    pub runs: Vec<ShapedRun>,
    /// The length of the line in utf-8 bytes
    pub len: usize,
    /// The boxes with a [key](crate::InlineBox::key) that are on this line, and where
    /// they sit.
    pub boxes: Vec<PlacedBox>,
}

impl Default for LineLayout {
    fn default() -> Self {
        Self {
            font_size: Pixels::default(),
            width: Pixels::default(),
            ascent: Pixels::default(),
            descent: Pixels::default(),
            scale_factor: 1.,
            runs: Vec::new(),
            len: 0,
            boxes: Vec::new(),
        }
    }
}

/// A run of text that has been shaped .
#[derive(Debug, Clone)]
pub struct ShapedRun {
    /// The font id for this run
    pub font_id: FontId,
    /// The size this run was shaped at
    pub font_size: Pixels,
    /// The ascent of the font of this run, at the size of this run
    pub ascent: Pixels,
    /// The descent of the font of this run, at the size of this run
    pub descent: Pixels,
    /// The line height of this run, if it has one of its own
    pub line_height: Option<Pixels>,
    /// The glyphs that make up this run
    pub glyphs: Vec<ShapedGlyph>,
    /// How the run sits on its line besides its font. A platform shaper leaves it at its
    /// default: it is set when the line is laid out with [`InlineBox`]es.
    pub inline: InlineRun,
}

impl ShapedRun {
    /// The ascent and descent of the content area of an inline box with the font size
    /// `font_size`, given the runs of its text: the largest of those shaped at
    /// `font_size`, as a nested box with a font size of its own does not change the
    /// content area of the box around it. When all its text is in such nested boxes,
    /// the metrics of the first run scaled to `font_size`. `None` without a run.
    pub fn content_extent<'a>(
        runs: impl IntoIterator<Item = &'a ShapedRun>,
        font_size: Pixels,
    ) -> Option<(Pixels, Pixels)> {
        let mut first: Option<&ShapedRun> = None;
        let mut own: Option<(Pixels, Pixels)> = None;
        for run in runs {
            first.get_or_insert(run);
            if (run.font_size - font_size).abs() < px(1e-3) {
                own = Some(match own {
                    Some(own) => (own.0.max(run.ascent), own.1.max(run.descent)),
                    None => (run.ascent, run.descent),
                });
            }
        }
        own.or_else(|| {
            first.map(|run| {
                let scale = font_size / run.font_size;
                (run.ascent * scale, run.descent * scale)
            })
        })
    }
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

/// The height of a wrapped row and where its baseline is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowMetrics {
    /// The height of the row.
    pub height: Pixels,
    /// The distance from the top of the row to its baseline.
    pub baseline: Pixels,
}

/// The rows a line makes when wrapped, and where each run sits in them.
#[derive(Clone, Debug, PartialEq)]
pub struct LineRows {
    /// The metrics of each row.
    pub rows: SmallVec<[RowMetrics; 1]>,
    /// The runs that are not on the baseline of their row, with how far above it.
    raises: SmallVec<[RunRaise; 1]>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RunRaise {
    row_ix: usize,
    run_ix: usize,
    raise: Pixels,
}

impl LineRows {
    /// How far above the baseline of row `row_ix` the baseline of run `run_ix` is, where it
    /// is in that row: zero for a run on the baseline, negative for one below it.
    pub fn raise(&self, run_ix: usize, row_ix: usize) -> Pixels {
        self.raises
            .iter()
            .find(|raise| raise.row_ix == row_ix && raise.run_ix == run_ix)
            .map_or(Pixels::ZERO, |raise| raise.raise)
    }
}

impl LineLayout {
    /// How far above the baseline of row `row_ix`, of the `rows` this line makes, the
    /// baseline of the box with `key` is: zero for a box on the baseline, negative for one
    /// below it. Zero too for a box this line has no key for, and for no key, which is the
    /// baseline of the row.
    pub fn box_raise(&self, key: Option<u64>, row_ix: usize, rows: &[RowMetrics]) -> Pixels {
        let Some(placed) = key.and_then(|key| self.boxes.iter().find(|placed| placed.key == key))
        else {
            return Pixels::ZERO;
        };
        let row = rows[row_ix];
        match placed.placement {
            RunPlacement::Baseline => Pixels::ZERO,
            RunPlacement::Raised(raise) => raise,
            RunPlacement::Top => row.baseline - placed.above,
            RunPlacement::Bottom => row.baseline - (row.height - placed.below),
        }
    }

    /// Whether the run is an inline box of its own in the line: it differs from the
    /// strut in the size or the line height it was set with, or in the content area its
    /// font has at that size, so the line is as tall as both together need.
    pub(crate) fn is_styled(&self, run: &ShapedRun) -> bool {
        run.font_size != self.font_size
            || run.line_height.is_some()
            || run.ascent != self.ascent
            || run.descent != self.descent
            || !run.inline.is_default()
    }

    fn has_styled_runs(&self) -> bool {
        self.runs.iter().any(|run| self.is_styled(run))
    }

    /// The metrics of each row this line makes when wrapped at `wrap_boundaries`, as
    /// CSS lays out a line box: the line's own ascent and descent with `line_height`
    /// form a strut, and every run with a font size or line height of its own that has
    /// a glyph in the row grows the row around the shared baseline.
    pub fn rows(
        &self,
        wrap_boundaries: &[WrapBoundary],
        line_height: Pixels,
    ) -> SmallVec<[RowMetrics; 1]> {
        self.line_rows(wrap_boundaries, line_height).rows
    }

    /// [`Self::rows`] with where the runs sit in them. A run with a vertical alignment
    /// is raised or lowered from the baseline of its row, and one aligned to the top or
    /// bottom of the row is not on its baseline at all, as CSS 2.1 §10.8 has it: the row
    /// is first made as tall as the runs on its baseline need, and then as tall as the
    /// tallest run aligned to its top or bottom.
    pub fn line_rows(&self, wrap_boundaries: &[WrapBoundary], line_height: Pixels) -> LineRows {
        let row_count = wrap_boundaries.len() + 1;
        let (strut_above, strut_below) =
            extent_in_line(self.ascent, self.descent, line_height, self.scale_factor);
        let strut = RowMetrics {
            height: line_height,
            baseline: strut_above,
        };
        if !self.has_styled_runs() {
            return LineRows {
                rows: smallvec![strut; row_count],
                raises: SmallVec::new(),
            };
        }

        let mut above: SmallVec<[Pixels; 1]> = smallvec![strut_above; row_count];
        let mut below: SmallVec<[Pixels; 1]> = smallvec![strut_below; row_count];
        let mut top_height: SmallVec<[Pixels; 1]> = smallvec![Pixels::ZERO; row_count];
        let mut bottom_height: SmallVec<[Pixels; 1]> = smallvec![Pixels::ZERO; row_count];
        // Every run that is not simply on the baseline, in each row it has a glyph in.
        let mut placed: SmallVec<[(usize, usize, RunPlacement, Pixels, Pixels); 1]> =
            SmallVec::new();
        let mut row_ix = 0;
        let mut next_boundary = 0;
        for (run_ix, run) in self.runs.iter().enumerate() {
            if !self.is_styled(run) {
                while wrap_boundaries
                    .get(next_boundary)
                    .is_some_and(|boundary| boundary.run_ix == run_ix)
                {
                    row_ix += 1;
                    next_boundary += 1;
                }
                continue;
            }

            let run_line_height = run.line_height.unwrap_or(line_height);
            let (run_above, run_below) =
                extent_in_line(run.ascent, run.descent, run_line_height, self.scale_factor);
            // The boxes of its aligned group around an atomic inline are one with it.
            let (run_above, run_below) = match run.inline.aligned_strut {
                Some(strut) => (run_above.max(strut.above), run_below.max(strut.below)),
                None => (run_above, run_below),
            };
            let mut last_row = None;
            for glyph_ix in 0..run.glyphs.len() {
                while wrap_boundaries.get(next_boundary) == Some(&WrapBoundary { run_ix, glyph_ix })
                {
                    row_ix += 1;
                    next_boundary += 1;
                }
                if last_row == Some(row_ix) {
                    continue;
                }
                last_row = Some(row_ix);
                match run.inline.placement {
                    RunPlacement::Baseline => {
                        above[row_ix] = above[row_ix].max(run_above);
                        below[row_ix] = below[row_ix].max(run_below);
                    }
                    RunPlacement::Raised(raise) => {
                        above[row_ix] = above[row_ix].max(run_above + raise);
                        below[row_ix] = below[row_ix].max(run_below - raise);
                    }
                    RunPlacement::Top => {
                        top_height[row_ix] = top_height[row_ix].max(run_above + run_below);
                    }
                    RunPlacement::Bottom => {
                        bottom_height[row_ix] = bottom_height[row_ix].max(run_above + run_below);
                    }
                }
                // Every inline box has its strut in its row, even one with nothing but a box in it.
                if let Some(strut) = run.inline.strut {
                    above[row_ix] = above[row_ix].max(strut.above);
                    below[row_ix] = below[row_ix].max(strut.below);
                }
                if run.inline.placement != RunPlacement::Baseline {
                    placed.push((row_ix, run_ix, run.inline.placement, run_above, run_below));
                }
            }
        }

        let rows: SmallVec<[RowMetrics; 1]> = (0..row_count)
            .map(|row_ix| {
                let (mut above, mut below) = (above[row_ix], below[row_ix]);
                // The taller of the runs aligned to the top and to the bottom decides
                // which way the row grows: down for the top one, up for the bottom one.
                let (top, bottom) = (top_height[row_ix], bottom_height[row_ix]);
                if top.max(bottom) > above + below {
                    if top >= bottom {
                        below = top - above;
                    } else {
                        above = bottom - below;
                    }
                }
                if above == strut_above && below == strut_below {
                    strut
                } else {
                    RowMetrics {
                        height: above + below,
                        baseline: above,
                    }
                }
            })
            .collect();
        let raises = placed
            .into_iter()
            .filter_map(|(row_ix, run_ix, placement, run_above, run_below)| {
                let row = rows[row_ix];
                let raise = match placement {
                    RunPlacement::Baseline => return None,
                    RunPlacement::Raised(raise) => raise,
                    RunPlacement::Top => row.baseline - run_above,
                    RunPlacement::Bottom => row.baseline - (row.height - run_below),
                };
                (raise != Pixels::ZERO).then_some(RunRaise {
                    row_ix,
                    run_ix,
                    raise,
                })
            })
            .collect();
        LineRows { rows, raises }
    }

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
    /// - `font_size`, `ascent`, and `descent` of the line and of each run are copied to both halves.
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
                    font_size: run.font_size,
                    ascent: run.ascent,
                    descent: run.descent,
                    line_height: run.line_height,
                    glyphs: run.glyphs[..split_pos].to_vec(),
                    inline: run.inline,
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
                    font_size: run.font_size,
                    ascent: run.ascent,
                    descent: run.descent,
                    line_height: run.line_height,
                    glyphs: right_glyphs,
                    inline: run.inline,
                });
            }
        }

        let left = LineLayout {
            font_size: self.font_size,
            width: x_offset,
            ascent: self.ascent,
            descent: self.descent,
            scale_factor: self.scale_factor,
            runs: left_runs,
            len: byte_index,
            boxes: self.boxes.clone(),
        };

        let right = LineLayout {
            font_size: self.font_size,
            width: self.width - x_offset,
            ascent: self.ascent,
            descent: self.descent,
            scale_factor: self.scale_factor,
            runs: right_runs,
            len: self.len - byte_index,
            boxes: self.boxes.clone(),
        };

        (left, right)
    }

    /// With a `tab_stop` or `insets`, also returns the x of every glyph, and then
    /// of the end of the line, after each tab character has advanced to its tab
    /// stop and each inset has pushed the glyphs after it. The stops are measured
    /// from the start of the wrapped line the tab is on, so where the lines break
    /// decides where the tabs end.
    pub(super) fn compute_wrap_boundaries(
        &self,
        text: &str,
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        options: TextLayoutOptions,
        tab_stop: Option<TabStopFor<'_>>,
        insets: &[TextInset],
    ) -> WrapComputation {
        let hang = options.spaces_hang();
        let line_breaks = LineBreaks::new(text, options.word_break == WordBreak::KeepAll);
        let break_all = options.word_break == WordBreak::BreakAll;
        let breaks_words = options.overflow_wrap != OverflowWrap::Normal;
        let mut boundaries = SmallVec::new();
        // The glyph each line starts at.
        let mut line_starts: SmallVec<[usize; 2]> = SmallVec::new();
        line_starts.push(0);
        let mut first_non_whitespace_ix = None;
        let mut last_candidate_ix = None;
        let mut last_boundary = WrapBoundary {
            run_ix: 0,
            glyph_ix: 0,
        };
        let mut last_boundary_x = px(0.);
        let mut broke_word = false;
        let mut prev_was_object = false;
        let mut word_wraps = !options.nowrap;
        let mut spaces: Option<SpaceRun> = None;
        let mut positions = GlyphPositions::new(self, text, tab_stop, options, insets);
        let mut placed = 0;

        for ix in 0..positions.glyphs.len() {
            positions.place(ix + 1, last_boundary_x);
            placed = ix + 1;
            let boundary = positions.glyphs[ix].boundary;
            let ch = positions.glyphs[ix].wrap_char;
            if ch == '\n' {
                continue;
            }

            // The spaces before this glyph and what a line may do around them.
            if ch == ' ' {
                let wrap = positions.glyphs[ix].wrap;
                let run = spaces.get_or_insert(SpaceRun {
                    word_wraps,
                    space_wraps: wrap.wraps,
                    opens_nowrap: false,
                });
                run.space_wraps = wrap.wraps;
                run.opens_nowrap |= wrap.opens_nowrap;
            }
            // Whether a line may break right before this glyph, which is a place a
            // line breaks at if the text wraps there.
            let may_break_here = match spaces {
                Some(run) => run.may_break(),
                None => ix == 0 || may_break_apart(&positions.glyphs[ix - 1], &positions.glyphs[ix]),
            };

            // Here is very similar to `LineWrapper::wrap_line` to determine text wrapping,
            // but there are some differences, so we have to duplicate the code here.
            if positions.glyphs[ix].is_object {
                // A line may break on either side of an atomic inline, like an image.
                if first_non_whitespace_ix.is_some() && may_break_here {
                    last_candidate_ix = Some(ix);
                }
            } else if prev_was_object && ch != ' ' && first_non_whitespace_ix.is_some() {
                if may_break_here {
                    last_candidate_ix = Some(ix);
                }
            } else if ch != ' '
                && first_non_whitespace_ix.is_some()
                && may_break_here
                && line_breaks.contains(positions.glyphs[ix].index)
            {
                last_candidate_ix = Some(ix);
            }

            if ch != ' ' && first_non_whitespace_ix.is_none() {
                first_non_whitespace_ix = Some(boundary);
            }

            // A hanging space is not measured, so it never causes a wrap: the word
            // after it overflows instead, and the line wraps before that word.
            let hanging = hang && positions.glyphs[ix].hangs;
            let width = positions.x(ix + 1) - last_boundary_x;

            // Without a place to break, a word that does not fit is left to overflow
            // when `overflow_wrap` is normal, and goes on until one turns up. A word
            // is not broken where the text does not wrap.
            if !hanging
                && wrap_width.is_some_and(|wrap_width| crate::util::overflows(width, wrap_width))
                && boundary > last_boundary
                && (last_candidate_ix.is_some()
                    || (breaks_words
                        && ix > 0
                        && may_break_apart(&positions.glyphs[ix - 1], &positions.glyphs[ix])))
            {
                // When used line_clamp, we should limit the number of lines.
                if let Some(max_lines) = max_lines
                    && boundaries.len() >= max_lines.saturating_sub(1)
                {
                    break;
                }

                let candidate_ix = last_candidate_ix.take();
                broke_word |= candidate_ix.is_none();
                let line_start_ix = candidate_ix.unwrap_or(ix);
                last_boundary = positions.glyphs[line_start_ix].boundary;
                last_boundary_x = positions.x(line_start_ix);
                boundaries.push(last_boundary);
                line_starts.push(line_start_ix);
                // The tab stops of what moved to the new line count from its start.
                for moved_ix in line_start_ix + 1..=ix + 1 {
                    positions.place(moved_ix, last_boundary_x);
                }
            }
            // After a space the line may break, and a space that does not fit then
            // starts the next line. Taken after the check above, so that the space
            // itself is measured against the breaks before it.
            if options.break_spaces && ch == ' ' && positions.glyphs[ix].wrap.wraps {
                last_candidate_ix = Some(ix + 1);
            }
            // Between letters the line may break, so the candidate is always the
            // glyph that overflows, and the line is filled to the edge.
            if break_all
                && let Some(next) = positions.glyphs.get(ix + 1)
                && may_break_between(ch, next.wrap_char)
                && may_break_apart(&positions.glyphs[ix], next)
            {
                last_candidate_ix = Some(ix + 1);
            }
            if ch != ' ' {
                spaces = None;
                word_wraps = positions.glyphs[ix].wrap.wraps;
            }
            prev_was_object = positions.glyphs[ix].is_object;
        }

        for ix in placed + 1..=positions.glyphs.len() {
            positions.place(ix, last_boundary_x);
        }

        let hanging_widths = if hang {
            positions.hanging_widths(&line_starts)
        } else {
            SmallVec::new()
        };
        let line_widths = positions.line_widths(&line_starts, hang);
        let line_ends = positions.content_ends(&line_starts, hang);
        let last_line_words = positions.words_on_line(line_starts[line_starts.len() - 1]);
        let (tabbed_x, glyph_insets) = positions.into_placed();
        WrapComputation {
            boundaries,
            tabbed_x,
            hanging_widths,
            glyph_insets,
            line_widths,
            last_line_words,
            broke_word,
            line_starts,
            line_ends,
        }
    }

    /// Where the text may break, and the x of its glyphs. A text wrapped at no width
    /// at all breaks at every place a line may break, without cutting a word.
    pub(super) fn break_opportunities(
        &self,
        text: &str,
        options: TextLayoutOptions,
        insets: &[TextInset],
    ) -> BreakOpportunities {
        let everywhere = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            text_wrap_style: TextWrapStyle::Auto,
            ..options
        };
        let computation =
            self.compute_wrap_boundaries(text, Some(px(0.)), None, everywhere, None, insets);
        let breaks = computation.line_starts[1..].to_vec();
        let glyphs = || self.runs.iter().flat_map(|run| run.glyphs.iter());
        let x = computation.tabbed_x.unwrap_or_else(|| {
            glyphs()
                .map(|glyph| glyph.position.x)
                .chain(iter::once(self.width))
                .collect()
        });
        let hangs = glyphs()
            .map(|glyph| text[glyph.index..].chars().next().is_some_and(hangs))
            .collect();
        BreakOpportunities { x, hangs, breaks }
    }

    /// The wrap computation of a line that starts a new line at each of the glyphs
    /// `breaks`, as [`Self::compute_wrap_boundaries`] would give it had it broken
    /// there. Without tab stops, which count from the start of the line they are on.
    pub(super) fn compute_wrap_boundaries_at(
        &self,
        text: &str,
        breaks: &[usize],
        options: TextLayoutOptions,
        insets: &[TextInset],
    ) -> WrapComputation {
        let hang = options.spaces_hang();
        let mut positions = GlyphPositions::new(self, text, None, options, insets);
        for ix in 1..=positions.glyphs.len() {
            positions.place(ix, px(0.));
        }
        let boundaries = breaks
            .iter()
            .map(|ix| positions.glyphs[*ix].boundary)
            .collect();
        let mut line_starts: SmallVec<[usize; 2]> = SmallVec::new();
        line_starts.push(0);
        line_starts.extend(breaks.iter().copied());
        let hanging_widths = if hang {
            positions.hanging_widths(&line_starts)
        } else {
            SmallVec::new()
        };
        let line_widths = positions.line_widths(&line_starts, hang);
        let line_ends = positions.content_ends(&line_starts, hang);
        let last_line_words = positions.words_on_line(line_starts[line_starts.len() - 1]);
        let (tabbed_x, glyph_insets) = positions.into_placed();
        WrapComputation {
            boundaries,
            tabbed_x,
            hanging_widths,
            glyph_insets,
            line_widths,
            last_line_words,
            broke_word: false,
            line_starts,
            line_ends,
        }
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
                font_size: run.font_size,
                ascent: run.ascent,
                descent: run.descent,
                line_height: run.line_height,
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|glyph| ShapedGlyph {
                        position: point(*x_iter.next().expect("one x per glyph"), glyph.position.y),
                        ..glyph.clone()
                    })
                    .collect(),
                inline: run.inline,
            })
            .collect();
        LineLayout {
            font_size: self.font_size,
            width: *x.last().expect("the end of the line has an x"),
            ascent: self.ascent,
            descent: self.descent,
            scale_factor: self.scale_factor,
            runs,
            len: self.len,
            boxes: self.boxes.clone(),
        }
    }
}

#[derive(Clone)]
pub(super) struct WrapComputation {
    pub(super) boundaries: SmallVec<[WrapBoundary; 1]>,
    /// With tab stops or insets, the x of every glyph, and then of the end of the
    /// line.
    pub(super) tabbed_x: Option<Vec<Pixels>>,
    /// How much of each line hangs, or nothing when spaces do not hang.
    pub(super) hanging_widths: SmallVec<[Pixels; 1]>,
    pub(super) glyph_insets: Vec<GlyphInset>,
    /// How wide each line is as alignment sees it: without its hanging spaces.
    pub(super) line_widths: SmallVec<[Pixels; 1]>,
    /// How many words, runs of anything but spaces and tabs, the last line has.
    pub(super) last_line_words: usize,
    /// Whether a line broke inside a word because the word did not fit.
    pub(super) broke_word: bool,
    /// The glyph each line starts at, counting through all the runs.
    pub(super) line_starts: SmallVec<[usize; 2]>,
    /// The glyph each line ends at, before the spaces that hang at its end.
    pub(super) line_ends: SmallVec<[usize; 1]>,
}

impl WrapComputation {
    /// A line of `glyph_count` glyphs that does not break.
    fn unbroken(glyph_count: usize) -> Self {
        Self {
            boundaries: SmallVec::new(),
            tabbed_x: None,
            hanging_widths: SmallVec::new(),
            glyph_insets: Vec::new(),
            line_widths: SmallVec::new(),
            last_line_words: 0,
            broke_word: false,
            line_starts: smallvec![0],
            line_ends: smallvec![glyph_count],
        }
    }
}

/// The places a text may break and where each of its glyphs is, for choosing among
/// the ways it can break.
pub(super) struct BreakOpportunities {
    /// The x of every glyph, and then of the end of the line.
    pub(super) x: Vec<Pixels>,
    /// Whether each glyph is a space or tab that hangs at the end of a line.
    pub(super) hangs: Vec<bool>,
    /// The glyphs a line may start at, but the first.
    pub(super) breaks: Vec<usize>,
}

struct WrapGlyph {
    boundary: WrapBoundary,
    /// The byte of the text the glyph starts at.
    index: usize,
    /// The character at the glyph's index, as the wrapping rules see it.
    wrap_char: char,
    is_tab: bool,
    /// The placeholder of an atomic inline, which a line may break on either side of.
    is_object: bool,
    hangs: bool,
    font_id: FontId,
    shaped_x: Pixels,
    wrap: RunWrap,
}

/// Whether a line may break between two characters that are neither of them a space,
/// `before` and `after`, which are next to each other. CSS gives the box around both
/// the say, which Chromium reads as: any of the three boxes lets it.
fn may_break_apart(before: &WrapGlyph, after: &WrapGlyph) -> bool {
    after.wrap.joint_wraps || before.wrap.wraps || after.wrap.wraps
}

/// The spaces between two words, as far as whether a line may break after them. The space
/// is the box's to say, as CSS has it, and Chromium lets the word before it say too,
/// unless a box that does not wrap starts at the space.
#[derive(Clone, Copy)]
struct SpaceRun {
    word_wraps: bool,
    space_wraps: bool,
    opens_nowrap: bool,
}

impl SpaceRun {
    fn may_break(&self) -> bool {
        self.space_wraps || (self.word_wraps && !self.opens_nowrap)
    }
}

/// The x of each glyph of a line and then of its end. They are the shaped
/// ones, unless the line has tab stops or insets.
struct GlyphPositions<'a> {
    glyphs: Vec<WrapGlyph>,
    shaped_width: Pixels,
    tabbed: Option<TabbedPositions<'a>>,
    /// Sorted by glyph.
    insets: Vec<GlyphInset>,
}

struct TabbedPositions<'a> {
    tab_stop: Option<TabStopFor<'a>>,
    /// Final x per glyph, then the end of the line.
    x: Vec<Pixels>,
    /// What the tabs and insets before each glyph added to its shaped x.
    shift: Vec<Pixels>,
}

/// The insets that each glyph of `layout` carries, in glyph order. The leading
/// inset of a range goes to the first glyph that starts inside it and the
/// trailing one to the last, so a range with no glyph of its own adds nothing.
fn glyph_insets(layout: &LineLayout, insets: &[TextInset]) -> Vec<GlyphInset> {
    if insets.is_empty() {
        return Vec::new();
    }
    let indices: Vec<usize> = layout
        .runs
        .iter()
        .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index))
        .collect();
    let mut glyph_insets: Vec<GlyphInset> = Vec::new();
    for inset in insets {
        let first = indices.partition_point(|index| *index < inset.range.start);
        let end = indices.partition_point(|index| *index < inset.range.end);
        if first == end {
            continue;
        }
        let last = end - 1;
        for (glyph_ix, leading, trailing) in [
            (first, inset.leading, Pixels::ZERO),
            (last, Pixels::ZERO, inset.trailing),
        ] {
            glyph_insets.push(GlyphInset {
                glyph_ix,
                index: indices[glyph_ix],
                leading,
                trailing,
            });
        }
    }
    glyph_insets.sort_by_key(|inset| inset.glyph_ix);
    glyph_insets.dedup_by(|later, earlier| {
        let same_glyph = later.glyph_ix == earlier.glyph_ix;
        if same_glyph {
            earlier.leading += later.leading;
            earlier.trailing += later.trailing;
        }
        same_glyph
    });
    glyph_insets
}

impl<'a> GlyphPositions<'a> {
    fn new(
        layout: &LineLayout,
        text: &str,
        tab_stop: Option<TabStopFor<'a>>,
        options: TextLayoutOptions,
        insets: &[TextInset],
    ) -> Self {
        let tab_is_space = tab_stop.is_some() || options.spaces_hang() || options.break_spaces;
        let text_wraps = RunWrap {
            wraps: !options.nowrap,
            joint_wraps: !options.nowrap,
            opens_nowrap: false,
        };
        let glyphs: Vec<WrapGlyph> = layout
            .runs
            .iter()
            .enumerate()
            .flat_map(|(run_ix, run)| {
                run.glyphs.iter().enumerate().map(move |(glyph_ix, glyph)| {
                    let character = text[glyph.index..].chars().next().unwrap();
                    let run_wrap = run.inline.wrap.unwrap_or(text_wraps);
                    WrapGlyph {
                        // Only the first glyph of a run has a place that may break
                        // between it and the run before.
                        wrap: if glyph_ix == 0 {
                            run_wrap
                        } else {
                            RunWrap {
                                joint_wraps: run_wrap.wraps,
                                opens_nowrap: false,
                                ..run_wrap
                            }
                        },
                        boundary: WrapBoundary { run_ix, glyph_ix },
                        index: glyph.index,
                        // A tab is white space that may break, like the space it is
                        // when the text is not laid out with tab stops.
                        wrap_char: if tab_is_space && character == '\t' {
                            ' '
                        } else {
                            character
                        },
                        is_tab: character == '\t',
                        is_object: run.inline.object,
                        hangs: hangs(character),
                        font_id: run.font_id,
                        shaped_x: glyph.position.x,
                    }
                })
            })
            .collect();
        let insets = glyph_insets(layout, insets);
        let tabbed = (tab_stop.is_some() || !insets.is_empty()).then(|| {
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
            insets,
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
        let (leading, trailing) = match self
            .insets
            .binary_search_by_key(&(ix - 1), |inset| inset.glyph_ix)
        {
            Ok(inset) => (self.insets[inset].leading, self.insets[inset].trailing),
            Err(_) => (Pixels::ZERO, Pixels::ZERO),
        };
        let stop = tabbed
            .tab_stop
            .filter(|_| previous.is_tab)
            .and_then(|tab_stop| tab_stop(previous.font_id));
        if let Some(stop) = stop {
            // The tab starts after its own leading inset, and its stop counts from there.
            let pen = tabbed.x[ix - 1] + leading;
            tabbed.x[ix] = pen + advance_to_tab_stop(pen - line_start_x, stop) + trailing;
            tabbed.shift[ix] = tabbed.x[ix] - shaped_x;
        } else {
            tabbed.shift[ix] = tabbed.shift[ix - 1] + leading + trailing;
            tabbed.x[ix] = shaped_x + tabbed.shift[ix];
        }
    }

    /// How wide the spaces at the end of each line are, for lines that start at
    /// the glyphs `line_starts`. A line of nothing else hangs whole.
    fn hanging_widths(&self, line_starts: &[usize]) -> SmallVec<[Pixels; 1]> {
        let line_end = |line_ix: usize| {
            line_starts
                .get(line_ix + 1)
                .copied()
                .unwrap_or(self.glyphs.len())
        };
        self.content_ends(line_starts, true)
            .into_iter()
            .enumerate()
            .map(|(line_ix, content_end)| self.x(line_end(line_ix)) - self.x(content_end))
            .collect()
    }

    /// The glyph each line of those that start at `line_starts` ends at, as alignment
    /// sees it: before the spaces that hang at its end, when `hang`.
    fn content_ends(&self, line_starts: &[usize], hang: bool) -> SmallVec<[usize; 1]> {
        line_starts
            .iter()
            .enumerate()
            .map(|(line_ix, line_start)| {
                let line_end = line_starts
                    .get(line_ix + 1)
                    .copied()
                    .unwrap_or(self.glyphs.len());
                let mut end = line_end;
                while hang && end > *line_start && self.glyphs[end - 1].hangs {
                    end -= 1;
                }
                end
            })
            .collect()
    }

    /// How wide each line is, for lines that start at the glyphs `line_starts`:
    /// as alignment sees it, so without the spaces that hang at its end.
    fn line_widths(&self, line_starts: &[usize], hang: bool) -> SmallVec<[Pixels; 1]> {
        let hanging = if hang {
            self.hanging_widths(line_starts)
        } else {
            SmallVec::new()
        };
        line_starts
            .iter()
            .enumerate()
            .map(|(line_ix, line_start)| {
                let line_end = line_starts
                    .get(line_ix + 1)
                    .copied()
                    .unwrap_or(self.glyphs.len());
                self.x(line_end)
                    - self.x(*line_start)
                    - hanging.get(line_ix).copied().unwrap_or(Pixels::ZERO)
            })
            .collect()
    }

    /// How many words the line that starts at glyph `line_start` and runs to the
    /// end of the text has.
    fn words_on_line(&self, line_start: usize) -> usize {
        let mut words = 0;
        let mut in_word = false;
        for glyph in &self.glyphs[line_start..] {
            let in_space = glyph.hangs || glyph.wrap_char == ' ';
            if !in_space && !in_word {
                words += 1;
            }
            in_word = !in_space;
        }
        words
    }

    fn into_placed(self) -> (Option<Vec<Pixels>>, Vec<GlyphInset>) {
        (self.tabbed.map(|tabbed| tabbed.x), self.insets)
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

    /// How wide the spaces that hang at the end of each wrapped line are, which
    /// alignment leaves out. Empty when the layout does not hang spaces.
    pub hanging_widths: SmallVec<[Pixels; 1]>,

    /// The insets of the glyphs that have any, in glyph order. Empty when the
    /// line was laid out without insets.
    pub insets: Vec<GlyphInset>,

    /// How wide each wrapped line is as Chrome measures it, see [`SnapMarks`]:
    /// without the spaces that hang at its end.
    line_widths: SmallVec<[Pixels; 1]>,

    /// How wide the line is whole, measured the same way and counting the spaces
    /// that hang.
    snapped_width: Pixels,
}

/// Where the placeholder of an atomic inline sits in a wrapped line: the top left corner
/// of the box's margin box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectPlacement {
    /// The byte of the text the placeholder is at.
    pub index: usize,
    /// The wrapped row of the line the box is on.
    pub row_ix: usize,
    /// The distance from the start of the row to the box, before the row is aligned and
    /// after the leading inset of the box's glyph.
    pub x: Pixels,
    /// The distance from the top of the line to the top of the box.
    pub y: Pixels,
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
    /// The layout of `unwrapped_layout`, which is `computation` placed, wrapped at
    /// `wrap_width`. `marks` are those of the line the unwrapped layout was made from.
    pub(super) fn new(
        unwrapped_layout: Arc<LineLayout>,
        wrap_width: Option<Pixels>,
        computation: WrapComputation,
        marks: &SnapMarks,
    ) -> Self {
        let x = glyph_positions(&unwrapped_layout);
        let scale_factor = unwrapped_layout.scale_factor;
        let line_widths = computation
            .line_starts
            .iter()
            .zip(&computation.line_ends)
            .map(|(start, end)| marks.width(&x, scale_factor, *start, *end))
            .collect();
        let snapped_width = marks.width(&x, scale_factor, 0, x.len() - 1);
        Self {
            unwrapped_layout,
            wrap_boundaries: computation.boundaries,
            wrap_width,
            hanging_widths: computation.hanging_widths,
            insets: computation.glyph_insets,
            line_widths,
            snapped_width,
        }
    }

    /// The length of the underlying text, in utf8 bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.unwrapped_layout.len
    }

    /// The width of this line, in pixels, whether or not it was wrapped.
    pub fn width(&self) -> Pixels {
        self.wrap_width
            .unwrap_or(Pixels::MAX)
            .min(self.snapped_width)
    }

    /// How wide each wrapped line is as Chrome measures it, a multiple of a layout
    /// unit (1/64 of a device pixel): without the spaces that hang at its end, and
    /// counting the tab stops and insets on it. A line of several style runs is the
    /// sum of the widths of its parts in each, where each is ceiled on its own.
    pub fn line_widths(&self) -> impl Iterator<Item = Pixels> + '_ {
        self.line_widths.iter().copied()
    }

    /// The metrics of each wrapped row of this line, for the given line height.
    pub fn rows(&self, line_height: Pixels) -> SmallVec<[RowMetrics; 1]> {
        self.unwrapped_layout
            .rows(&self.wrap_boundaries, line_height)
    }

    /// The largest ascent and descent among the runs that have a glyph in `bytes`,
    /// or `None` when no glyph is. This is the content area of an inline box that
    /// spans those bytes.
    pub fn run_extent(&self, bytes: Range<usize>) -> Option<(Pixels, Pixels)> {
        self.runs_in(bytes)
            .map(|run| (run.ascent, run.descent))
            .reduce(|first, second| (first.0.max(second.0), first.1.max(second.1)))
    }

    /// The runs of text that have a glyph in `bytes`. The placeholder of an atomic inline
    /// is not one: it is not part of the content area of the inline box around it.
    pub fn runs_in(&self, bytes: Range<usize>) -> impl Iterator<Item = &ShapedRun> {
        self.unwrapped_layout.runs.iter().filter(move |run| {
            !run.inline.object && run.glyphs.iter().any(|glyph| bytes.contains(&glyph.index))
        })
    }

    /// The ascent and descent of the font of an inline box with the font size
    /// `font_size` that spans `bytes`, or `None` when no glyph is in `bytes`. See
    /// [`ShapedRun::content_extent`].
    pub fn content_extent(
        &self,
        bytes: Range<usize>,
        font_size: Pixels,
    ) -> Option<(Pixels, Pixels)> {
        ShapedRun::content_extent(self.runs_in(bytes), font_size)
    }

    /// The size of the whole wrapped text, for the given line_height.
    /// can span multiple lines if there are multiple wrap boundaries.
    pub fn size(&self, line_height: Pixels) -> Size<Pixels> {
        let height = if self.unwrapped_layout.has_styled_runs() {
            self.rows(line_height)
                .iter()
                .fold(Pixels::ZERO, |height, row| height + row.height)
        } else {
            line_height * (self.wrap_boundaries.len() + 1)
        };
        Size {
            width: self.width(),
            height,
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

    /// The wrapped row of the line that `y`, from the top of the line, is on. Past the last row it is one
    /// more than there are rows.
    pub(crate) fn row_at_y(&self, y: Pixels, line_height: Pixels) -> usize {
        let rows = self
            .unwrapped_layout
            .has_styled_runs()
            .then(|| self.rows(line_height));
        match &rows {
            None => (y / line_height) as usize,
            Some(rows) => {
                let mut bottom = Pixels::ZERO;
                rows.iter()
                    .position(|row| {
                        bottom += row.height;
                        y < bottom
                    })
                    .unwrap_or(rows.len())
            }
        }
    }

    fn _index_for_position(
        &self,
        mut position: Point<Pixels>,
        line_height: Pixels,
        closest: bool,
    ) -> Result<usize, usize> {
        let wrapped_line_ix = self.row_at_y(position.y, line_height);

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

    /// Returns the pixel position for the given byte index. For the placeholder of an
    /// atomic inline it is where the top left corner of the box's margin box is, unless the
    /// box wrapped to the next row: the placeholder is then at a wrap boundary, which is
    /// the end of the row above for every index (see [`Self::row_for_index`]), and a range
    /// of the text before the box ends there.
    pub fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
        let (row_ix, x) = self.row_and_x_for_index(index)?;
        if let Some(object) = self
            .objects(line_height)
            .into_iter()
            .find(|object| object.index == index && object.row_ix == row_ix)
        {
            return Some(point(object.x, object.y));
        }
        let rows = self
            .unwrapped_layout
            .has_styled_runs()
            .then(|| self.rows(line_height));
        Some(point(x, row_top(rows.as_deref(), row_ix, line_height)))
    }

    /// The distance from the top of the line to the baseline of the run that the glyph at
    /// `index`, or the first one after it, is in, where the run sits in its row. `None`
    /// when no glyph is at or after `index`.
    pub fn baseline_for_index(&self, index: usize, line_height: Pixels) -> Option<Pixels> {
        let layout = &self.unwrapped_layout;
        let (run_ix, glyph_ix) = layout.runs.iter().enumerate().find_map(|(run_ix, run)| {
            run.glyphs
                .iter()
                .position(|glyph| glyph.index >= index)
                .map(|glyph_ix| (run_ix, glyph_ix))
        })?;
        let line_rows = layout.line_rows(&self.wrap_boundaries, line_height);
        let row_ix = self
            .wrap_boundaries
            .partition_point(|boundary| *boundary <= WrapBoundary { run_ix, glyph_ix });
        let row_top = line_rows.rows[..row_ix]
            .iter()
            .fold(Pixels::ZERO, |top, row| top + row.height);
        Some(row_top + line_rows.rows[row_ix].baseline - line_rows.raise(run_ix, row_ix))
    }

    /// Where each atomic inline of this line sits, in the order they come in the text.
    pub fn objects(&self, line_height: Pixels) -> SmallVec<[ObjectPlacement; 2]> {
        let layout = &self.unwrapped_layout;
        let mut objects = SmallVec::new();
        if !layout.runs.iter().any(|run| run.inline.object) {
            return objects;
        }
        let line_rows = layout.line_rows(&self.wrap_boundaries, line_height);
        for (run_ix, run) in layout.runs.iter().enumerate() {
            if !run.inline.object {
                continue;
            }
            for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                let row_ix = self
                    .wrap_boundaries
                    .partition_point(|boundary| *boundary <= WrapBoundary { run_ix, glyph_ix });
                let row_start_x = match row_ix.checked_sub(1) {
                    Some(previous) => {
                        let boundary = self.wrap_boundaries[previous];
                        layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].position.x
                    }
                    None => Pixels::ZERO,
                };
                let row_top = line_rows.rows[..row_ix]
                    .iter()
                    .fold(Pixels::ZERO, |top, row| top + row.height);
                let baseline = line_rows.rows[row_ix].baseline - line_rows.raise(run_ix, row_ix);
                let leading = self
                    .insets
                    .iter()
                    .find(|inset| inset.index == glyph.index)
                    .map_or(Pixels::ZERO, |inset| inset.leading);
                objects.push(ObjectPlacement {
                    index: glyph.index,
                    row_ix,
                    x: glyph.position.x - row_start_x + leading,
                    y: row_top + baseline - run.ascent,
                });
            }
        }
        objects
    }

    /// The wrapped row `position_for_index` puts the byte index on: a boundary
    /// between two rows belongs to the end of the upper one.
    pub fn row_for_index(&self, index: usize) -> Option<usize> {
        self.row_and_x_for_index(index).map(|(row_ix, _)| row_ix)
    }

    fn row_and_x_for_index(&self, index: usize) -> Option<(usize, Pixels)> {
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
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_start_ix = line_end_ix;
                continue;
            } else {
                let line_start_x = self.unwrapped_layout.x_for_index(line_start_ix);
                let x = self.unwrapped_layout.x_for_index(index) - line_start_x;
                return Some((ix, x));
            }
        }

        None
    }
}

/// The top of row `row_ix` relative to the top of its line, given the rows of a
/// line with styled runs, or `None` for a line whose rows are all `line_height`.
fn row_top(rows: Option<&[RowMetrics]>, row_ix: usize, line_height: Pixels) -> Pixels {
    match rows {
        None => row_ix as f32 * line_height,
        Some(rows) => rows[..row_ix]
            .iter()
            .fold(Pixels::ZERO, |top, row| top + row.height),
    }
}

/// What the line layout cache has done since the window was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextCounters {
    /// Lines the platform shaped: cache misses.
    pub shapes: u64,
    /// Wrapped layouts computed (shaped, wrapped and inset) because no frame had them.
    pub wrapped_layouts: u64,
    /// Lookups that found a layout of this frame or of the frame before.
    pub hits: u64,
    /// Lookups that found neither but a layout the window shaped some frames ago, which the
    /// per-window shape store kept. They are not shapes.
    pub shape_cache_hits: u64,
}

#[derive(Default)]
struct AtomicTextCounters {
    shapes: AtomicU64,
    wrapped_layouts: AtomicU64,
    hits: AtomicU64,
    shape_cache_hits: AtomicU64,
}

impl AtomicTextCounters {
    fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) struct LineLayoutCache {
    previous_frame: Mutex<FrameCache>,
    current_frame: RwLock<FrameCache>,
    platform_text_system: Arc<dyn PlatformTextSystem>,
    counters: AtomicTextCounters,
    /// Behind the frame caches: what was shaped, for as long as the budget allows. Locked after
    /// the frame caches, and never while taking them.
    shape_store: Mutex<ShapeStore>,
    font_generation: Arc<AtomicU64>,
    /// The device pixels to a pixel of the window's display, as `f32` bits: what lines are
    /// shaped for, and part of the key of every layout.
    scale_factor: AtomicU32,
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

impl FrameCache {
    fn is_unused(&self) -> bool {
        self.lines.is_empty()
            && self.wrapped_lines.is_empty()
            && self.lines_by_hash.is_empty()
            && self.wrapped_lines_by_hash.is_empty()
            && self.used_lines.is_empty()
            && self.used_wrapped_lines.is_empty()
            && self.used_lines_by_hash.is_empty()
            && self.used_wrapped_lines_by_hash.is_empty()
    }
}

#[derive(Clone, Default)]
pub(crate) struct LineLayoutIndex {
    lines_index: usize,
    wrapped_lines_index: usize,
    lines_by_hash_index: usize,
    wrapped_lines_by_hash_index: usize,
}

impl LineLayoutIndex {
    /// Whether the range `inner` is inside `outer`.
    pub(crate) fn range_is_within(inner: &Range<Self>, outer: &Range<Self>) -> bool {
        let axis = |index: fn(&Self) -> usize| {
            index(&inner.start) >= index(&outer.start) && index(&inner.end) <= index(&outer.end)
        };
        axis(|index| index.lines_index)
            && axis(|index| index.wrapped_lines_index)
            && axis(|index| index.lines_by_hash_index)
            && axis(|index| index.wrapped_lines_by_hash_index)
    }

    /// Where the index `self`, which is inside the range that started at `from`, is once that
    /// range was copied to `to`.
    pub(crate) fn moved(&self, from: &Self, to: &Self) -> Self {
        Self {
            lines_index: self.lines_index + to.lines_index - from.lines_index,
            wrapped_lines_index: self.wrapped_lines_index + to.wrapped_lines_index - from.wrapped_lines_index,
            lines_by_hash_index: self.lines_by_hash_index + to.lines_by_hash_index - from.lines_by_hash_index,
            wrapped_lines_by_hash_index: self.wrapped_lines_by_hash_index + to.wrapped_lines_by_hash_index
                - from.wrapped_lines_by_hash_index,
        }
    }
}

impl LineLayoutCache {
    /// `font_generation` counts the times fonts were added to the text system, which empties the
    /// shape store.
    pub fn new(
        platform_text_system: Arc<dyn PlatformTextSystem>,
        font_generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            previous_frame: Mutex::default(),
            current_frame: RwLock::default(),
            platform_text_system,
            counters: AtomicTextCounters::default(),
            shape_store: Mutex::new(ShapeStore::new(SHAPE_STORE_BUDGET_BYTES)),
            font_generation,
            scale_factor: AtomicU32::new(1f32.to_bits()),
        }
    }

    pub fn scale_factor(&self) -> f32 {
        f32::from_bits(self.scale_factor.load(Ordering::Relaxed))
    }

    /// Lines shaped from now on are for a display of `scale_factor`; those of another scale
    /// stay in the caches under their own key.
    pub fn set_scale_factor(&self, scale_factor: f32) {
        self.scale_factor
            .store(scale_factor.to_bits(), Ordering::Relaxed);
    }

    fn font_generation(&self) -> u64 {
        self.font_generation.load(Ordering::Relaxed)
    }

    pub fn counters(&self) -> TextCounters {
        TextCounters {
            shapes: self.counters.shapes.load(Ordering::Relaxed),
            wrapped_layouts: self.counters.wrapped_layouts.load(Ordering::Relaxed),
            hits: self.counters.hits.load(Ordering::Relaxed),
            shape_cache_hits: self.counters.shape_cache_hits.load(Ordering::Relaxed),
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
        // A frame that replays everything looks nothing up, and swapping after it would drop
        // the layouts the frame before looked up, which the next frame that measures needs.
        if curr_frame.is_unused() {
            return;
        }
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
        item_starts: &[usize],
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        options: TextLayoutOptions,
        insets: &[TextInset],
        inline: &[InlineSpec],
        line_height: Pixels,
    ) -> Arc<WrappedLineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        // The line height only decides where inline boxes sit, so it must not take a
        // cache entry of its own when there are none.
        let line_height = if inline.is_empty() {
            Pixels::ZERO
        } else {
            line_height
        };
        // Text without a tab or a space is laid out the same whatever its tab size
        // or what happens to its spaces, so it must not take a cache entry of its
        // own. Hanging that break_spaces overrides is dropped for the same reason,
        // and so are the settings of where a line breaks when it has no wrap width.
        let has_spaces = text.as_ref().contains(hangs);
        let options = TextLayoutOptions {
            tab_size: options.tab_size.filter(|_| text.as_ref().contains('\t')),
            hang_trailing_spaces: options.spaces_hang() && has_spaces,
            break_spaces: options.break_spaces && has_spaces,
            word_break: if wrap_width.is_some() {
                options.word_break
            } else {
                WordBreak::default()
            },
            overflow_wrap: if wrap_width.is_some() {
                options.overflow_wrap
            } else {
                OverflowWrap::default()
            },
            // A clamped text keeps its greedy breaks: only part of it is laid out.
            text_wrap_style: if wrap_width.is_some() && max_lines.is_none() {
                options.text_wrap_style
            } else {
                TextWrapStyle::default()
            },
            strut_font: options.strut_font,
            nowrap: options.nowrap && wrap_width.is_some(),
        };
        let max_lines = max_lines.filter(|_| wrap_width.is_some());
        let scale_factor = self.scale_factor();
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            item_starts,
            wrap_width,
            max_lines,
            force_width: None,
            options,
            insets,
            inline,
            line_height,
            scale_bits: scale_factor.to_bits(),
        } as &dyn AsCacheKeyRef;

        let current_frame = self.current_frame.upgradable_read();
        if let Some(layout) = current_frame.wrapped_lines.get(key) {
            AtomicTextCounters::add(&self.counters.hits);
            return layout.clone();
        }

        let previous_frame_entry = self.previous_frame.lock().wrapped_lines.remove_entry(key);
        let stored = match previous_frame_entry {
            Some(_) => None,
            None => {
                let font_generation = self.font_generation();
                self.shape_store.lock().wrapped_line(key, font_generation)
            }
        };
        if let Some((key, layout)) = previous_frame_entry {
            AtomicTextCounters::add(&self.counters.hits);
            self.shape_store
                .lock()
                .keep_wrapped_line(&key, &layout, self.font_generation());
            let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
            current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            current_frame.used_wrapped_lines.push(key);
            layout
        } else if let Some((key, layout)) = stored {
            AtomicTextCounters::add(&self.counters.shape_cache_hits);
            let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
            current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            current_frame.used_wrapped_lines.push(key);
            layout
        } else {
            drop(current_frame);
            AtomicTextCounters::add(&self.counters.wrapped_layouts);
            let text = SharedString::from(text);
            let shaped_layout = self.layout_inline_line(
                &text,
                font_size,
                runs,
                inline,
                line_height,
                options.strut_font,
                !options.nowrap,
                scale_factor,
            );
            let tab_stop = options
                .tab_size
                .map(|tab_size| self.tab_stop_resolver(tab_size, font_size));
            let needs_pass = tab_stop.is_some()
                || wrap_width.is_some()
                || options.hang_trailing_spaces
                || !insets.is_empty();
            let marks = self.snap_marks(&shaped_layout, runs, item_starts);
            let (unwrapped_layout, computation) = if needs_pass {
                let computation = shaped_layout.compute_styled_wrap_boundaries(
                    text.as_ref(),
                    wrap_width,
                    max_lines,
                    options,
                    tab_stop.as_ref().map(|tab_stop| tab_stop as TabStopFor<'_>),
                    insets,
                );
                let unwrapped_layout = match &computation.tabbed_x {
                    Some(x) => Arc::new(shaped_layout.with_x_positions(x)),
                    None => shaped_layout,
                };
                (unwrapped_layout, computation)
            } else {
                let glyph_count = shaped_layout.runs.iter().map(|run| run.glyphs.len()).sum();
                (shaped_layout, WrapComputation::unbroken(glyph_count))
            };
            let layout = Arc::new(WrappedLineLayout::new(
                unwrapped_layout,
                wrap_width,
                computation,
                &marks,
            ));
            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                item_starts: item_starts.into(),
                wrap_width,
                max_lines,
                force_width: None,
                options,
                insets: insets.into(),
                inline: inline.into(),
                line_height,
                scale_bits: scale_factor.to_bits(),
            });

            self.shape_store
                .lock()
                .keep_wrapped_line(&key, &layout, self.font_generation());
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

    /// `text` shaped as `runs` say, with the atomic inlines of `inline` taking the place of
    /// their characters and every run placed on its line as `inline` says.
    fn layout_inline_line(
        &self,
        text: &SharedString,
        font_size: Pixels,
        runs: &[FontRun],
        inline: &[InlineSpec],
        line_height: Pixels,
        strut_font: Option<FontId>,
        root_wraps: bool,
        scale_factor: f32,
    ) -> Arc<LineLayout> {
        if inline.is_empty() {
            return self.layout_line::<&SharedString>(text, font_size, runs, None, strut_font);
        }
        let shaped = if inline.iter().any(|spec| spec.atomic().is_some()) {
            Arc::new(self.layout_line_with_atomics(
                text,
                font_size,
                runs,
                inline,
                strut_font,
                scale_factor,
            ))
        } else {
            self.layout_line::<&SharedString>(text, font_size, runs, None, strut_font)
        };
        let x_height = strut_font
            .or_else(|| runs.first().map(|run| run.font_id))
            .map_or(Pixels::ZERO, |font_id| {
                self.platform_text_system
                    .font_metrics(font_id)
                    .x_height(font_size)
            });
        let boxes = resolve_placements(
            inline,
            LineMetrics {
                font_size,
                ascent: shaped.ascent,
                descent: shaped.descent,
                x_height,
                scale_factor,
            },
            line_height,
            &|font_id| self.platform_text_system.font_metrics(font_id),
        );
        Arc::new(apply_inline(&shaped, inline, &boxes, root_wraps))
    }

    /// Shapes the text between the atomic inlines of `inline`, which the shaper never
    /// sees, and puts one glyph of the atomic's advance where each one is. A run of the
    /// atomic's own holds the glyph, with the extent of its box for an ascent and descent.
    fn layout_line_with_atomics(
        &self,
        text: &SharedString,
        font_size: Pixels,
        runs: &[FontRun],
        inline: &[InlineSpec],
        strut_font: Option<FontId>,
        scale_factor: f32,
    ) -> LineLayout {
        let mut layout_runs: Vec<ShapedRun> = Vec::new();
        let mut x = Pixels::ZERO;
        let mut ascent = Pixels::ZERO;
        let mut descent = Pixels::ZERO;

        let mut segment: SmallVec<[FontRun; 4]> = SmallVec::new();
        let mut segment_start = 0;
        let mut position = 0;
        let mut shape_segment = |segment: &mut SmallVec<[FontRun; 4]>,
                                 segment_start: usize,
                                 position: usize,
                                 x: &mut Pixels,
                                 layout_runs: &mut Vec<ShapedRun>,
                                 ascent: &mut Pixels,
                                 descent: &mut Pixels| {
            if segment.is_empty() {
                return;
            }
            let shaped = self.layout_line::<&SharedString>(
                &SharedString::new(&text[segment_start..position]),
                font_size,
                segment,
                None,
                strut_font,
            );
            for run in &shaped.runs {
                layout_runs.push(ShapedRun {
                    glyphs: run
                        .glyphs
                        .iter()
                        .map(|glyph| ShapedGlyph {
                            position: point(glyph.position.x + *x, glyph.position.y),
                            index: glyph.index + segment_start,
                            ..glyph.clone()
                        })
                        .collect(),
                    ..run.clone()
                });
            }
            *x += shaped.width;
            *ascent = (*ascent).max(shaped.ascent);
            *descent = (*descent).max(shaped.descent);
            segment.clear();
        };

        for run in runs {
            let end = position + run.len;
            let atomic = inline.iter().find_map(|spec| {
                spec.atomic()
                    .filter(|_| spec.range == (position..end))
            });
            match atomic {
                Some(extent) => {
                    shape_segment(
                        &mut segment,
                        segment_start,
                        position,
                        &mut x,
                        &mut layout_runs,
                        &mut ascent,
                        &mut descent,
                    );
                    let run_size = run.font_size.unwrap_or(font_size);
                    let strut = self
                        .platform_text_system
                        .font_metrics(strut_font.unwrap_or(run.font_id))
                        .line_box(font_size, scale_factor);
                    ascent = ascent.max(strut.ascent);
                    descent = descent.max(strut.descent);
                    layout_runs.push(ShapedRun {
                        font_id: run.font_id,
                        font_size: run_size,
                        ascent: extent.ascent,
                        descent: extent.descent,
                        line_height: Some(extent.ascent + extent.descent),
                        glyphs: vec![ShapedGlyph {
                            id: GlyphId(0),
                            position: point(x, Pixels::ZERO),
                            index: position,
                            is_emoji: false,
                        }],
                        inline: InlineRun::default(),
                    });
                    x += extent.advance;
                }
                None => {
                    if segment.is_empty() {
                        segment_start = position;
                    }
                    segment.push(*run);
                }
            }
            position = end;
        }
        shape_segment(
            &mut segment,
            segment_start,
            position,
            &mut x,
            &mut layout_runs,
            &mut ascent,
            &mut descent,
        );

        LineLayout {
            font_size,
            width: x,
            ascent,
            descent,
            scale_factor,
            runs: layout_runs,
            len: text.len(),
            boxes: Vec::new(),
        }
    }

    pub fn layout_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
        strut_font: Option<FontId>,
    ) -> Arc<LineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let options = TextLayoutOptions {
            strut_font,
            ..TextLayoutOptions::default()
        };
        let scale_factor = self.scale_factor();
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            item_starts: &[],
            wrap_width: None,
            max_lines: None,
            force_width,
            options,
            insets: &[],
            inline: &[],
            line_height: Pixels::ZERO,
            scale_bits: scale_factor.to_bits(),
        } as &dyn AsCacheKeyRef;

        let current_frame = self.current_frame.upgradable_read();
        if let Some(layout) = current_frame.lines.get(key) {
            AtomicTextCounters::add(&self.counters.hits);
            return layout.clone();
        }

        let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
        let previous_frame_entry = self.previous_frame.lock().lines.remove_entry(key);
        let font_generation = self.font_generation();
        let stored = match previous_frame_entry {
            Some(_) => None,
            None => self.shape_store.lock().line(key, font_generation),
        };
        if let Some((key, layout)) = previous_frame_entry {
            AtomicTextCounters::add(&self.counters.hits);
            self.shape_store
                .lock()
                .keep_line(&key, &layout, font_generation);
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        } else if let Some((key, layout)) = stored {
            AtomicTextCounters::add(&self.counters.shape_cache_hits);
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        } else {
            let text = SharedString::from(text);
            AtomicTextCounters::add(&self.counters.shapes);
            let mut layout = self
                .platform_text_system
                .layout_line(&text, font_size, runs, scale_factor);
            if let Some(strut_font) = strut_font {
                let strut = self
                    .platform_text_system
                    .font_metrics(strut_font)
                    .line_box(font_size, scale_factor);
                layout.ascent = strut.ascent;
                layout.descent = strut.descent;
            }

            if let Some(force_width) = force_width {
                apply_force_width_to_layout(&mut layout, force_width);
            }

            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                item_starts: Box::default(),
                wrap_width: None,
                max_lines: None,
                force_width,
                options,
                insets: Box::default(),
                inline: Box::default(),
                line_height: Pixels::ZERO,
                scale_bits: scale_factor.to_bits(),
            });
            let layout = Arc::new(layout);
            self.shape_store
                .lock()
                .keep_line(&key, &layout, font_generation);
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
        let scale_factor = self.scale_factor();
        let key_ref = HashedCacheKeyRef {
            text_hash,
            text_len,
            font_size,
            runs,
            wrap_width: None,
            force_width,
            scale_bits: scale_factor.to_bits(),
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
                scale_bits: key.scale_bits,
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
                scale_bits: key.scale_bits,
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
        let scale_factor = self.scale_factor();
        let key_ref = HashedCacheKeyRef {
            text_hash,
            text_len,
            font_size,
            runs,
            wrap_width: None,
            force_width,
            scale_bits: scale_factor.to_bits(),
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
                scale_bits: key.scale_bits,
            } == key_ref
        }) {
            AtomicTextCounters::add(&self.counters.hits);
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
                    scale_bits: key.scale_bits,
                } == key_ref
            })
            .cloned()
        {
            if let Some((key, layout)) = previous_frame.lines_by_hash.remove_entry(&existing_key) {
                current_frame
                    .lines_by_hash
                    .insert(key.clone(), layout.clone());
                current_frame.used_lines_by_hash.push(key);
                AtomicTextCounters::add(&self.counters.hits);
                return layout;
            }
        }

        let text = materialize_text();
        AtomicTextCounters::add(&self.counters.shapes);
        let mut layout = self
            .platform_text_system
            .layout_line(&text, font_size, runs, scale_factor);

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
            scale_bits: scale_factor.to_bits(),
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
    /// The size to shape this run at, `None` for the size of the line. Only the
    /// macOS shaper honours it, the others shape every run at the size of the line.
    pub font_size: Option<Pixels>,
    /// The line height of the run, carried through to [`ShapedRun::line_height`].
    pub line_height: Option<Pixels>,
}

impl FontRun {
    /// The line height of the run that the byte at `ix` falls in.
    pub fn line_height_at(runs: &[FontRun], ix: usize) -> Option<Pixels> {
        let mut end = 0;
        for run in runs {
            end += run.len;
            if ix < end {
                return run.line_height;
            }
        }
        panic!("byte {ix} is past the {end} bytes of the font runs");
    }
}

trait AsCacheKeyRef {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_>;
}

#[derive(Clone, Debug, Eq)]
struct CacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    /// Where the items of the line start, see [`SnapMarks`], besides where its font runs do.
    item_starts: Box<[usize]>,
    wrap_width: Option<Pixels>,
    /// How many wrapped lines the layout stops at, which changes where it ends.
    max_lines: Option<usize>,
    force_width: Option<Pixels>,
    options: TextLayoutOptions,
    insets: Box<[TextInset]>,
    inline: Box<[InlineSpec]>,
    line_height: Pixels,
    /// `f32::to_bits` of the scale factor the line is shaped for.
    scale_bits: u32,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct CacheKeyRef<'a> {
    text: &'a str,
    font_size: Pixels,
    runs: &'a [FontRun],
    item_starts: &'a [usize],
    wrap_width: Option<Pixels>,
    /// How many wrapped lines the layout stops at, which changes where it ends.
    max_lines: Option<usize>,
    force_width: Option<Pixels>,
    options: TextLayoutOptions,
    insets: &'a [TextInset],
    inline: &'a [InlineSpec],
    line_height: Pixels,
    scale_bits: u32,
}

#[derive(Clone, Debug)]
struct HashedCacheKey {
    text_hash: u64,
    text_len: usize,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
    scale_bits: u32,
}

#[derive(Copy, Clone)]
struct HashedCacheKeyRef<'a> {
    text_hash: u64,
    text_len: usize,
    font_size: Pixels,
    runs: &'a [FontRun],
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
    scale_bits: u32,
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
            && self.scale_bits == other.scale_bits
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
        self.scale_bits.hash(state);
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
            && self.scale_bits == other.scale_bits
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
        self.scale_bits.hash(state);
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
            item_starts: &self.item_starts,
            wrap_width: self.wrap_width,
            max_lines: self.max_lines,
            force_width: self.force_width,
            options: self.options,
            insets: &self.insets,
            inline: &self.inline,
            line_height: self.line_height,
            scale_bits: self.scale_bits,
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
            boxes: Vec::new(),
            font_size: px(16.),
            width: px(100.),
            ascent: px(12.),
            descent: px(4.),
            scale_factor: 1.,
            runs: vec![ShapedRun {
                font_id: FontId(0),
                font_size: px(16.),
                ascent: px(12.),
                descent: px(4.),
                line_height: None,
                glyphs,
                inline: InlineRun::default(),
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
        let computation = shaped.compute_wrap_boundaries(
            text,
            wrap_width.map(px),
            None,
            TextLayoutOptions::default(),
            Some(&tab_stop),
            &[],
        );
        WrappedLineLayout::new(
            Arc::new(shaped.with_x_positions(&computation.tabbed_x.clone().unwrap())),
            wrap_width.map(px),
            computation,
            &SnapMarks::default(),
        )
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
        let options = TextLayoutOptions::default();
        let with_stops = shaped.compute_wrap_boundaries(
            text,
            Some(px(65.)),
            None,
            options,
            Some(&tab_stop),
            &[],
        );
        let without_stops =
            shaped.compute_wrap_boundaries(text, Some(px(65.)), None, options, None, &[]);
        assert_eq!(with_stops.boundaries, without_stops.boundaries);
        assert_eq!(without_stops.tabbed_x, None);
        let shaped_x: Vec<Pixels> = shaped.runs[0]
            .glyphs
            .iter()
            .map(|glyph| glyph.position.x)
            .collect();
        assert_eq!(
            &with_stops.tabbed_x.unwrap()[..text.len()],
            shaped_x.as_slice()
        );
    }

    #[test]
    fn test_a_tab_stop_that_is_not_positive_makes_the_tab_empty() {
        let text = "a\tb";
        let layout = tabbed_line(text, None, 0.);
        assert_eq!(layout.position_for_index(2, px(20.)).unwrap().x, px(10.));
    }

    /// `text` as ten pixel wide characters, wrapped at `wrap_width`, with the
    /// spaces at the end of a line hanging or not.
    fn hanging_line(text: &str, wrap_width: Option<f32>, hang: bool) -> WrappedLineLayout {
        let options = TextLayoutOptions {
            hang_trailing_spaces: hang,
            ..Default::default()
        };
        line_with_options(text, wrap_width, options)
    }

    /// `text` as ten pixel wide characters, wrapped at `wrap_width` and laid out
    /// as `options` say.
    fn line_with_options(
        text: &str,
        wrap_width: Option<f32>,
        options: TextLayoutOptions,
    ) -> WrappedLineLayout {
        let shaped = monospace_layout(text, 10.);
        let computation =
            shaped.compute_wrap_boundaries(text, wrap_width.map(px), None, options, None, &[]);
        WrappedLineLayout::new(
            Arc::new(shaped),
            wrap_width.map(px),
            computation,
            &SnapMarks::default(),
        )
    }

    fn pixels(values: &[f32]) -> Vec<Pixels> {
        values.iter().copied().map(px).collect()
    }

    #[test]
    fn test_a_space_at_a_soft_wrap_hangs() {
        // "aaaa " is 50 wide in 45: the space overflows but does not wrap.
        let layout = hanging_line("aaaa bbbb", Some(45.), true);
        assert_eq!(boundary_indices(&layout), vec![5]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[10., 0.]));
    }

    #[test]
    fn test_spaces_do_not_hang_unless_asked_to() {
        // The space wraps by itself and then starts a line of its own.
        let layout = hanging_line("aaaa bbbb", Some(45.), false);
        assert_eq!(boundary_indices(&layout), vec![4, 5]);
        assert!(layout.hanging_widths.is_empty());
    }

    #[test]
    fn test_a_long_run_of_spaces_at_the_wrap_edge_does_not_wrap() {
        let layout = hanging_line("aaaa          bbbb", Some(50.), true);
        // One wrap, before the word, and no line of spaces.
        assert_eq!(boundary_indices(&layout), vec![14]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[100., 0.]));
        assert_eq!(layout.wrap_boundaries.len() + 1, 2);
    }

    #[test]
    fn test_the_next_line_starts_at_the_first_character_after_the_spaces() {
        let text = "aaa   bbb   ccc";
        let layout = hanging_line(text, Some(65.), true);
        assert_eq!(boundary_indices(&layout), vec![6, 12]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[30., 30., 0.]));
        // The first character of each line, which is not the boundary character
        // itself: that one is also the end of the line above.
        let columns = columns(&layout, text);
        assert_eq!(columns[7], (10., 1));
        assert_eq!(columns[13], (10., 2));
    }

    #[test]
    fn test_a_line_of_only_spaces_does_not_wrap() {
        let layout = hanging_line("          ", Some(30.), true);
        assert!(layout.wrap_boundaries.is_empty());
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[100.]));
    }

    #[test]
    fn test_a_word_that_does_not_fit_still_breaks_after_the_spaces() {
        let layout = hanging_line("ab   cdefghijkl", Some(50.), true);
        // "ab   " hangs, "cdefg" fills the line, "hijkl" is the next.
        assert_eq!(boundary_indices(&layout), vec![5, 10]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[30., 0., 0.]));
    }

    #[test]
    fn test_the_last_line_keeps_its_width_and_hangs_its_spaces() {
        let layout = hanging_line("aaa  ", Some(100.), true);
        assert!(layout.wrap_boundaries.is_empty());
        assert_eq!(layout.width(), px(50.));
        assert_eq!(layout.unwrapped_layout.width, px(50.));
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[20.]));
    }

    #[test]
    fn test_a_trailing_space_that_fills_the_line_does_not_wrap_it() {
        let layout = hanging_line("aaaa ", Some(45.), true);
        assert!(layout.wrap_boundaries.is_empty());
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[10.]));
        assert_eq!(
            hanging_line("aaaa ", Some(45.), false)
                .wrap_boundaries
                .len(),
            1
        );
    }

    #[test]
    fn test_spaces_hang_without_a_wrap_width() {
        let layout = hanging_line("ab   ", None, true);
        assert!(layout.wrap_boundaries.is_empty());
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[30.]));
    }

    #[test]
    fn test_only_spaces_and_tabs_hang() {
        // The no-break spaces are content and count: "ab" and them are 40 wide.
        let layout = hanging_line("ab\u{a0}\u{a0} cd", Some(45.), true);
        assert_eq!(boundary_indices(&layout), vec![7]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[10., 0.]));
    }

    #[test]
    fn test_a_tab_hangs_at_a_soft_wrap() {
        let text = "aaaa\tbb";
        let shaped = monospace_layout(text, 10.);
        let tab_stop = |_: FontId| Some(px(40.));
        let hanging = TextLayoutOptions {
            hang_trailing_spaces: true,
            ..Default::default()
        };
        let hung = shaped.compute_wrap_boundaries(
            text,
            Some(px(50.)),
            None,
            hanging,
            Some(&tab_stop),
            &[],
        );
        // The tab ends at 80, past the 50 wide line, but it hangs.
        assert_eq!(hung.boundaries.len(), 1);
        assert_eq!(hung.hanging_widths.to_vec(), pixels(&[40., 0.]));
        assert_eq!(hung.tabbed_x.unwrap()[5], px(80.));
        let unhung = shaped.compute_wrap_boundaries(
            text,
            Some(px(50.)),
            None,
            TextLayoutOptions::default(),
            Some(&tab_stop),
            &[],
        );
        // The tab wraps by itself and starts a line.
        assert_eq!(unhung.boundaries.len(), 2);
        assert!(unhung.hanging_widths.is_empty());
    }

    #[test]
    fn test_hit_testing_reaches_a_hanging_space_past_the_wrap_width() {
        let layout = hanging_line("aaaa bbbb", Some(45.), true);
        let line_height = px(20.);
        // The space covers 40 to 50, and the line is 45 wide.
        assert_eq!(
            layout.index_for_position(point(px(47.), px(0.)), line_height),
            Ok(4)
        );
        assert_eq!(
            layout.position_for_index(4, line_height),
            Some(point(px(40.), px(0.)))
        );
        // Past it is the end of the line, which is the start of the next.
        assert_eq!(
            layout.index_for_position(point(px(55.), px(0.)), line_height),
            Err(5)
        );
        assert_eq!(
            layout.index_for_position(point(px(5.), px(20.)), line_height),
            Ok(5)
        );
    }

    fn unbroken_words(text: &str, wrap_width: f32) -> Vec<usize> {
        let options = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            ..Default::default()
        };
        boundary_indices(&line_with_options(text, Some(wrap_width), options))
    }

    #[test]
    fn test_a_line_breaks_after_a_hyphen_and_not_before_it() {
        // Ten pixel characters: "well-" is 50 wide and the line 55.
        assert_eq!(unbroken_words("well-known", 55.), vec![5]);
        assert_eq!(unbroken_words("a-b-c", 25.), vec![2, 4]);
        // The hyphen stays on the line it ends, even when it is the character that overflows.
        assert_eq!(unbroken_words("well-known", 45.), vec![5]);
        // After a space and then a hyphen, the hyphen starts the line and the break follows it.
        assert_eq!(unbroken_words("aa -bb", 25.), vec![3, 4]);
    }

    #[test]
    fn test_the_dashes_that_end_a_word_break_after_them() {
        // Three bytes each, so `c` starts at 5.
        assert_eq!(unbroken_words("ab\u{2010}cd", 25.), vec![5]);
        assert_eq!(unbroken_words("ab\u{2013}cd", 25.), vec![5]);
        // The non-breaking hyphen is glue.
        assert_eq!(unbroken_words("ab\u{2011}cd", 25.), Vec::<usize>::new());
    }

    #[test]
    fn test_a_hyphen_breaks_before_a_digit_but_not_before_punctuation_that_ends_a_word() {
        assert_eq!(unbroken_words("12-34", 25.), vec![3]);
        assert_eq!(unbroken_words("ab-1", 25.), vec![3]);
        for text in ["ab-.cd", "ab-,cd", "ab-)", "ab-!"] {
            assert_eq!(unbroken_words(text, 25.), Vec::<usize>::new(), "{text}");
        }
        // Chrome breaks between two hyphens, as it does after one in front of a letter.
        assert_eq!(unbroken_words("ab--", 25.), vec![3]);
        // A hyphen after U+2010 is not a place to break, the letter after it is.
        assert_eq!(unbroken_words("ab\u{2010}-cd", 25.), vec![6]);
    }

    #[test]
    fn test_a_line_breaks_where_chrome_does() {
        // Ten pixel characters, in 25 pixels, so a text breaks at the first place there is
        // one after its second character.
        for (text, breaks) in [
            // Before an opening bracket only after punctuation.
            ("art(x)", vec![]),
            ("pre-(fix)", vec![4]),
            // After a hyphen in front of a quote or a symbol.
            ("art-\"x\"", vec![4]),
            ("art-%x", vec![4]),
            ("re-\u{2011}entry", vec![3]),
            // After a question mark and not after a slash.
            ("a/b?c", vec![4]),
            ("a/bcd/e", vec![]),
            // A hyphen in front of a digit follows a letter or a digit, a minus sign does not.
            ("x-1", vec![2]),
            ("$-1", vec![]),
            ("-12", vec![]),
            // Between CJK characters, and between emoji.
            ("ab\u{4E2D}\u{6587}cd", vec![2, 8]),
            ("\u{1F600}\u{1F601}\u{1F602}", vec![8]),
        ] {
            assert_eq!(unbroken_words(text, 25.), breaks, "{text}");
        }
    }

    #[test]
    fn test_a_hyphen_does_not_break_where_the_text_does_not_wrap() {
        let options = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            nowrap: true,
            ..Default::default()
        };
        let layout = line_with_options("well-known", Some(55.), options);
        assert!(boundary_indices(&layout).is_empty());
    }

    #[test]
    fn test_min_content_is_the_longest_segment_between_hyphens() {
        let text = "self-evident truths";
        let shaped = monospace_layout(text, 10.);
        let opportunities = shaped.break_opportunities(text, TextLayoutOptions::default(), &[]);
        assert_eq!(opportunities.breaks, vec![5, 13]);
    }

    fn breaking_line(text: &str, wrap_width: Option<f32>) -> WrappedLineLayout {
        let options = TextLayoutOptions {
            break_spaces: true,
            ..Default::default()
        };
        line_with_options(text, wrap_width, options)
    }

    #[test]
    fn test_a_space_that_does_not_fit_starts_the_next_line() {
        // "a bb " is 50 wide and the second of the three spaces ends at 60, in 55.
        let layout = breaking_line("a bb   c", Some(55.));
        assert_eq!(boundary_indices(&layout), vec![5]);
        // Without it the spaces stay with the word before them, which moves down.
        let layout = hanging_line("a bb   c", Some(55.), false);
        assert_eq!(boundary_indices(&layout), vec![2, 7]);
    }

    #[test]
    fn test_a_run_of_spaces_breaks_between_any_two() {
        let text = "a        b";
        let layout = breaking_line(text, Some(35.));
        assert_eq!(boundary_indices(&layout), vec![3, 6, 9]);
        // The characters after the boundary ones, which also end the line above.
        let columns = columns(&layout, text);
        assert_eq!(columns[4], (10., 1));
        assert_eq!(columns[7], (10., 2));
    }

    #[test]
    fn test_a_line_of_only_spaces_wraps() {
        let layout = breaking_line("          ", Some(30.));
        assert_eq!(boundary_indices(&layout), vec![3, 6, 9]);
        assert!(layout.hanging_widths.is_empty());
    }

    #[test]
    fn test_spaces_break_after_leading_spaces_too() {
        let layout = breaking_line("   aaaa", Some(55.));
        assert_eq!(boundary_indices(&layout), vec![3]);
        // Otherwise the word has no earlier opportunity, and breaks inside.
        let layout = hanging_line("   aaaa", Some(55.), false);
        assert_eq!(boundary_indices(&layout), vec![5]);
    }

    #[test]
    fn test_spaces_that_break_are_measured_at_the_end_of_a_line() {
        let layout = breaking_line("aaa  ", Some(100.));
        assert!(layout.wrap_boundaries.is_empty());
        assert!(layout.hanging_widths.is_empty());
        assert_eq!(layout.width(), px(50.));
        // A space past the edge wraps instead of hanging over it.
        let layout = breaking_line("aaa    bbb", Some(45.));
        assert_eq!(boundary_indices(&layout), vec![4, 7]);
        assert!(layout.hanging_widths.is_empty());
    }

    #[test]
    fn test_a_word_that_does_not_fit_still_breaks_inside_when_spaces_break() {
        let layout = breaking_line("ab cdefghijkl", Some(50.));
        // "ab " then "cdefg" then "hijkl".
        assert_eq!(boundary_indices(&layout), vec![3, 8]);
    }

    #[test]
    fn test_a_tab_breaks_like_a_space_when_spaces_break() {
        let text = "aaaa\tbb";
        let shaped = monospace_layout(text, 10.);
        let tab_stop = |_: FontId| Some(px(40.));
        let options = TextLayoutOptions {
            break_spaces: true,
            ..Default::default()
        };
        let broken = shaped.compute_wrap_boundaries(
            text,
            Some(px(50.)),
            None,
            options,
            Some(&tab_stop),
            &[],
        );
        // The tab ends at 80, past the line, so it begins the next one, and
        // neither line hangs it.
        assert_eq!(broken.boundaries.len(), 2);
        assert!(broken.hanging_widths.is_empty());
        let tab_after = line_with_options("aa\tbbbb", Some(45.), options);
        assert_eq!(boundary_indices(&tab_after), vec![3]);
    }

    #[test]
    fn test_breaking_spaces_changes_nothing_in_text_without_spaces() {
        for text in ["aaaabbbbcc", "aaaa-bbbb-cc"] {
            let plain = hanging_line(text, Some(45.), false);
            let breaking = breaking_line(text, Some(45.));
            assert_eq!(plain.wrap_boundaries, breaking.wrap_boundaries, "{text}");
        }
    }

    #[test]
    fn test_breaking_spaces_overrides_hanging_them() {
        let options = TextLayoutOptions {
            hang_trailing_spaces: true,
            break_spaces: true,
            ..Default::default()
        };
        assert!(!options.spaces_hang());
        let both = line_with_options("aaaa bbbb", Some(45.), options);
        let breaking = breaking_line("aaaa bbbb", Some(45.));
        assert_eq!(both.wrap_boundaries, breaking.wrap_boundaries);
        assert!(both.hanging_widths.is_empty());
        assert!(
            TextLayoutOptions {
                hang_trailing_spaces: true,
                ..Default::default()
            }
            .spaces_hang()
        );
    }

    fn overflow_line(
        text: &str,
        wrap_width: f32,
        overflow_wrap: OverflowWrap,
    ) -> WrappedLineLayout {
        let options = TextLayoutOptions {
            overflow_wrap,
            ..Default::default()
        };
        line_with_options(text, Some(wrap_width), options)
    }

    /// The character, not byte, index each wrap boundary is at.
    fn boundary_chars(layout: &WrappedLineLayout, text: &str) -> Vec<usize> {
        boundary_indices(layout)
            .into_iter()
            .map(|byte| text[..byte].chars().count())
            .collect()
    }

    #[test]
    fn test_the_default_options_break_a_word_that_does_not_fit() {
        assert_eq!(OverflowWrap::default(), OverflowWrap::BreakWord);
        // What this layout did before there was a choice: a word with no earlier
        // place to break is cut where it overflows, and one after a space is not.
        let layout = line_with_options("aaaaaaaa bbbb", Some(35.), TextLayoutOptions::default());
        assert_eq!(boundary_indices(&layout), vec![3, 6, 9, 12]);
        let layout = line_with_options("aa bbbbbbbb", Some(55.), TextLayoutOptions::default());
        assert_eq!(boundary_indices(&layout), vec![3, 8]);
        let layout = line_with_options("中中中中 中中", Some(35.), TextLayoutOptions::default());
        assert_eq!(boundary_chars(&layout, "中中中中 中中"), vec![3, 6]);
    }

    #[test]
    fn test_normal_overflow_wrap_keeps_a_long_word_whole() {
        let text = "aaaaaaaa bbbb";
        let layout = overflow_line(text, 35., OverflowWrap::Normal);
        // "aaaaaaaa " overflows its line, and "bbbb" its own.
        assert_eq!(boundary_indices(&layout), vec![9]);
        let columns = columns(&layout, text);
        assert_eq!(columns[7], (70., 0));
        assert_eq!(columns[10], (10., 1));
        // A word alone on the line overflows it, however long.
        let layout = overflow_line("aaaaaaaaaaaa", 35., OverflowWrap::Normal);
        assert!(layout.wrap_boundaries.is_empty());
        assert_eq!(layout.unwrapped_layout.width, px(120.));
    }

    #[test]
    fn test_normal_overflow_wrap_still_breaks_where_a_word_can_start() {
        let layout = overflow_line("aa bb cc dd", 45., OverflowWrap::Normal);
        assert_eq!(boundary_indices(&layout), vec![3, 6, 9]);
        // After overflowing, the line breaks at the next place there is one.
        let layout = overflow_line("aaaaaaaa bb cc", 35., OverflowWrap::Normal);
        assert_eq!(boundary_indices(&layout), vec![9, 12]);
    }

    #[test]
    fn test_break_word_and_anywhere_break_a_word_only_without_an_earlier_break() {
        for overflow_wrap in [OverflowWrap::BreakWord, OverflowWrap::Anywhere] {
            // "bbbbbbbb" fits after the space on a line of its own, so it breaks
            // there and not inside "aa bbb".
            let layout = overflow_line("aa bbbbbbbb", 55., overflow_wrap);
            assert_eq!(boundary_indices(&layout), vec![3, 8], "{overflow_wrap:?}");
            // With no earlier place the word breaks where it overflows.
            let layout = overflow_line("aaaaaaaa", 35., overflow_wrap);
            assert_eq!(boundary_indices(&layout), vec![3, 6], "{overflow_wrap:?}");
        }
    }

    #[test]
    fn test_overflowing_words_when_spaces_break_leave_no_line_of_one_space() {
        // "aaaa " does not fit in 45, and the space, which is the only place to
        // break, does not start a line of its own.
        let options = TextLayoutOptions {
            break_spaces: true,
            overflow_wrap: OverflowWrap::Normal,
            ..Default::default()
        };
        let layout = line_with_options("aaaa bbbb", Some(45.), options);
        assert_eq!(boundary_indices(&layout), vec![5]);
        let layout = line_with_options("aaaa bbbb", Some(35.), options);
        assert_eq!(boundary_indices(&layout), vec![5]);
        // A run of spaces still breaks between any two of them.
        let layout = line_with_options("aaaa    bbbb", Some(65.), options);
        assert_eq!(boundary_indices(&layout), vec![6]);
        // A word that is broken inside puts the space on its own line.
        let broken = TextLayoutOptions {
            break_spaces: true,
            ..Default::default()
        };
        let layout = line_with_options("aaaa bbbb", Some(45.), broken);
        assert_eq!(boundary_indices(&layout), vec![4, 5]);
    }

    #[test]
    fn test_overflow_wrap_normal_with_a_line_clamp_stops_at_the_last_line() {
        let text = "aa bbbbbbbbbbbb cc dd";
        let shaped = monospace_layout(text, 10.);
        let options = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            ..Default::default()
        };
        let computation =
            shaped.compute_wrap_boundaries(text, Some(px(35.)), Some(2), options, None, &[]);
        assert_eq!(computation.boundaries.len(), 1);
    }

    fn word_break_line(
        text: &str,
        wrap_width: f32,
        word_break: WordBreak,
        overflow_wrap: OverflowWrap,
    ) -> WrappedLineLayout {
        let options = TextLayoutOptions {
            word_break,
            overflow_wrap,
            ..Default::default()
        };
        line_with_options(text, Some(wrap_width), options)
    }

    #[test]
    fn test_the_default_word_break_is_normal() {
        assert_eq!(WordBreak::default(), WordBreak::Normal);
        let layout = word_break_line("aaaa bbbb", 65., WordBreak::Normal, OverflowWrap::BreakWord);
        let default = line_with_options("aaaa bbbb", Some(65.), TextLayoutOptions::default());
        assert_eq!(layout.wrap_boundaries, default.wrap_boundaries);
    }

    #[test]
    fn test_break_all_fills_the_line_and_breaks_mid_word() {
        let text = "aaaa bbbb";
        // A space is a place to break, but the line holds one more letter.
        let normal = word_break_line(text, 65., WordBreak::Normal, OverflowWrap::Normal);
        assert_eq!(boundary_indices(&normal), vec![5]);
        let all = word_break_line(text, 65., WordBreak::BreakAll, OverflowWrap::Normal);
        assert_eq!(boundary_indices(&all), vec![6]);
        // It breaks whatever overflow-wrap says.
        let all = word_break_line("aaaaaaaa", 35., WordBreak::BreakAll, OverflowWrap::Normal);
        assert_eq!(boundary_indices(&all), vec![3, 6]);
    }

    #[test]
    fn test_break_all_leaves_glue_closing_punctuation_and_spaces_alone() {
        // ")" cannot start a line, so "b)" moves down together.
        let layout = word_break_line("ab)cd", 25., WordBreak::BreakAll, OverflowWrap::Normal);
        assert_eq!(boundary_indices(&layout), vec![1, 3]);
        // Nor can a line break next to a no-break space.
        let glued = "aa\u{00A0}bb";
        let layout = word_break_line(glued, 35., WordBreak::BreakAll, OverflowWrap::Normal);
        assert_eq!(boundary_chars(&layout, glued), vec![1, 4]);
        // And a space hangs where it overflows, as it does without break-all.
        let options = TextLayoutOptions {
            word_break: WordBreak::BreakAll,
            overflow_wrap: OverflowWrap::Normal,
            hang_trailing_spaces: true,
            ..Default::default()
        };
        let layout = line_with_options("aaaa bbbb", Some(45.), options);
        assert_eq!(boundary_indices(&layout), vec![5]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[10., 0.]));
    }

    #[test]
    fn test_keep_all_keeps_a_run_of_cjk_together() {
        let text = "中中中中 中中";
        let normal = word_break_line(text, 35., WordBreak::Normal, OverflowWrap::Normal);
        assert_eq!(boundary_chars(&normal, text), vec![3, 6]);
        let kept = word_break_line(text, 35., WordBreak::KeepAll, OverflowWrap::Normal);
        // The word overflows, and the break is at the space after it.
        assert_eq!(boundary_chars(&kept, text), vec![5]);
        // The run still breaks at a space, and inside only if overflow-wrap says so.
        let broken = word_break_line(text, 35., WordBreak::KeepAll, OverflowWrap::BreakWord);
        assert_eq!(boundary_chars(&broken, text), vec![3, 5]);
        // Punctuation that is not CJK is still a place to break: here after the slash, as in Chrome.
        let slash = "中中/中中";
        let kept = word_break_line(slash, 35., WordBreak::KeepAll, OverflowWrap::Normal);
        assert_eq!(boundary_chars(&kept, slash), vec![3]);
    }

    fn inset(range: Range<usize>, leading: f32, trailing: f32) -> TextInset {
        TextInset {
            range,
            leading: px(leading),
            trailing: px(trailing),
        }
    }

    /// `text` as ten pixel wide characters, laid out with `insets`, a tab stop
    /// every `tab_stop` pixels if there is one, and as `options` say.
    fn inset_line_with(
        text: &str,
        wrap_width: Option<f32>,
        options: TextLayoutOptions,
        tab_stop: Option<f32>,
        insets: &[TextInset],
    ) -> WrappedLineLayout {
        let shaped = monospace_layout(text, 10.);
        let resolver = tab_stop.map(|stop| move |_: FontId| Some(px(stop)));
        let computation = shaped.compute_wrap_boundaries(
            text,
            wrap_width.map(px),
            None,
            options,
            resolver.as_ref().map(|resolver| resolver as TabStopFor<'_>),
            insets,
        );
        WrappedLineLayout::new(
            Arc::new(match &computation.tabbed_x {
                Some(x) => shaped.with_x_positions(x),
                None => shaped,
            }),
            wrap_width.map(px),
            computation,
            &SnapMarks::default(),
        )
    }

    fn inset_line(text: &str, wrap_width: Option<f32>, insets: &[TextInset]) -> WrappedLineLayout {
        inset_line_with(text, wrap_width, TextLayoutOptions::default(), None, insets)
    }

    /// The x of each glyph and then of the end of the line.
    fn x_and_width(layout: &WrappedLineLayout) -> Vec<f32> {
        let mut x = glyph_x_positions(&layout.unwrapped_layout);
        x.push(f32::from(layout.unwrapped_layout.width));
        x
    }

    #[test]
    fn test_no_insets_lay_out_as_before() {
        let tab_stop = |_: FontId| Some(px(40.));
        let texts = ["aaaa bbbb", "ab\tcd ef", "a  b", "x", ""];
        let option_sets = [
            TextLayoutOptions::default(),
            TextLayoutOptions {
                hang_trailing_spaces: true,
                ..Default::default()
            },
            TextLayoutOptions {
                break_spaces: true,
                word_break: WordBreak::BreakAll,
                ..Default::default()
            },
        ];
        for text in texts {
            for options in option_sets {
                for wrap_width in [None, Some(25.), Some(65.)] {
                    for stop in [None, Some(&tab_stop as TabStopFor<'_>)] {
                        let shaped = monospace_layout(text, 10.);
                        let wrap_width = wrap_width.map(px);
                        let plain = shaped.compute_wrap_boundaries(
                            text,
                            wrap_width,
                            None,
                            options,
                            stop,
                            &[],
                        );
                        assert!(plain.glyph_insets.is_empty());
                        assert_eq!(plain.tabbed_x.is_some(), stop.is_some());

                        // Insets that add nothing change nothing.
                        let zero = [inset(0..text.len(), 0., 0.)];
                        let inset = shaped
                            .compute_wrap_boundaries(text, wrap_width, None, options, stop, &zero);
                        assert_eq!(inset.boundaries, plain.boundaries, "{text:?} {options:?}");
                        assert_eq!(inset.hanging_widths, plain.hanging_widths);
                        let shaped_x: Vec<Pixels> = shaped.runs[0]
                            .glyphs
                            .iter()
                            .map(|glyph| glyph.position.x)
                            .chain([shaped.width])
                            .collect();
                        let expected = plain.tabbed_x.unwrap_or(shaped_x);
                        if !text.is_empty() {
                            assert_eq!(inset.tabbed_x.unwrap(), expected);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_an_inset_widens_the_line_and_pushes_the_glyphs_after_it() {
        // "cd" is inset: 5 before c, 7 after d.
        let layout = inset_line("abcdef", None, &[inset(2..4, 5., 7.)]);
        assert_eq!(x_and_width(&layout), vec![0., 10., 20., 35., 52., 62., 72.]);
        assert_eq!(layout.width(), px(72.));
        assert_eq!(
            layout.insets,
            vec![
                GlyphInset {
                    glyph_ix: 2,
                    index: 2,
                    leading: px(5.),
                    trailing: px(0.)
                },
                GlyphInset {
                    glyph_ix: 3,
                    index: 3,
                    leading: px(0.),
                    trailing: px(7.)
                },
            ]
        );
    }

    #[test]
    fn test_an_inset_on_one_glyph_has_both_ends() {
        let layout = inset_line("abc", None, &[inset(1..2, 6., 4.)]);
        assert_eq!(x_and_width(&layout), vec![0., 10., 30., 40.]);
        assert_eq!(layout.insets.len(), 1);
        assert_eq!(layout.insets[0].leading, px(6.));
        assert_eq!(layout.insets[0].trailing, px(4.));
    }

    #[test]
    fn test_x_and_index_agree_across_an_inset() {
        let layout = inset_line("abcdef", None, &[inset(2..4, 5., 7.)]);
        let line = &layout.unwrapped_layout;
        for (index, x) in [(0, 0.), (1, 10.), (2, 20.), (3, 35.), (4, 52.), (5, 62.)] {
            assert_eq!(line.x_for_index(index), px(x));
            assert_eq!(line.index_for_x(px(x)), Some(index));
            assert_eq!(line.closest_index_for_x(px(x)), index);
        }
        assert_eq!(line.x_for_index(6), px(72.));
        // The cell of c holds its leading inset, and d's its trailing one.
        assert_eq!(line.index_for_x(px(34.9)), Some(2));
        assert_eq!(line.index_for_x(px(51.9)), Some(3));
        assert_eq!(line.index_for_x(px(72.)), None);
        // Between c at 20 and d at 35: the middle is 27.5.
        assert_eq!(line.closest_index_for_x(px(27.)), 2);
        assert_eq!(line.closest_index_for_x(px(28.)), 3);
    }

    #[test]
    fn test_hit_testing_at_both_edges_of_an_inset_glyph() {
        // b is 20 wide: 6 of leading inset, the glyph, and 4 of trailing inset.
        let layout = inset_line("abc", None, &[inset(1..2, 6., 4.)]);
        let line_height = px(20.);
        let at = |x: f32| point(px(x), px(5.));
        assert_eq!(layout.index_for_position(at(9.9), line_height), Ok(0));
        assert_eq!(layout.index_for_position(at(10.), line_height), Ok(1));
        assert_eq!(layout.index_for_position(at(29.9), line_height), Ok(1));
        assert_eq!(layout.index_for_position(at(30.), line_height), Ok(2));
        assert_eq!(
            layout.closest_index_for_position(at(19.), line_height),
            Ok(1)
        );
        assert_eq!(
            layout.closest_index_for_position(at(21.), line_height),
            Ok(2)
        );
        assert_eq!(
            layout.position_for_index(1, line_height).unwrap().x,
            px(10.)
        );
        assert_eq!(
            layout.position_for_index(2, line_height).unwrap().x,
            px(30.)
        );
    }

    #[test]
    fn test_nested_and_adjacent_insets_add() {
        let layout = inset_line(
            "abcdefgh",
            None,
            &[
                inset(0..8, 3., 3.),
                inset(2..4, 5., 5.),
                // Two insets that start on the same glyph.
                inset(4..6, 1., 2.),
                inset(4..5, 3., 0.),
            ],
        );
        assert_eq!(
            x_and_width(&layout),
            vec![0., 13., 23., 38., 53., 67., 79., 89., 102.]
        );
        let at_four = layout.insets.iter().find(|inset| inset.index == 4).unwrap();
        assert_eq!((at_four.leading, at_four.trailing), (px(4.), px(0.)));
        // Adjacent: the trailing inset of one and the leading inset of the next
        // are both between their glyphs.
        let adjacent = inset_line("abcd", None, &[inset(0..2, 0., 4.), inset(2..4, 6., 0.)]);
        assert_eq!(x_and_width(&adjacent), vec![0., 10., 24., 40., 50.]);
    }

    #[test]
    fn test_insets_are_given_in_any_order() {
        let inner_first = [inset(2..4, 5., 5.), inset(0..8, 3., 3.)];
        let outer_first = [inset(0..8, 3., 3.), inset(2..4, 5., 5.)];
        assert_eq!(
            x_and_width(&inset_line("abcdefgh", None, &inner_first)),
            x_and_width(&inset_line("abcdefgh", None, &outer_first)),
        );
    }

    #[test]
    fn test_an_inset_with_no_glyph_adds_nothing() {
        let layout = inset_line("abc", None, &[inset(1..1, 6., 4.), inset(5..9, 6., 4.)]);
        assert_eq!(x_and_width(&layout), vec![0., 10., 20., 30.]);
        assert!(layout.insets.is_empty());
    }

    #[test]
    fn test_a_break_before_the_span_keeps_its_leading_inset_on_the_new_line() {
        let text = "aaaa bbbb";
        let line_height = px(20.);
        let layout = inset_line(text, Some(55.), &[inset(5..9, 10., 0.)]);
        assert_eq!(boundary_indices(&layout), vec![5]);
        // The first line ends where the span starts, without the inset...
        assert_eq!(
            layout.position_for_index(5, line_height).unwrap().x,
            px(50.)
        );
        assert_eq!(layout.unwrapped_layout.x_for_index(5), px(50.));
        // ...which is at the start of the second, before the first glyph.
        let line = &layout.unwrapped_layout;
        assert_eq!(
            layout.position_for_index(6, line_height),
            Some(point(px(20.), line_height))
        );
        assert_eq!(line.width, px(100.));
        assert_eq!(layout.insets[0].index, 5);
        assert_eq!(
            layout.index_for_position(point(px(5.), line_height + px(1.)), line_height),
            Ok(5)
        );
    }

    #[test]
    fn test_a_leading_inset_alone_does_not_end_a_line() {
        // "aaaa " fits in 55, and so would "aaaa" with b's inset, but the inset
        // belongs to b and goes where b goes.
        let layout = inset_line("aaaa bbbb", Some(55.), &[inset(5..9, 10., 0.)]);
        assert_eq!(boundary_indices(&layout), vec![5]);
        let layout = inset_line("aaaa bbbb", Some(65.), &[inset(5..9, 10., 0.)]);
        assert_eq!(boundary_indices(&layout), vec![5]);
        // Without the inset the same width keeps the line whole.
        let layout = inset_line("aaaa bbbb", Some(95.), &[]);
        assert!(boundary_indices(&layout).is_empty());
    }

    #[test]
    fn test_a_trailing_inset_that_does_not_fit_wraps_the_last_word() {
        let text = "aaa bbb";
        let line_height = px(20.);
        // 70 wide in 75, but 80 with the closing padding.
        let plain = inset_line(text, Some(75.), &[]);
        assert!(boundary_indices(&plain).is_empty());
        let layout = inset_line(text, Some(75.), &[inset(4..7, 0., 10.)]);
        assert_eq!(boundary_indices(&layout), vec![4]);
        // The padding stays after the word, on the second line.
        assert_eq!(
            layout.position_for_index(text.len(), line_height),
            Some(point(px(40.), line_height))
        );
        // And the first line is not wider for it.
        assert_eq!(
            layout.position_for_index(4, line_height).unwrap().x,
            px(40.)
        );
    }

    #[test]
    fn test_an_inset_word_that_fits_nowhere_overflows_or_breaks_like_any_word() {
        let text = "bbbbbbb";
        let insets = [inset(0..7, 5., 5.)];
        let breaking = inset_line(text, Some(40.), &insets);
        // 0, 15, 25, 35, 45: the fourth glyph overflows 40, and then the seventh.
        assert_eq!(boundary_indices(&breaking), vec![3, 6]);
        let overflowing = inset_line_with(
            text,
            Some(40.),
            TextLayoutOptions {
                overflow_wrap: OverflowWrap::Normal,
                ..Default::default()
            },
            None,
            &insets,
        );
        assert!(boundary_indices(&overflowing).is_empty());
        assert_eq!(overflowing.unwrapped_layout.width, px(80.));
    }

    #[test]
    fn test_a_tab_after_an_inset_goes_to_the_stop_from_the_line_start() {
        // a and b take 10 + 5 + 10 + 5 = 30; the tab goes on to 40.
        let layout = inset_line_with(
            "ab\tc",
            None,
            TextLayoutOptions::default(),
            Some(40.),
            &[inset(0..2, 5., 5.)],
        );
        assert_eq!(x_and_width(&layout), vec![0., 15., 30., 40., 50.]);
    }

    #[test]
    fn test_a_tab_with_an_inset_starts_after_its_leading_inset() {
        // The tab starts at 25 and ends on the stop at 40, then its 5 of
        // trailing inset.
        let layout = inset_line_with(
            "ab\tc",
            None,
            TextLayoutOptions::default(),
            Some(40.),
            &[inset(2..3, 5., 5.)],
        );
        assert_eq!(x_and_width(&layout), vec![0., 10., 20., 45., 55.]);
    }

    #[test]
    fn test_tab_stops_count_from_the_wrapped_line_with_insets() {
        // "aaaa" fills the first line; the second starts at b, whose inset moves
        // it 6 right, and the tab after it goes to 40 from the line start.
        let text = "aaaa b\tc";
        let layout = inset_line_with(
            text,
            Some(55.),
            TextLayoutOptions::default(),
            Some(40.),
            &[inset(5..6, 6., 0.)],
        );
        assert_eq!(boundary_indices(&layout), vec![5]);
        let line_height = px(20.);
        assert_eq!(
            layout.position_for_index(6, line_height),
            Some(point(px(16.), line_height))
        );
        assert_eq!(
            layout.position_for_index(7, line_height),
            Some(point(px(40.), line_height))
        );
    }

    #[test]
    fn test_a_hanging_space_hangs_with_its_trailing_inset() {
        let options = TextLayoutOptions {
            hang_trailing_spaces: true,
            ..Default::default()
        };
        let layout = inset_line_with(
            "aaaa bbbb",
            Some(45.),
            options,
            None,
            &[inset(4..5, 0., 10.)],
        );
        assert_eq!(boundary_indices(&layout), vec![5]);
        assert_eq!(layout.hanging_widths.to_vec(), pixels(&[20., 0.]));
    }

    #[test]
    fn test_insets_on_either_side_of_a_wrap_each_keep_their_end() {
        // The span covers all of "bb cc" and wraps after the space. Its leading
        // inset is before "bb", its trailing inset after "cc".
        let text = "bb cc";
        let line_height = px(20.);
        let layout = inset_line(text, Some(45.), &[inset(0..5, 8., 6.)]);
        assert_eq!(boundary_indices(&layout), vec![3]);
        let position = |index| layout.position_for_index(index, line_height).unwrap();
        assert_eq!(position(1), point(px(18.), px(0.)));
        assert_eq!(position(3), point(px(38.), px(0.)));
        assert_eq!(position(4), point(px(10.), line_height));
        assert_eq!(position(5), point(px(26.), line_height));
    }

    mod text_wrap_style {
        use super::*;

        /// `text` as ten pixel wide characters, wrapped at `wrap_width` the way
        /// `layout_wrapped_line` does, with the spaces at the end of a line hanging.
        fn styled(
            text: &str,
            wrap_width: f32,
            style: TextWrapStyle,
            max_lines: Option<usize>,
        ) -> (WrappedLineLayout, WrapComputation) {
            let options = TextLayoutOptions {
                hang_trailing_spaces: true,
                text_wrap_style: style,
                ..Default::default()
            };
            let shaped = monospace_layout(text, 10.);
            let computation = shaped.compute_styled_wrap_boundaries(
                text,
                Some(px(wrap_width)),
                max_lines,
                options,
                None,
                &[],
            );
            let layout = WrappedLineLayout::new(
                Arc::new(shaped),
                Some(px(wrap_width)),
                computation.clone(),
                &SnapMarks::default(),
            );
            (layout, computation)
        }

        fn breaks(text: &str, wrap_width: f32, style: TextWrapStyle) -> Vec<usize> {
            boundary_indices(&styled(text, wrap_width, style, None).0)
        }

        fn widths(text: &str, wrap_width: f32, style: TextWrapStyle) -> Vec<f32> {
            styled(text, wrap_width, style, None)
                .0
                .line_widths()
                .map(f32::from)
                .collect()
        }

        #[test]
        fn test_auto_and_the_default_are_greedy() {
            let text = "aaaa bbbb cccc dddd eeeeeeee";
            assert_eq!(TextWrapStyle::default(), TextWrapStyle::Auto);
            assert_eq!(breaks(text, 200., TextWrapStyle::Auto), vec![20]);
        }

        #[test]
        fn test_balance_evens_out_the_lines_without_adding_one() {
            let text = "aaaa bbbb cccc dddd eeeeeeee";
            // Greedy: 19 characters, then 8. Balanced: 14, then 13.
            assert_eq!(breaks(text, 200., TextWrapStyle::Auto), vec![20]);
            assert_eq!(breaks(text, 200., TextWrapStyle::Balance), vec![15]);
            assert_eq!(widths(text, 200., TextWrapStyle::Balance), vec![140., 130.]);
        }

        #[test]
        fn test_balance_evens_out_lines_that_no_narrower_width_would_move() {
            // Greedy fills three lines of four words, so a width that keeps four
            // lines is as wide as the first. The breaks still move, 3, 3, 3, 4.
            let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mm";
            assert_eq!(breaks(text, 200., TextWrapStyle::Auto), vec![20, 40, 60]);
            assert_eq!(breaks(text, 200., TextWrapStyle::Balance), vec![15, 30, 45]);
            assert_eq!(
                widths(text, 200., TextWrapStyle::Balance),
                vec![140., 140., 140., 170.]
            );
        }

        #[test]
        fn test_balance_counts_the_insets_of_a_line() {
            // The inset makes "cccc" 30 wider, which the lines are measured with.
            let text = "aaaa bbbb cccc dddd eeee ffff";
            let shaped = monospace_layout(text, 10.);
            let insets = [inset(10..14, 15., 15.)];
            let options = TextLayoutOptions {
                hang_trailing_spaces: true,
                text_wrap_style: TextWrapStyle::Balance,
                ..Default::default()
            };
            let computation = shaped.compute_styled_wrap_boundaries(
                text,
                Some(px(200.)),
                None,
                options,
                None,
                &insets,
            );
            let layout = WrappedLineLayout::new(
                Arc::new(shaped.with_x_positions(&computation.tabbed_x.clone().unwrap())),
                Some(px(200.)),
                computation,
                &SnapMarks::default(),
            );
            let widths: Vec<_> = layout.line_widths().map(f32::from).collect();
            assert_eq!(widths.len(), 2);
            assert!(widths.iter().all(|width| *width <= 200.), "{widths:?}");
            assert!((widths[0] - widths[1]).abs() <= 50., "{widths:?}");
        }

        #[test]
        fn test_balance_keeps_the_reported_width() {
            let text = "aaaa bbbb cccc dddd eeeeeeee";
            let (layout, _) = styled(text, 200., TextWrapStyle::Balance, None);
            assert_eq!(layout.wrap_width, Some(px(200.)));
            assert_eq!(layout.width(), px(200.));
        }

        #[test]
        fn test_balance_hangs_the_spaces_of_the_lines_it_chose() {
            let text = "aaaa bbbb cccc dddd eeeeeeee";
            let (layout, computation) = styled(text, 200., TextWrapStyle::Balance, None);
            assert_eq!(computation.hanging_widths.to_vec(), pixels(&[10., 0.]));
            // The hanging space is not part of the line as it is aligned.
            assert_eq!(
                layout.line_widths().collect::<Vec<_>>(),
                pixels(&[140., 130.])
            );
        }

        #[test]
        fn test_balance_does_not_shrink_a_line_below_its_widest_word() {
            // The long word takes 100, and balancing at less would overflow it.
            let text = "aa bb cc eeeeeeeeee";
            let overflowing = |style| TextLayoutOptions {
                hang_trailing_spaces: true,
                overflow_wrap: OverflowWrap::Normal,
                text_wrap_style: style,
                ..Default::default()
            };
            let shaped = monospace_layout(text, 10.);
            let wrap = |style| {
                shaped.compute_styled_wrap_boundaries(
                    text,
                    Some(px(150.)),
                    None,
                    overflowing(style),
                    None,
                    &[],
                )
            };
            let balanced = wrap(TextWrapStyle::Balance);
            assert_eq!(balanced.boundaries, wrap(TextWrapStyle::Auto).boundaries);
            assert_eq!(balanced.line_widths.to_vec(), pixels(&[80., 100.]));
        }

        #[test]
        fn test_balance_leaves_a_paragraph_of_more_than_six_lines_greedy() {
            // Seven lines of one word each.
            let text = "aaaaaa bbbbbb cccccc dddddd eeeeee ffffff gggggg";
            assert_eq!(widths(text, 60., TextWrapStyle::Auto).len(), 7);
            assert_eq!(
                breaks(text, 60., TextWrapStyle::Balance),
                breaks(text, 60., TextWrapStyle::Auto)
            );
        }

        #[test]
        fn test_balance_balances_six_lines() {
            let text = "aaaaaa bbbbbb cccccc dddddd eeeeee ffffff";
            assert_eq!(widths(text, 100., TextWrapStyle::Auto).len(), 6);
            let balanced = widths(text, 100., TextWrapStyle::Balance);
            assert_eq!(balanced.len(), 6);
            assert!(balanced.iter().all(|width| *width <= 60.));
        }

        #[test]
        fn test_balance_leaves_an_overflowing_word_alone() {
            let text = "aaaa bbbbbbbbbbbbbbbbbbbbbbbbb";
            assert_eq!(
                breaks(text, 100., TextWrapStyle::Balance),
                breaks(text, 100., TextWrapStyle::Auto)
            );
        }

        #[test]
        fn test_balance_is_not_applied_to_a_clamped_text_a_tab_or_break_spaces() {
            let text = "aaaa bbbb cccc dddd eeeeeeee";
            let (clamped, _) = styled(text, 200., TextWrapStyle::Balance, Some(3));
            assert_eq!(boundary_indices(&clamped), vec![20]);

            let tabbed = "aaaa bbbb cccc dddd\teeeeeeee";
            assert_eq!(
                breaks(tabbed, 200., TextWrapStyle::Balance),
                breaks(tabbed, 200., TextWrapStyle::Auto)
            );

            let shaped = monospace_layout(text, 10.);
            let spaces = shaped.compute_styled_wrap_boundaries(
                text,
                Some(px(200.)),
                None,
                TextLayoutOptions {
                    break_spaces: true,
                    text_wrap_style: TextWrapStyle::Balance,
                    ..Default::default()
                },
                None,
                &[],
            );
            let greedy = shaped.compute_wrap_boundaries(
                text,
                Some(px(200.)),
                None,
                TextLayoutOptions {
                    break_spaces: true,
                    ..Default::default()
                },
                None,
                &[],
            );
            assert_eq!(spaces.boundaries, greedy.boundaries);
        }

        #[test]
        fn test_pretty_gives_the_last_line_a_second_word() {
            let text = "aaaa bbbb cccc dddd ee";
            assert_eq!(breaks(text, 200., TextWrapStyle::Auto), vec![20]);
            // "dddd" joins "ee", and the paragraph still has two lines.
            assert_eq!(breaks(text, 200., TextWrapStyle::Pretty), vec![15]);
            assert_eq!(widths(text, 200., TextWrapStyle::Pretty), vec![140., 70.]);
        }

        #[test]
        fn test_pretty_moves_as_little_as_it_takes() {
            // Only "dddd" has to move to the last line.
            let text = "aaaa bbbb cccc dddd ee";
            let (_, computation) = styled(text, 200., TextWrapStyle::Pretty, None);
            assert_eq!(computation.boundaries.len(), 1);
            assert_eq!(computation.last_line_words, 2);
        }

        #[test]
        fn test_pretty_leaves_a_last_line_of_two_words_or_a_long_one() {
            // Two words on the last line.
            let text = "aaaa bbbb cccc dddd e f";
            assert_eq!(
                breaks(text, 200., TextWrapStyle::Pretty),
                breaks(text, 200., TextWrapStyle::Auto)
            );
            // One word, but at least a third of the line.
            let text = "aaaa bbbb cccc dddd eeeeeeee";
            assert_eq!(
                breaks(text, 200., TextWrapStyle::Pretty),
                breaks(text, 200., TextWrapStyle::Auto)
            );
        }

        #[test]
        fn test_pretty_never_adds_a_line() {
            // Giving "ee" a second word would take the first line's only word.
            let text = "xxxxxxxxxxxxxxxxxxx ee";
            assert_eq!(
                breaks(text, 200., TextWrapStyle::Pretty),
                breaks(text, 200., TextWrapStyle::Auto)
            );
        }

        #[test]
        fn test_pretty_rebreaks_only_the_last_two_lines_when_that_is_enough() {
            let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mm";
            assert_eq!(breaks(text, 200., TextWrapStyle::Auto), vec![20, 40, 60]);
            // Only "llll" moves.
            assert_eq!(breaks(text, 200., TextWrapStyle::Pretty), vec![20, 40, 55]);
        }

        #[test]
        fn test_pretty_leaves_the_lines_before_the_last_four_where_greedy_put_them() {
            // Two words of 20 characters fill the first two lines, which no narrower
            // width keeps whole: they stay out of the re-breaking.
            let text = "pppppppppppppppppppp qqqqqqqqqqqqqqqqqqqq aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mm";
            let greedy = breaks(text, 200., TextWrapStyle::Auto);
            assert_eq!(greedy.len(), 5);
            let pretty = breaks(text, 200., TextWrapStyle::Pretty);
            assert_eq!(pretty.len(), 5);
            assert_eq!(greedy[..3], pretty[..3]);
            assert_ne!(greedy, pretty);
        }

        #[test]
        fn test_min_content_ignores_the_style() {
            let options = TextLayoutOptions {
                text_wrap_style: TextWrapStyle::Balance,
                ..Default::default()
            };
            assert_eq!(
                options.for_min_content().text_wrap_style,
                TextWrapStyle::Auto
            );
        }
    }

    mod rows {
        use super::{Pixels, RowMetrics, ShapedRun, WrappedLineLayout};
        use crate as gpui;
        use crate::{TestAppContext, TextRun, WindowTextSystem, font, point, px};
        use std::sync::Arc;

        const LINE_HEIGHT: f32 = 30.;

        fn run(len: usize, font_size: Option<f32>, line_height: Option<f32>) -> TextRun {
            TextRun {
                len,
                font: font("Menlo"),
                font_size: font_size.map(px),
                line_height: line_height.map(px),
                ..Default::default()
            }
        }

        fn shape(
            cx: &mut TestAppContext,
            text: &str,
            runs: &[TextRun],
            wrap_width: f32,
        ) -> Arc<WrappedLineLayout> {
            cx.update(|cx| {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                let lines = text_system
                    .shape_text(
                        text.to_string().into(),
                        px(10.),
                        runs,
                        Some(px(wrap_width)),
                        None,
                    )
                    .unwrap();
                assert_eq!(lines.len(), 1);
                lines[0].layout.clone()
            })
        }

        fn assert_px(actual: Pixels, expected: Pixels) {
            assert!(
                (f32::from(actual) - f32::from(expected)).abs() < 1e-3,
                "{actual:?} != {expected:?}"
            );
        }

        fn assert_row(row: RowMetrics, height: Pixels, baseline: Pixels) {
            assert_px(row.height, height);
            assert_px(row.baseline, baseline);
        }

        fn strut(layout: &WrappedLineLayout) -> RowMetrics {
            let line_height = px(LINE_HEIGHT);
            RowMetrics {
                height: line_height,
                baseline: crate::extent_in_line(
                    layout.ascent(),
                    layout.descent(),
                    line_height,
                    layout.unwrapped_layout.scale_factor,
                )
                .0,
            }
        }

        fn above(run: &ShapedRun, line_height: Pixels) -> Pixels {
            crate::extent_in_line(run.ascent, run.descent, line_height, 1.).0
        }

        /// "aaaa " plain, "bbbb " big, "cc dd": plain "cc " and big "dd", which wraps into
        /// a row of only plain text, a row of only big text, and a row of both.
        fn mixed(cx: &mut TestAppContext, big_line_height: Option<f32>) -> Arc<WrappedLineLayout> {
            let layout = shape(
                cx,
                "aaaa bbbb cc dd",
                &[
                    run(5, None, None),
                    run(5, Some(20.), big_line_height),
                    run(3, None, None),
                    run(2, Some(20.), big_line_height),
                ],
                60.,
            );
            assert_eq!(layout.runs().len(), 4);
            let starts = layout
                .wrap_boundaries()
                .iter()
                .map(|boundary| layout.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index)
                .collect::<Vec<_>>();
            assert_eq!(starts, vec![5, 10]);
            layout
        }

        /// Menlo 10px has a content area of 9 + 2 CSS pixels at scale 1 and 9.5 + 2.5 at scale 2, so a
        /// line of 30px has 19 and 18 CSS pixels over it, half of which is 9 and 9 device pixels
        /// above the text: the baseline is at 18 and at 18.5.
        #[crate::test]
        fn the_half_leading_of_a_row_is_floored_to_a_device_pixel_of_the_scale_it_was_shaped_for(
            cx: &mut TestAppContext,
        ) {
            cx.update(|cx| {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                let line_height = px(30.);
                let mut baselines = Vec::new();
                for scale in [1., 2.] {
                    text_system.set_scale_factor(scale);
                    let lines = text_system
                        .shape_text("aaaa".into(), px(10.), &[run(4, None, None)], None, None)
                        .unwrap();
                    let layout = &lines[0].layout;
                    assert_eq!(layout.unwrapped_layout.scale_factor, scale);
                    let row = layout.rows(line_height)[0];
                    let (ascent, descent) = (layout.ascent(), layout.descent());
                    assert_eq!(
                        row.baseline,
                        crate::extent_in_line(ascent, descent, line_height, scale).0
                    );
                    assert_eq!(row.height, line_height);
                    // The half-leading is a whole number of device pixels above the text.
                    let half_leading = (row.baseline - ascent).0 * scale;
                    assert_eq!(half_leading, half_leading.floor(), "{half_leading} at scale {scale}");
                    baselines.push(row.baseline);
                }
                assert_eq!(baselines, [px(18.), px(18.5)]);
            });
        }

        #[crate::test]
        fn a_layout_shaped_for_one_scale_is_not_the_answer_at_another(cx: &mut TestAppContext) {
            cx.update(|cx| {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                let shape = |scale: f32| {
                    text_system.set_scale_factor(scale);
                    text_system
                        .shape_text("aaaa".into(), px(10.), &[run(4, None, None)], None, None)
                        .unwrap()[0]
                        .layout
                        .clone()
                };
                let at_1 = shape(1.);
                let at_2 = shape(2.);
                assert!(!Arc::ptr_eq(&at_1, &at_2));
                assert_eq!(text_system.text_counters().wrapped_layouts, 2);
                // Back at the first scale the first layout is found again, and the second is not shaped twice.
                assert!(Arc::ptr_eq(&at_1, &shape(1.)));
                assert!(Arc::ptr_eq(&at_2, &shape(2.)));
                assert_eq!(text_system.text_counters().wrapped_layouts, 2);
            });
        }

        #[crate::test]
        fn a_row_is_as_tall_as_its_tallest_run_around_one_baseline(cx: &mut TestAppContext) {
            let layout = mixed(cx, Some(60.));
            let big = &layout.runs()[1];
            let big_above = above(big, px(60.));
            let rows = layout.rows(px(LINE_HEIGHT));
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0], strut(&layout));
            assert_row(rows[1], px(60.), big_above);
            assert_row(rows[2], px(60.), big_above);
            assert_eq!(
                layout
                    .unwrapped_layout
                    .rows(layout.wrap_boundaries(), px(LINE_HEIGHT)),
                rows
            );
            assert_px(layout.size(px(LINE_HEIGHT)).height, px(150.));
        }

        #[crate::test]
        fn a_big_run_without_a_line_height_of_its_own_uses_the_line_height(
            cx: &mut TestAppContext,
        ) {
            let layout = mixed(cx, None);
            let big = &layout.runs()[1];
            let big_above = above(big, px(LINE_HEIGHT));
            let big_below = px(LINE_HEIGHT) - big_above;
            let strut = strut(&layout);
            let half_leading = px(LINE_HEIGHT) - strut.baseline;
            let rows = layout.rows(px(LINE_HEIGHT));
            assert_eq!(rows[0], strut);
            let expected_above = big_above.max(strut.baseline);
            let expected_below = big_below.max(half_leading);
            for row in &rows[1..] {
                assert_row(*row, expected_above + expected_below, expected_above);
            }
            assert!(rows[1].height > px(LINE_HEIGHT));
        }

        #[crate::test]
        fn a_run_with_only_a_line_height_grows_its_row(cx: &mut TestAppContext) {
            let layout = shape(
                cx,
                "aaaa bbbb cccc",
                &[
                    run(5, None, None),
                    run(5, None, Some(50.)),
                    run(4, None, None),
                ],
                30.,
            );
            assert_eq!(layout.wrap_boundaries().len(), 2);
            let tall = above(&layout.runs()[1], px(50.));
            let rows = layout.rows(px(LINE_HEIGHT));
            assert_eq!(rows[0], strut(&layout));
            assert_row(rows[1], px(50.), tall);
            assert_eq!(rows[2], strut(&layout));
            assert_px(layout.size(px(LINE_HEIGHT)).height, px(110.));
        }

        #[crate::test]
        fn a_smaller_run_with_a_small_line_height_leaves_the_row_the_strut(
            cx: &mut TestAppContext,
        ) {
            let layout = shape(
                cx,
                "aaaa bbbb cccc",
                &[
                    run(5, None, None),
                    run(5, Some(5.), Some(6.)),
                    run(4, None, None),
                ],
                30.,
            );
            assert_eq!(layout.wrap_boundaries().len(), 2);
            assert_eq!(
                layout.rows(px(LINE_HEIGHT)).as_slice(),
                [strut(&layout); 3].as_slice()
            );
            assert_eq!(layout.size(px(LINE_HEIGHT)).height, px(LINE_HEIGHT * 3.));
        }

        #[crate::test]
        fn without_styled_runs_every_row_is_the_line_height(cx: &mut TestAppContext) {
            let layout = shape(cx, "aaaa bbbb cccc", &[run(14, None, None)], 30.);
            let rows = layout.rows(px(LINE_HEIGHT));
            assert_eq!(rows.len(), 3);
            let expected = strut(&layout);
            assert!(rows.iter().all(|row| *row == expected));
            assert_eq!(layout.size(px(LINE_HEIGHT)).height, px(LINE_HEIGHT) * 3);
            assert_eq!(layout.size(px(LINE_HEIGHT)).width, layout.width());
            // An index on a wrap boundary is still on the row that ends there.
            for (index, row) in [(0, 0), (5, 0), (6, 1), (11, 2)] {
                assert_eq!(
                    layout.position_for_index(index, px(LINE_HEIGHT)).unwrap().y,
                    px(LINE_HEIGHT) * row
                );
            }
            assert_eq!(
                layout.index_for_position(point(px(1.), px(65.)), px(LINE_HEIGHT)),
                Ok(10)
            );
            assert_eq!(
                layout.index_for_position(point(px(1.), px(90.)), px(LINE_HEIGHT)),
                Err(0)
            );
            assert_eq!(
                layout.index_for_position(point(px(1.), px(-5.)), px(LINE_HEIGHT)),
                Ok(0)
            );
        }

        #[crate::test]
        fn positions_and_indices_follow_the_row_heights(cx: &mut TestAppContext) {
            let layout = mixed(cx, Some(60.));
            let line_height = px(LINE_HEIGHT);
            let tops = [0., 30., 90.];
            // An index on a wrap boundary is still on the row that ends there.
            for (index, top) in [
                (0, tops[0]),
                (5, tops[0]),
                (6, tops[1]),
                (7, tops[1]),
                (10, tops[1]),
                (11, tops[2]),
                (13, tops[2]),
            ] {
                assert_px(
                    layout.position_for_index(index, line_height).unwrap().y,
                    px(top),
                );
            }
            // A big glyph is 12px wide, a plain one 6px.
            assert_px(
                layout.position_for_index(7, line_height).unwrap().x,
                px(24.),
            );

            let at = |x: f32, y: f32| layout.index_for_position(point(px(x), px(y)), line_height);
            assert_eq!(at(7., -5.), Ok(1));
            assert_eq!(at(7., 29.), Ok(1));
            assert_eq!(at(1., 30.), Ok(5));
            assert_eq!(at(13., 80.), Ok(6));
            assert_eq!(at(1., 89.), Ok(5));
            assert_eq!(at(1., 90.), Ok(10));
            assert_eq!(at(1., 100.), Ok(10));
            assert_eq!(at(1., 149.), Ok(10));
            assert_eq!(at(1., 150.), Err(0));
            assert_eq!(at(1., 1000.), Err(0));

            assert_eq!(
                layout.closest_index_for_position(point(px(11.), px(40.)), line_height),
                Ok(6)
            );
            assert_eq!(
                layout.closest_index_for_position(point(px(1.), px(150.)), line_height),
                Err(0)
            );
        }

        #[crate::test]
        fn the_extent_of_a_range_is_the_largest_of_the_runs_it_touches(cx: &mut TestAppContext) {
            let layout = mixed(cx, None);
            let plain = (layout.runs()[0].ascent, layout.runs()[0].descent);
            let big = (layout.runs()[1].ascent, layout.runs()[1].descent);
            assert_eq!(layout.run_extent(0..5), Some(plain));
            assert_eq!(layout.run_extent(5..10), Some(big));
            assert_eq!(
                layout.run_extent(3..7),
                Some((plain.0.max(big.0), plain.1.max(big.1)))
            );
            assert_eq!(layout.run_extent(20..30), None);
            assert_eq!(layout.run_extent(5..5), None);
        }

        #[crate::test]
        fn the_content_extent_is_that_of_the_runs_of_the_font_size(cx: &mut TestAppContext) {
            let layout = mixed(cx, None);
            let plain = (layout.runs()[0].ascent, layout.runs()[0].descent);
            let big = (layout.runs()[1].ascent, layout.runs()[1].descent);
            assert!(big.0 > plain.0);
            assert_eq!(layout.content_extent(0..15, px(10.)), Some(plain));
            assert_eq!(layout.content_extent(0..15, px(20.)), Some(big));
            assert_eq!(layout.content_extent(3..7, px(10.)), Some(plain));
            assert_eq!(layout.content_extent(3..7, px(20.)), Some(big));
            assert_eq!(layout.content_extent(20..30, px(10.)), None);
            assert_eq!(layout.content_extent(5..5, px(10.)), None);
        }

        #[crate::test]
        fn the_content_extent_of_a_font_size_no_run_has_scales_the_first_run(
            cx: &mut TestAppContext,
        ) {
            let layout = mixed(cx, None);
            let big = &layout.runs()[1];
            let (ascent, descent) = layout.content_extent(5..10, px(40.)).unwrap();
            assert_px(ascent, big.ascent * 2.);
            assert_px(descent, big.descent * 2.);
            let (ascent, descent) = layout.content_extent(3..7, px(5.)).unwrap();
            let plain = &layout.runs()[0];
            assert_px(ascent, plain.ascent / 2.);
            assert_px(descent, plain.descent / 2.);
        }
    }

    /// `text` as ten pixel wide characters, with the placeholder of an atomic inline
    /// wherever there is a U+FFFC: it is `object_width` wide and in a run of its own.
    fn object_line(
        text: &str,
        object_width: f32,
        wrap_width: Option<f32>,
        options: TextLayoutOptions,
    ) -> WrappedLineLayout {
        let mut runs: Vec<ShapedRun> = Vec::new();
        let mut x = 0.;
        for (index, character) in text.char_indices() {
            let is_object = character == crate::OBJECT_REPLACEMENT_CHARACTER;
            if is_object || runs.last().is_none_or(|run| run.inline.object) {
                runs.push(ShapedRun {
                    inline: InlineRun {
                        placement: RunPlacement::Baseline,
                        object: is_object,
                        ..Default::default()
                    },
                    ..make_layout(Vec::new()).runs.remove(0)
                });
            }
            runs.last_mut().unwrap().glyphs.push(glyph_at(x, index));
            x += if is_object { object_width } else { 10. };
        }
        let shaped = LineLayout {
            width: px(x),
            len: text.len(),
            runs,
            ..make_layout(Vec::new())
        };
        let computation = shaped.compute_wrap_boundaries(
            text,
            wrap_width.map(px),
            None,
            options,
            None,
            &[],
        );
        WrappedLineLayout::new(
            Arc::new(shaped),
            wrap_width.map(px),
            computation,
            &SnapMarks::default(),
        )
    }

    #[test]
    fn test_a_line_breaks_before_and_after_an_atomic_inline() {
        let normal = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            ..Default::default()
        };
        // "aaaa" ends at 40, the box at 80, and the "b"s at 90 and 100: with 90 to fill,
        // the second "b" overflows, and the line breaks after the box where it can.
        let text = "aaaa\u{FFFC}bbbb";
        let layout = object_line(text, 40., Some(90.), normal);
        assert_eq!(boundary_indices(&layout), vec![7]);
        // A box that does not fit moves down with no letter before it to break at.
        let layout = object_line("aaaa\u{FFFC}", 60., Some(90.), normal);
        assert_eq!(boundary_indices(&layout), vec![4]);
        // And one wider than the line overflows it from the start.
        let layout = object_line("\u{FFFC}aa", 100., Some(90.), normal);
        assert_eq!(boundary_indices(&layout), vec![3]);
        // Several boxes break between any two.
        let layout = object_line("\u{FFFC}\u{FFFC}\u{FFFC}", 40., Some(90.), normal);
        assert_eq!(boundary_indices(&layout), vec![6]);
    }

    #[test]
    fn test_a_space_before_an_atomic_inline_hangs_when_the_box_wraps() {
        let options = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            hang_trailing_spaces: true,
            ..Default::default()
        };
        let layout = object_line("aaaa \u{FFFC}", 60., Some(90.), options);
        assert_eq!(boundary_indices(&layout), vec![5]);
        assert_eq!(layout.hanging_widths.first(), Some(&px(10.)));
    }

    #[test]
    fn test_a_run_aligned_to_the_top_or_bottom_grows_the_row_the_way_the_taller_one_says() {
        let rows = |placements: &[(RunPlacement, f32, f32)]| {
            let mut layout = make_layout(Vec::new());
            // The strut: a font of ascent 12 and descent 4 in a line height of 16.
            layout.runs.clear();
            for (ix, (placement, ascent, descent)) in placements.iter().enumerate() {
                layout.runs.push(ShapedRun {
                    ascent: px(*ascent),
                    descent: px(*descent),
                    line_height: Some(px(ascent + descent)),
                    glyphs: vec![glyph_at(ix as f32, ix)],
                    inline: InlineRun {
                        placement: *placement,
                        object: true,
                        ..Default::default()
                    },
                    ..make_layout(Vec::new()).runs.remove(0)
                });
            }
            layout.rows(&[], px(16.))[0]
        };
        // Baseline runs: the strut is 12 above and 4 below.
        assert_eq!(
            rows(&[(RunPlacement::Baseline, 20., 0.)]),
            RowMetrics { height: px(24.), baseline: px(20.) }
        );
        // A taller top run grows the row down, a taller bottom one up.
        assert_eq!(
            rows(&[(RunPlacement::Top, 30., 0.)]),
            RowMetrics { height: px(30.), baseline: px(12.) }
        );
        assert_eq!(
            rows(&[(RunPlacement::Bottom, 30., 0.)]),
            RowMetrics { height: px(30.), baseline: px(26.) }
        );
        // The taller of the two decides, and the top one when they are as tall.
        assert_eq!(
            rows(&[(RunPlacement::Top, 20., 0.), (RunPlacement::Bottom, 30., 0.)]),
            RowMetrics { height: px(30.), baseline: px(26.) }
        );
        assert_eq!(
            rows(&[(RunPlacement::Top, 30., 0.), (RunPlacement::Bottom, 20., 0.)]),
            RowMetrics { height: px(30.), baseline: px(12.) }
        );
        assert_eq!(
            rows(&[(RunPlacement::Top, 30., 0.), (RunPlacement::Bottom, 30., 0.)]),
            RowMetrics { height: px(30.), baseline: px(12.) }
        );
        // A raised run is 5px higher: 15 above the baseline, and the strut still has 4
        // below it.
        assert_eq!(
            rows(&[(RunPlacement::Raised(px(5.)), 10., 2.)]),
            RowMetrics { height: px(19.), baseline: px(15.) }
        );
        // A lowered one has 7 below it, and the strut 12 above.
        assert_eq!(
            rows(&[(RunPlacement::Raised(px(-5.)), 10., 2.)]),
            RowMetrics { height: px(19.), baseline: px(12.) }
        );
    }

    /// Ten pixel wide characters in `markup`, whose boxes are `<n>` (does not wrap),
    /// `<w>` (wraps) and `<p>` (says nothing), wrapped at `columns` and a half; the
    /// rows, without the spaces between them. `root_nowrap` is a text that does not wrap
    /// outside its boxes.
    fn rows_of(markup: &str, columns: f32, root_nowrap: bool, options: TextLayoutOptions) -> Vec<String> {
        let mut text = String::new();
        let mut specs: Vec<InlineSpec> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        let mut rest = markup;
        while !rest.is_empty() {
            if let Some(tag) = ["<n>", "<w>", "<p>"].iter().find(|tag| rest.starts_with(**tag)) {
                specs.push(InlineSpec {
                    range: text.len()..text.len(),
                    parent: open.last().copied(),
                    align: crate::VerticalAlign::Baseline,
                    line_height: None,
                    wraps: match *tag {
                        "<n>" => Some(false),
                        "<w>" => Some(true),
                        _ => None,
                    },
                    key: None,
                    content: crate::text_system::SpecContent::Text {
                        font_id: FontId(0),
                        font_size: px(16.),
                    },
                });
                open.push(specs.len() - 1);
                rest = &rest[3..];
            } else if ["</n>", "</w>", "</p>"].iter().any(|tag| rest.starts_with(*tag)) {
                let ix = open.pop().expect("a closing tag matches an opening one");
                specs[ix].range.end = text.len();
                rest = &rest[4..];
            } else {
                let character = rest.chars().next().unwrap();
                text.push(character);
                rest = &rest[character.len_utf8()..];
            }
        }
        let boxes: Vec<crate::text_system::ResolvedBox> = specs
            .iter()
            .map(|_| crate::text_system::ResolvedBox {
                placement: RunPlacement::Baseline,
                above: px(0.),
                below: px(0.),
                ascent: px(0.),
            })
            .collect();
        let options = TextLayoutOptions {
            nowrap: root_nowrap,
            hang_trailing_spaces: true,
            ..options
        };
        let shaped = apply_inline(&monospace_layout(&text, 10.), &specs, &boxes, !root_nowrap);
        let computation = shaped.compute_wrap_boundaries(
            &text,
            Some(px(columns * 10. + 5.)),
            None,
            options,
            None,
            &[],
        );
        let chars: Vec<char> = text.chars().collect();
        let mut rows = Vec::new();
        for (ix, start) in computation.line_starts.iter().enumerate() {
            let end = computation.line_starts.get(ix + 1).copied().unwrap_or(chars.len());
            rows.push(chars[*start..end].iter().collect::<String>().trim().to_string());
        }
        rows
    }

    fn rows(markup: &str, columns: f32, root_nowrap: bool) -> Vec<String> {
        let options = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            ..Default::default()
        };
        rows_of(markup, columns, root_nowrap, options)
    }

    /// The expectations of the next tests were read off Chromium, with the same text in a
    /// block and `text-wrap-mode` on its spans: `fixtures/text-wrap-spans-oracle.html`.
    #[test]
    fn test_a_box_that_does_not_wrap_is_not_broken_and_moves_whole() {
        for (markup, expected) in [
            ("aaaa bbbb cccc dddd eeee ffff", "aaaa bbbb cccc dddd|eeee ffff"),
            ("aaaa bbbb <n>cccc dddd eeee</n> ffff", "aaaa bbbb|cccc dddd eeee ffff"),
            ("aaaa bbbb cccc <n>dddd eeee</n> ffff", "aaaa bbbb cccc|dddd eeee ffff"),
            ("<n>aaaa bbbb cccc dddd eeee</n> ffff gggg", "aaaa bbbb cccc dddd eeee|ffff gggg"),
            ("aaaa bbbb cccc <n>dddd eeee ffff gggg</n>", "aaaa bbbb cccc|dddd eeee ffff gggg"),
            (
                "aaaa <n>bbbb cccc dddd eeee ffff gggg</n> hhhh",
                "aaaa|bbbb cccc dddd eeee ffff gggg|hhhh",
            ),
            ("aaaa bbbb<n>cccc dddd</n>eeee ffff", "aaaa|bbbbcccc ddddeeee|ffff"),
            ("aaaa bbbb <n>cccc</n> dddd eeee ffff", "aaaa bbbb cccc dddd|eeee ffff"),
        ] {
            assert_eq!(rows(markup, 20., false).join("|"), expected, "{markup}");
        }
    }

    #[test]
    fn test_the_space_of_a_box_that_does_not_wrap_does_not_break() {
        for (markup, expected) in [
            // The space leads the box.
            ("aaaa bbbb cccc<n> dddd eeee</n> ffff", "aaaa bbbb|cccc dddd eeee ffff"),
            // The space ends it.
            ("aaaa bbbb <n>cccc dddd </n>eeee ffff", "aaaa bbbb|cccc dddd eeee ffff"),
        ] {
            assert_eq!(rows(markup, 20., false).join("|"), expected, "{markup}");
        }
    }

    #[test]
    fn test_a_box_inside_and_a_box_around_a_box_take_the_mode_they_say_or_inherit() {
        for (markup, expected) in [
            (
                "aaaa bbbb <n>cccc dddd <w>eeee ffff</w> gggg</n> hhhh",
                "aaaa bbbb|cccc dddd eeee ffff|gggg hhhh",
            ),
            ("aaaa bbbb <n>cccc <p>dddd eeee</p> ffff</n> gggg", "aaaa bbbb|cccc dddd eeee ffff|gggg"),
            ("aaaa bbbb <n><p><p>cccc dddd eeee</p></p></n> ffff", "aaaa bbbb|cccc dddd eeee ffff"),
        ] {
            assert_eq!(rows(markup, 20., false).join("|"), expected, "{markup}");
        }
    }

    #[test]
    fn test_a_box_that_wraps_in_text_that_does_not_wraps() {
        for (markup, expected) in [
            ("aaaa bbbb cccc dddd eeee ffff", "aaaa bbbb cccc dddd eeee ffff"),
            ("aaaa bbbb <w>cccc dddd eeee ffff</w> gggg", "aaaa bbbb cccc dddd|eeee ffff gggg"),
            ("aaaa bbbb cccc <w>dddd eeee</w> ffff", "aaaa bbbb cccc dddd|eeee ffff"),
            ("<w>aaaa bbbb cccc dddd eeee</w> ffff gggg", "aaaa bbbb cccc dddd|eeee ffff gggg"),
            ("aaaa bbbb cccc<w> dddd eeee</w> ffff", "aaaa bbbb cccc dddd|eeee ffff"),
            ("aaaa bbbb <w>cccc dddd </w>eeee ffff", "aaaa bbbb cccc dddd|eeee ffff"),
            ("aaaa bbbb <w>cccc <p>dddd eeee</p> ffff</w> gggg", "aaaa bbbb cccc dddd|eeee ffff gggg"),
        ] {
            assert_eq!(rows(markup, 20., true).join("|"), expected, "{markup}");
        }
    }

    #[test]
    fn test_a_word_is_not_broken_where_the_text_does_not_wrap() {
        let breaking = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Anywhere,
            ..Default::default()
        };
        assert_eq!(
            rows_of("aaaa <n>bbbbbbbbbbbbbbbbbbbb</n> cccc", 10., false, breaking).join("|"),
            "aaaa|bbbbbbbbbbbbbbbbbbbb|cccc"
        );
        assert_eq!(
            rows("aaaa <w>bbbbbbbbbbbbbbbbbbbb</w> cccc", 10., true).join("|"),
            "aaaa bbbbbbbbbbbbbbbbbbbb|cccc"
        );
        // Where the text wraps the word is broken, but not in the box that does not.
        assert_eq!(
            rows_of("aaaa <w>bbbbbbbbbbbbbbbbbbbb</w> cccc", 10., true, breaking).join("|"),
            "aaaa bbbbb|bbbbbbbbbb|bbbbb cccc"
        );
    }

    /// Whether `W S N`, a word, a space and a word in boxes each of its own, break at the
    /// space in a text 10.5 columns wide, for every mode of the text and of the three
    /// boxes (`i` is no box): Chromium's, from the same file.
    #[test]
    fn test_the_break_at_a_space_between_boxes_is_the_space_s_or_the_word_before_it() {
        let modes = ['i', 'w', 'n'];
        for (root_nowrap, expected) in [
            (false, "111111000111111000111111000"),
            (true, "000111000111111000000111000"),
        ] {
            let mut expected = expected.chars();
            for word in modes {
                for space in modes {
                    for next in modes {
                        let boxed = |mode: char, text: &str| match mode {
                            'i' => text.to_string(),
                            mode => format!("<{mode}>{text}</{mode}>"),
                        };
                        let markup = format!("{}{}{}", boxed(word, "wwwwww"), boxed(space, " "), boxed(next, "zzzzzz"));
                        let broke = rows(&markup, 10., root_nowrap).len() > 1;
                        assert_eq!(
                            broke,
                            expected.next() == Some('1'),
                            "text nowrap {root_nowrap}, word {word}, space {space}, next {next}"
                        );
                    }
                }
            }
        }
    }

    /// A box that does not wrap starting at the space takes the break away from the word
    /// before it, even when it says the same as the text around it.
    #[test]
    fn test_a_box_that_does_not_wrap_opening_at_the_space_is_no_break() {
        // Chromium: a nowrap text with a span that wraps, then a plain span with the space.
        assert_eq!(rows("<w>wwwwww</w> zzzzzz", 10., true).len(), 2);
        assert_eq!(rows("<w>wwwwww</w><p> </p>zzzzzz", 10., true).len(), 1);
        assert_eq!(rows("<w>wwwwww</w><p> </p>zzzzzz", 10., false).len(), 2);
    }

    #[test]
    fn test_between_letters_a_box_around_both_or_either_side_lets_the_line_break() {
        let break_all = TextLayoutOptions {
            word_break: WordBreak::BreakAll,
            ..Default::default()
        };
        let modes = ['i', 'w', 'n'];
        for (root_nowrap, expected) in [(false, "111111111"), (true, "010111010")] {
            let mut expected = expected.chars();
            for first in modes {
                for second in modes {
                    let boxed = |mode: char, text: &str| match mode {
                        'i' => text.to_string(),
                        mode => format!("<{mode}>{text}</{mode}>"),
                    };
                    let markup = format!("{}{}", boxed(first, "wwwwwwwww"), boxed(second, "zzzzzzzzzz"));
                    let broke = rows_of(&markup, 10., root_nowrap, break_all).len() > 1;
                    assert_eq!(
                        broke,
                        expected.next() == Some('1'),
                        "text nowrap {root_nowrap}, {first} then {second}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod line_box_tests {
    use super::*;

    fn run(ascent: f32, descent: f32, line_height: Option<f32>) -> ShapedRun {
        ShapedRun {
            font_id: FontId(0),
            font_size: px(20.),
            ascent: px(ascent),
            descent: px(descent),
            line_height: line_height.map(px),
            glyphs: vec![ShapedGlyph {
                id: crate::GlyphId(0),
                position: point(px(0.), px(0.)),
                index: 0,
                is_emoji: false,
            }],
            inline: InlineRun::default(),
        }
    }

    fn layout(runs: Vec<ShapedRun>) -> LineLayout {
        LineLayout {
            boxes: Vec::new(),
            font_size: px(20.),
            width: px(10.),
            ascent: px(19.),
            descent: px(4.),
            scale_factor: 1.,
            runs,
            len: 1,
        }
    }

    #[test]
    fn test_a_run_with_the_content_area_of_the_strut_is_not_a_box_of_its_own() {
        let layout = layout(vec![run(19., 4., None)]);
        assert!(!layout.is_styled(&layout.runs[0]));
        // The half-leading is 0.5px, floored: the strut is 19px above the baseline.
        assert_eq!(
            layout.rows(&[], px(24.)).as_slice(),
            [RowMetrics {
                height: px(24.),
                baseline: px(19.)
            }]
        );
    }

    /// Bold Helvetica Neue at 20px (a content area of 20 and 4) beside regular Helvetica Neue
    /// (19 and 4), in a line of 24px: Chrome makes the line 25px tall,
    /// `mixed.helvetica-neue.reg20-bold20.24px`.
    #[test]
    fn test_a_run_whose_font_has_another_content_area_is_a_box_of_its_own() {
        let layout = layout(vec![run(19., 4., None), run(20., 4., None)]);
        assert!(layout.is_styled(&layout.runs[1]));
        // The strut reaches 19 above the baseline and 5 below, the bold run 20 and 4.
        assert_eq!(
            layout.rows(&[], px(24.)).as_slice(),
            [RowMetrics {
                height: px(25.),
                baseline: px(20.)
            }]
        );
    }

    #[test]
    fn test_the_strut_is_not_grown_by_the_font_of_a_run() {
        let mut layout = layout(vec![run(20., 4., None)]);
        layout.ascent = px(19.);
        layout.descent = px(4.);
        // Only the run has glyphs, but the strut is still the line's font.
        assert_eq!(layout.rows(&[], px(24.))[0].height, px(25.));
    }
}
