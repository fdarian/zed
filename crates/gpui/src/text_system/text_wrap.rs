//! CSS `text-wrap-style`: which of the ways a text can break into lines of the
//! given width it takes.
//!
//! The breaks are chosen among the places a line may break (the ones greedy wrapping
//! at no width at all uses), by the same measure of a line that greedy wrapping uses:
//! hanging spaces, insets and all. The width the text reports to layout is not
//! touched, the box is sized from the available width and only the breaks move, as
//! in CSS.

use super::line_layout::{
    LineLayout, SnapMarks, TabStopFor, TextInset, TextLayoutOptions, WrapComputation,
};
use crate::Pixels;
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
    /// single short word, with as many lines as `Auto` makes.
    Pretty,
}

/// Chromium balances a paragraph of at most this many lines (`kMaxLinesForBalance`
/// in `score_line_break_context.h`).
const MAX_BALANCED_LINES: usize = 6;

/// Chromium re-breaks this many lines at the end of a paragraph for `pretty`
/// (`kMaxLinesForOptimal`).
const PRETTY_LINES: usize = 4;

/// The last line of a paragraph is short for `pretty` when it is narrower than this
/// fraction of the line (`kShortLineDenominator`).
const SHORT_LINE_DENOMINATOR: f32 = 3.;

/// What a last line of a single word costs, in the square pixels a line's slack
/// costs (`kOrphansPenalty`).
const ORPHAN_PENALTY: f32 = 10_000.;

/// Two ways to break that differ by less than this, in square pixels, cost the same.
const TIE_TOLERANCE: f32 = 0.5;

/// What the last line of the lines being broken costs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastLine {
    /// The square of what is left of the wrap width, as every other line costs.
    Slack,
    /// Nothing for its slack, and a lot for being a single word.
    Orphan,
}

impl LineLayout {
    /// [`Self::compute_wrap_boundaries`], with the breaks `options.text_wrap_style`
    /// asks for among those that make as many lines as the greedy ones.
    pub(super) fn compute_styled_wrap_boundaries(
        &self,
        text: &str,
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        options: TextLayoutOptions,
        tab_stop: Option<TabStopFor<'_>>,
        insets: &[TextInset],
        marks: &SnapMarks,
    ) -> WrapComputation {
        let greedy = self.compute_wrap_boundaries(
            text, wrap_width, max_lines, options, tab_stop, insets, marks,
        );
        let (Some(wrap_width), None) = (wrap_width, max_lines) else {
            return greedy;
        };
        let line_count = greedy.line_starts.len();
        // Chromium lays out a text with a tab, or that breaks after every space, as
        // `auto` does, and so one with a line that overflows (checked below). A word
        // wider than the line leaves nothing to even out.
        if line_count < 2 || text.contains('\t') || options.break_spaces || greedy.broke_word {
            return greedy;
        }
        let (last_line, first_line) = match options.text_wrap_style {
            TextWrapStyle::Auto => return greedy,
            TextWrapStyle::Balance if line_count <= MAX_BALANCED_LINES => (LastLine::Slack, 0),
            TextWrapStyle::Balance => return greedy,
            TextWrapStyle::Pretty => {
                // Chromium only re-breaks when the last line is one word, shorter
                // than a third of the line.
                let last_line_width = greedy.line_widths[line_count - 1];
                if greedy.last_line_words >= 2
                    || last_line_width * SHORT_LINE_DENOMINATOR >= wrap_width
                {
                    return greedy;
                }
                (LastLine::Orphan, line_count.saturating_sub(PRETTY_LINES))
            }
        };

        let opportunities = self.break_opportunities(text, options, insets, marks);
        let glyph_count = opportunities.x.len() - 1;
        let fit = crate::util::fit_layout_units(wrap_width, self.scale_factor);
        let fits = |from: usize, to: usize| {
            marks.fit_units(
                |glyph| opportunities.x[glyph],
                glyph_count,
                self.scale_factor,
                from,
                to,
            ) <= fit
        };
        let overflows = greedy
            .line_starts
            .iter()
            .zip(&greedy.line_ends)
            .any(|(start, end)| !fits(*start, *end));
        if overflows {
            return greedy;
        }
        // The lines before the first one that is re-broken stay where greedy put them.
        let start = greedy.line_starts[first_line];
        let mut candidates = vec![start];
        candidates.extend(opportunities.breaks.iter().copied().filter(|ix| *ix > start));
        candidates.push(opportunities.x.len() - 1);
        let hangs = options.hang_trailing_spaces && !options.break_spaces;

        let line_cost = |from: usize, to: usize| -> Option<f32> {
            let first = candidates[from];
            let mut end = candidates[to];
            if hangs {
                while end > first && opportunities.hangs[end - 1] {
                    end -= 1;
                }
            }
            if !fits(first, end) {
                return None;
            }
            let width = opportunities.x[end] - opportunities.x[first];
            let slack = f32::from(wrap_width - width);
            let last = to == candidates.len() - 1;
            Some(match last_line {
                LastLine::Orphan if last => {
                    if to - from == 1 {
                        ORPHAN_PENALTY
                    } else {
                        0.
                    }
                }
                _ => slack * slack,
            })
        };
        let Some(chosen) = cheapest_breaks(candidates.len(), line_count - first_line, line_cost)
        else {
            return greedy;
        };

        let mut breaks: Vec<usize> = greedy.line_starts[1..=first_line].to_vec();
        breaks.extend(chosen.into_iter().map(|candidate| candidates[candidate]));
        self.compute_wrap_boundaries_at(text, &breaks, options, insets)
    }
}

/// Which of `count` candidate places to break, the first the start of the text and
/// the last its end, start the second to the last of `lines` lines when the lines
/// cost the least in all; or `None` when no way of making that many lines has every
/// one fit. `cost` is the cost of the line between two candidates, `None` for one
/// that does not fit. Returns indices into the candidates.
fn cheapest_breaks(
    count: usize,
    lines: usize,
    cost: impl Fn(usize, usize) -> Option<f32>,
) -> Option<Vec<usize>> {
    // The cheapest way to make `k` lines that end at a candidate, and the candidate
    // the last of them starts at.
    let mut cheapest = vec![vec![None::<(f32, usize)>; count]; lines + 1];
    cheapest[0][0] = Some((0., 0));
    for k in 1..=lines {
        for to in 1..count {
            for from in (0..to).rev() {
                // A line only gets wider as it takes more words.
                let Some(line) = cost(from, to) else { break };
                let Some((before, _)) = cheapest[k - 1][from] else {
                    continue;
                };
                let total = before + line;
                // Ties go to the shortest last line, so the earlier lines stay as
                // full as greedy makes them. Float error must not decide a tie.
                if cheapest[k][to].is_none_or(|(best, _)| total < best - TIE_TOLERANCE) {
                    cheapest[k][to] = Some((total, from));
                }
            }
        }
    }
    cheapest[lines][count - 1]?;
    let mut breaks = Vec::with_capacity(lines - 1);
    let mut to = count - 1;
    for k in (1..=lines).rev() {
        let (_, from) = cheapest[k][to]?;
        if k > 1 {
            breaks.push(from);
        }
        to = from;
    }
    breaks.reverse();
    Some(breaks)
}
