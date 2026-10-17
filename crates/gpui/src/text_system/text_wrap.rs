//! CSS `text-wrap-style`: which of the ways a text can break into lines of the
//! given width it takes.
//!
//! Both styles are found by laying the text out again at a narrower width and
//! keeping the breaks, so tab stops, insets and hanging spaces stay consistent
//! with them. The width the text reports to layout is not touched: the box is sized
//! from the available width and only the breaks move, as in CSS.

use super::line_layout::{LineLayout, TabStopFor, TextInset, TextLayoutOptions, WrapComputation};
use crate::{Pixels, px};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which breaks a wrapped line gets, as in CSS `text-wrap-style`. `stable` is
/// `Auto`: both lay a text out greedily.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TextWrapStyle {
    /// Each line takes as many words as fit.
    #[default]
    Auto,
    /// The lines of a paragraph of few lines are made about equally long, with as
    /// many lines as `Auto` makes.
    Balance,
    /// The last lines of a paragraph are re-broken so that the last one is not a
    /// single word, with as many lines as `Auto` makes.
    Pretty,
}

/// Lines from line `from_line` (counting from 0) on break at `width`.
#[derive(Clone, Copy, Debug)]
pub(super) struct NarrowedLines {
    pub(super) from_line: usize,
    pub(super) width: Pixels,
}

/// Chromium balances a paragraph of at most this many lines (`kMaxLinesForBalance`
/// in `score_line_break_context.h`).
const MAX_BALANCED_LINES: usize = 6;

/// Chromium re-breaks this many lines at the end of a paragraph for `pretty`
/// (`kMaxLinesForOptimal`).
const PRETTY_LINES: usize = 4;

/// A last line shorter than this fraction of the line is short, for `pretty`.
const SHORT_LINE_DENOMINATOR: f32 = 3.;

/// A search for a width stops when its bounds are this close.
const WIDTH_PRECISION: f32 = 1. / 16.;

/// A line wider than its wrap width by no more than this still counts as fitting:
/// glyph positions are floats.
const FIT_TOLERANCE: f32 = 1. / 256.;

impl LineLayout {
    /// [`Self::compute_wrap_boundaries`], with the breaks `options.text_wrap_style`
    /// asks for among those that make no more lines than the greedy ones do.
    pub(super) fn compute_styled_wrap_boundaries(
        &self,
        text: &str,
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        options: TextLayoutOptions,
        tab_stop: Option<TabStopFor<'_>>,
        insets: &[TextInset],
    ) -> WrapComputation {
        let greedy =
            self.compute_wrap_boundaries(text, wrap_width, max_lines, options, tab_stop, insets);
        let (Some(wrap_width), None) = (wrap_width, max_lines) else {
            return greedy;
        };
        let line_count = greedy.boundaries.len() + 1;
        // Chromium lays out a text with a tab, or that breaks after every space, as
        // `auto` does, and so one with a line that overflows. A word wider than the
        // line leaves nothing to even out.
        if line_count < 2
            || text.contains('\t')
            || options.break_spaces
            || greedy.broke_word
            || widest(&greedy.line_widths, 0) > wrap_width + px(FIT_TOLERANCE)
        {
            return greedy;
        }
        let narrowed = |from_line, width| {
            self.compute_narrowed_wrap_boundaries(
                text,
                Some(wrap_width),
                Some(NarrowedLines { from_line, width }),
                None,
                options,
                tab_stop,
                insets,
            )
        };
        match options.text_wrap_style {
            TextWrapStyle::Auto => greedy,
            TextWrapStyle::Balance if line_count <= MAX_BALANCED_LINES => {
                // The narrowest width that still makes no more lines than greedy
                // does, bisected: a line count only grows as the width shrinks.
                let mut balanced = None;
                let (mut lo, mut hi) = (px(0.), wrap_width);
                while f32::from(hi - lo) > WIDTH_PRECISION {
                    let mid = (lo + hi) / 2.;
                    let computation = narrowed(0, mid);
                    if fits(&computation, line_count, 0, mid) {
                        hi = mid;
                        balanced = Some(computation);
                    } else {
                        lo = mid;
                    }
                }
                balanced.unwrap_or(greedy)
            }
            TextWrapStyle::Balance => greedy,
            TextWrapStyle::Pretty => {
                // Chromium only re-breaks when the last line is one word and shorter
                // than a third of the line (`kShortLineDenominator`).
                let last_line_width = greedy.line_widths[line_count - 1];
                if greedy.last_line_words >= 2
                    || last_line_width * SHORT_LINE_DENOMINATOR >= wrap_width
                {
                    return greedy;
                }
                // Narrowing the last lines moves words onto the last one. Move as
                // few lines as it takes, the last two first. For each, bisect to the
                // widest width that gives the last line a second word, which is also
                // the one that moves the fewest words.
                let first_narrowed = line_count.saturating_sub(PRETTY_LINES);
                for from_line in (first_narrowed..line_count - 1).rev() {
                    let (mut lo, mut hi) = (px(0.), wrap_width);
                    while f32::from(hi - lo) > WIDTH_PRECISION {
                        let mid = (lo + hi) / 2.;
                        let computation = narrowed(from_line, mid);
                        if fits(&computation, line_count, from_line, mid)
                            && computation.last_line_words < 2
                        {
                            hi = mid;
                        } else {
                            lo = mid;
                        }
                    }
                    let computation = narrowed(from_line, lo);
                    if fits(&computation, line_count, from_line, lo)
                        && computation.last_line_words >= 2
                    {
                        return computation;
                    }
                }
                greedy
            }
        }
    }
}

/// The widest of the lines from `from_line` on.
fn widest(line_widths: &[Pixels], from_line: usize) -> Pixels {
    line_widths
        .iter()
        .skip(from_line)
        .copied()
        .fold(px(0.), Pixels::max)
}

/// Whether `computation` makes no more than `line_count` lines, none of those from
/// `from_line` on wider than `width`, and breaks no word: a narrower wrap width must
/// not make a word overflow or cut it.
fn fits(computation: &WrapComputation, line_count: usize, from_line: usize, width: Pixels) -> bool {
    !computation.broke_word
        && computation.boundaries.len() < line_count
        && widest(&computation.line_widths, from_line) <= width + px(FIT_TOLERANCE)
}
