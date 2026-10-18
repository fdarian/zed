use crate::{
    Pixels, Result, SharedString, TextInset, TextLayoutOptions, TextRun, WindowTextSystem,
    WrappedLine, px,
};
use std::{borrow::Cow, iter, ops::Range};

/// Where and how `WindowTextSystem::truncate_end` cuts text.
pub(crate) struct EndTruncation<'a> {
    pub font_size: Pixels,
    /// The width of a line, which is also the wrap width when the lines wrap.
    pub line_width: Pixels,
    pub wraps: bool,
    /// How many lines the text may take. Without it each paragraph, the text between
    /// newlines, may take one.
    pub max_lines: Option<usize>,
    pub affix: &'a str,
    pub affix_width: Pixels,
    pub options: TextLayoutOptions,
    pub insets: &'a [TextInset],
}

/// One line of a laid out paragraph, as byte indices into the paragraph and the x
/// of both ends.
struct Row {
    start: usize,
    end: usize,
    start_x: Pixels,
    end_x: Pixels,
}

impl Row {
    fn of(line: &WrappedLine, row: usize) -> Self {
        let boundaries = line.layout.wrap_boundaries();
        let glyph = |ix: usize| {
            let boundary = boundaries[ix];
            &line.layout.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
        };
        let (start, start_x) = match row.checked_sub(1) {
            Some(previous) => (glyph(previous).index, glyph(previous).position.x),
            None => (0, px(0.)),
        };
        let (end, end_x) = if row < boundaries.len() {
            (glyph(row).index, glyph(row).position.x)
        } else {
            (line.len(), line.layout.unwrapped_layout.width)
        };
        Self {
            start,
            end,
            start_x,
            end_x,
        }
    }

    /// The width of the row without the spaces that hang at its end.
    fn width(&self, line: &WrappedLine, row: usize) -> Pixels {
        let hanging = line
            .layout
            .hanging_widths
            .get(row)
            .copied()
            .unwrap_or(px(0.));
        self.end_x - self.start_x - hanging
    }

    /// The last place in the row where the text can end with the affix after it
    /// and still fit in `line_width`, which is the row's start when nothing does.
    fn cut(&self, line: &WrappedLine, affix_width: Pixels, line_width: Pixels) -> usize {
        let glyphs = line
            .layout
            .unwrapped_layout
            .runs
            .iter()
            .flat_map(|run| &run.glyphs)
            .filter(|glyph| (self.start..self.end).contains(&glyph.index))
            .map(|glyph| (glyph.index, glyph.position.x));
        let mut cut = self.start;
        for (index, x) in glyphs.chain(iter::once((self.end, self.end_x))) {
            if crate::util::overflows(x - self.start_x + affix_width, line_width) {
                break;
            }
            cut = index;
        }
        cut
    }
}

impl WindowTextSystem {
    /// Cuts `text` at its end the way a browser puts `text-overflow: ellipsis` on a
    /// `line-clamp`: the lines kept are the lines the text wraps to unclamped, and the
    /// last of them ends after the last glyph that fits with `affix`, without the
    /// spaces and punctuation before it. A line that overflows without a place to
    /// break is cut as well. The text is shaped as it is laid out, so tab stops, hanging
    /// spaces and the places a line may break all count.
    ///
    /// Returns the text itself when nothing is cut.
    pub(crate) fn truncate_end<'a>(
        &self,
        text: &SharedString,
        runs: &'a [TextRun],
        truncation: &EndTruncation,
    ) -> Result<(SharedString, Cow<'a, [TextRun]>)> {
        let wrap_width = truncation.wraps.then_some(truncation.line_width);
        let lines = self.shape_text_with_insets(
            text.clone(),
            truncation.font_size,
            runs,
            wrap_width,
            None,
            truncation.options,
            truncation.insets,
        )?;

        let mut cuts: Vec<Range<usize>> = Vec::new();
        let mut remaining = truncation.max_lines.map(|max_lines| max_lines.max(1));
        let mut paragraph_start = 0;
        for line in &lines {
            let paragraph_end = paragraph_start + line.len();
            let rows = line.layout.wrap_boundaries().len() + 1;
            let allowed = remaining.unwrap_or(1);
            let content_follows = !text[paragraph_end..].trim().is_empty();
            let cut_by_lines =
                rows > allowed || (remaining.is_some() && rows == allowed && content_follows);

            let row_ix = if cut_by_lines { allowed - 1 } else { rows - 1 };
            let row = Row::of(line, row_ix);
            if cut_by_lines || crate::util::overflows(row.width(line, row_ix), truncation.line_width) {
                let cut = row.cut(line, truncation.affix_width, truncation.line_width);
                let kept = line.text[row.start..cut]
                    .trim_end_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation());
                let cut_at = paragraph_start + row.start + kept.len();
                if cut_by_lines && remaining.is_some() {
                    cuts.push(cut_at..text.len());
                    break;
                }
                cuts.push(cut_at..paragraph_end);
            }

            if let Some(remaining) = remaining.as_mut() {
                *remaining -= rows.min(*remaining);
                if *remaining == 0 {
                    break;
                }
            }
            paragraph_start = paragraph_end + '\n'.len_utf8();
        }

        if cuts.is_empty() {
            return Ok((text.clone(), Cow::Borrowed(runs)));
        }
        let mut cut_text = text.to_string();
        let mut cut_runs = runs.to_vec();
        for cut in cuts.into_iter().rev() {
            replace_in_runs(&mut cut_runs, cut.clone(), truncation.affix.len());
            cut_text.replace_range(cut, truncation.affix);
        }
        Ok((cut_text.into(), Cow::Owned(cut_runs)))
    }
}

/// Makes `runs` fit the text after `range` of it is replaced by `replacement_len`
/// bytes. The replacement has the style of the run that `range` starts in, except
/// that it is the size of the line: it was measured as such, as the ellipsis of a
/// block is in CSS.
fn replace_in_runs(runs: &mut Vec<TextRun>, range: Range<usize>, replacement_len: usize) {
    let mut start = 0;
    let mut replaced = false;
    let mut out = Vec::with_capacity(runs.len() + 2);
    for run in runs.drain(..) {
        let end = start + run.len;
        let before = range.start.clamp(start, end) - start;
        let after = end - range.end.clamp(start, end);
        let owns_replacement = !replaced && (start..end).contains(&range.start);
        replaced |= owns_replacement;
        start = end;
        let sized = run.font_size.is_some() || run.line_height.is_some();
        if owns_replacement && sized {
            let mut push = |len: usize, run: TextRun| {
                if len > 0 {
                    out.push(TextRun { len, ..run });
                }
            };
            push(before, run.clone());
            push(
                replacement_len,
                TextRun {
                    font_size: None,
                    line_height: None,
                    ..run.clone()
                },
            );
            push(after, run);
        } else {
            let len = before + after + if owns_replacement { replacement_len } else { 0 };
            if len > 0 {
                out.push(TextRun { len, ..run });
            }
        }
    }
    *runs = out;
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate as gpui;
    use crate::{OverflowWrap, TabSize, TestAppContext, WordBreak, font};

    const AFFIX: &str = "\u{2026}";

    struct Scenario {
        text: &'static str,
        /// The width of a line, in the advance of a space.
        columns: f32,
        wraps: bool,
        clamp: Option<usize>,
        options: TextLayoutOptions,
    }

    impl Scenario {
        fn wrapped(text: &'static str, columns: f32, clamp: usize) -> Self {
            Self {
                text,
                columns,
                wraps: true,
                clamp: Some(clamp),
                options: TextLayoutOptions::default(),
            }
        }

        fn unwrapped(text: &'static str, columns: f32) -> Self {
            Self {
                text,
                columns,
                wraps: false,
                clamp: None,
                options: TextLayoutOptions::default(),
            }
        }

        fn with(mut self, options: TextLayoutOptions) -> Self {
            self.options = options;
            self
        }
    }

    struct Fixture {
        text_system: WindowTextSystem,
        space: Pixels,
    }

    impl Fixture {
        fn run(&self, len: usize) -> TextRun {
            TextRun {
                len,
                font: font("Menlo"),
                font_size: None,
                line_height: None,
                ..Default::default()
            }
        }

        fn width(&self, columns: f32) -> Pixels {
            // A quarter of a column of slack, so a line of exactly that many columns fits.
            self.space * (columns + 0.25)
        }

        fn rows(
            &self,
            text: &str,
            wrap_width: Option<Pixels>,
            options: TextLayoutOptions,
        ) -> Vec<String> {
            let lines = self
                .text_system
                .shape_text_with_options(
                    text.to_string().into(),
                    px(16.),
                    &[self.run(text.len())],
                    wrap_width,
                    None,
                    options,
                )
                .unwrap();
            lines
                .iter()
                .flat_map(|line| {
                    let mut starts = vec![0];
                    let mut ends = Vec::new();
                    for boundary in line.layout.wrap_boundaries() {
                        let index = line.layout.unwrapped_layout.runs[boundary.run_ix].glyphs
                            [boundary.glyph_ix]
                            .index;
                        starts.push(index);
                        ends.push(index);
                    }
                    ends.push(line.len());
                    starts
                        .into_iter()
                        .zip(ends)
                        .map(|(start, end)| line.text[start..end].to_string())
                        .collect::<Vec<_>>()
                })
                .collect()
        }

        fn truncate(&self, scenario: &Scenario) -> String {
            self.truncate_with_runs(scenario, &[]).0
        }

        /// The truncated text, and the length of each of its runs when the text is
        /// split into runs of `run_lens` bytes.
        fn truncate_with_runs(
            &self,
            scenario: &Scenario,
            run_lens: &[usize],
        ) -> (String, Vec<usize>) {
            let text: SharedString = scenario.text.into();
            let runs = if run_lens.is_empty() {
                vec![self.run(text.len())]
            } else {
                run_lens.iter().map(|len| self.run(*len)).collect()
            };
            let truncation = EndTruncation {
                font_size: px(16.),
                line_width: self.width(scenario.columns),
                wraps: scenario.wraps,
                max_lines: scenario.clamp,
                affix: AFFIX,
                affix_width: self.space,
                options: scenario.options,
                insets: &[],
            };
            let (text, runs) = self
                .text_system
                .truncate_end(&text, &runs, &truncation)
                .unwrap();
            (text.to_string(), runs.iter().map(|run| run.len).collect())
        }

        fn wrap_width(&self, scenario: &Scenario) -> Option<Pixels> {
            scenario.wraps.then(|| self.width(scenario.columns))
        }

        /// The truncated text keeps the rows the text has without a clamp, and
        /// ends the last of them with the affix when anything was cut.
        fn assert_keeps_the_rows_of_the_layout(&self, scenario: &Scenario) {
            let wrap_width = self.wrap_width(scenario);
            let clamp = scenario.clamp.unwrap();
            let all = self.rows(scenario.text, wrap_width, scenario.options);
            let truncated = self.truncate(scenario);
            let kept = self.rows(&truncated, wrap_width, scenario.options);
            let context = format!(
                "{:?} in {} columns, clamped to {clamp}, truncated to {truncated:?}, rows {all:?} then {kept:?}",
                scenario.text, scenario.columns
            );
            assert_eq!(kept.len(), all.len().min(clamp), "{context}");
            let last = kept.len() - 1;
            assert_eq!(kept[..last], all[..last], "{context}");
            let body = kept[last].strip_suffix(AFFIX).unwrap_or(&kept[last]);
            assert!(all[last].starts_with(body), "{context}");
            if all.len() > clamp {
                assert!(kept[last].ends_with(AFFIX), "{context}");
            }
        }
    }

    fn with_fixture(cx: &mut TestAppContext, test: impl FnOnce(&Fixture)) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let space = cx
                .text_system()
                .advance(cx.text_system().resolve_font(&font("Menlo")), px(16.), ' ')
                .unwrap()
                .width;
            test(&Fixture { text_system, space });
        });
    }

    fn tabs(spaces: f32) -> TextLayoutOptions {
        TextLayoutOptions {
            tab_size: Some(TabSize::Spaces(spaces)),
            ..Default::default()
        }
    }

    fn hanging() -> TextLayoutOptions {
        TextLayoutOptions {
            hang_trailing_spaces: true,
            ..Default::default()
        }
    }

    fn breaking_spaces() -> TextLayoutOptions {
        TextLayoutOptions {
            break_spaces: true,
            ..Default::default()
        }
    }

    fn word_break(word_break: WordBreak) -> TextLayoutOptions {
        TextLayoutOptions {
            word_break,
            ..Default::default()
        }
    }

    fn overflow_wrap(overflow_wrap: OverflowWrap) -> TextLayoutOptions {
        TextLayoutOptions {
            overflow_wrap,
            ..Default::default()
        }
    }

    /// What gpuix lays out text with for `white-space: normal` and `tab-size: 4`.
    fn css() -> TextLayoutOptions {
        TextLayoutOptions {
            tab_size: Some(TabSize::Spaces(4.)),
            hang_trailing_spaces: true,
            overflow_wrap: OverflowWrap::Normal,
            ..Default::default()
        }
    }

    #[crate::test]
    fn a_tab_is_measured_to_its_stop_when_clamping_lines(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            // Columns: aa, tab to 4, bb, tab to 8, cc, space, then dd would end at 13.
            let scenario =
                Scenario::wrapped("aa\tbb\tcc dd ee ff gg hh ii jj", 12., 2).with(tabs(4.));
            assert_eq!(
                fixture.truncate(&scenario),
                "aa\tbb\tcc dd ee ff gg\u{2026}"
            );
        });
    }

    #[crate::test]
    fn a_tab_is_measured_to_its_stop_in_a_line_that_does_not_wrap(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            // c is at column 8, the tab after it at 9, and d at 12.
            let scenario = Scenario::unwrapped("a\tb\tc\td\te", 10.).with(tabs(4.));
            assert_eq!(fixture.truncate(&scenario), "a\tb\tc\u{2026}");
        });
    }

    #[crate::test]
    fn text_that_fits_with_its_tabs_is_not_cut(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::unwrapped("a\tb\tc", 9.).with(tabs(4.));
            assert_eq!(fixture.truncate(&scenario), "a\tb\tc");
        });
    }

    #[crate::test]
    fn spaces_at_a_soft_wrap_do_not_count_against_the_line(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            // The first line is aaaaaa and three spaces that hang past the edge.
            let scenario = Scenario::wrapped("aaaaaa   bbbbbb cc", 8., 2).with(hanging());
            assert_eq!(fixture.truncate(&scenario), "aaaaaa   bbbbbb\u{2026}");
        });
    }

    #[crate::test]
    fn a_hanging_space_is_not_kept_before_the_affix(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("aaaa bbbb cccc", 5., 1).with(hanging());
            assert_eq!(fixture.truncate(&scenario), "aaaa\u{2026}");
        });
    }

    #[crate::test]
    fn a_line_breaks_after_every_space_when_clamping_lines(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            // Each space may end a line, so the second line is the space after abcd, ef and a space.
            let scenario = Scenario::wrapped("abcd  ef gh ij", 5., 2).with(breaking_spaces());
            assert_eq!(fixture.truncate(&scenario), "abcd  ef\u{2026}");
        });
    }

    #[crate::test]
    fn break_all_fills_each_line_to_the_edge(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            // ab cd / efghi / j: the lines break between letters, not at the space.
            let scenario =
                Scenario::wrapped("ab cdefghij", 5., 2).with(word_break(WordBreak::BreakAll));
            assert_eq!(fixture.truncate(&scenario), "ab cdefgh\u{2026}");
        });
    }

    #[crate::test]
    fn a_word_that_does_not_fit_overflows_instead_of_being_cut(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            // The first line is the whole word, and the text takes two lines.
            let scenario = Scenario::wrapped("abcdefghij klm", 5., 2)
                .with(overflow_wrap(OverflowWrap::Normal));
            assert_eq!(fixture.truncate(&scenario), "abcdefghij klm");
        });
    }

    #[crate::test]
    fn an_overflowing_word_on_the_last_line_is_cut_at_a_glyph(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("abcdefghij klm", 5., 1)
                .with(overflow_wrap(OverflowWrap::Normal));
            assert_eq!(fixture.truncate(&scenario), "abcd\u{2026}");
        });
    }

    #[crate::test]
    fn a_long_last_word_that_overflows_is_cut_at_a_glyph(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario =
                Scenario::wrapped("ab cdefghij", 5., 2).with(overflow_wrap(OverflowWrap::Normal));
            assert_eq!(fixture.truncate(&scenario), "ab cdef\u{2026}");
        });
    }

    #[crate::test]
    fn a_newline_ends_a_line_of_the_clamp(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("hello\nworld foo bar baz", 7., 2);
            assert_eq!(fixture.truncate(&scenario), "hello\nworld\u{2026}");
        });
    }

    #[crate::test]
    fn content_after_the_last_line_cuts_it(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("hello\nworld\nmore", 7., 2);
            assert_eq!(fixture.truncate(&scenario), "hello\nworld\u{2026}");
        });
    }

    #[crate::test]
    fn a_last_line_that_fits_with_the_affix_keeps_all_its_text(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("hello\nwor\nmore", 7., 2);
            assert_eq!(fixture.truncate(&scenario), "hello\nwor\u{2026}");
        });
    }

    #[crate::test]
    fn white_space_after_the_last_line_does_not_cut_it(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            for text in ["hello\nworld\n", "hello\nworld", "hello\nworld\n  \n"] {
                let scenario = Scenario::wrapped(text, 7., 2);
                assert_eq!(fixture.truncate(&scenario), text);
            }
        });
    }

    #[crate::test]
    fn text_in_fewer_lines_than_the_clamp_is_not_cut(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("aa bbb cccccc", 8., 3);
            assert_eq!(fixture.truncate(&scenario), "aa bbb cccccc");
        });
    }

    #[crate::test]
    fn each_line_of_text_that_does_not_wrap_is_cut_on_its_own(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::unwrapped("abcdefghij\nxy\nklmnopqrstu", 6.);
            assert_eq!(
                fixture.truncate(&scenario),
                "abcde\u{2026}\nxy\nklmno\u{2026}"
            );
        });
    }

    #[crate::test]
    fn a_paragraph_that_wraps_without_a_clamp_is_cut_to_a_line(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario {
                clamp: None,
                ..Scenario::wrapped("aaaa bbbb cccc\ndd", 9., 1).with(hanging())
            };
            assert_eq!(fixture.truncate(&scenario), "aaaa bbb\u{2026}\ndd");
        });
    }

    #[crate::test]
    fn the_affix_takes_the_style_of_the_run_it_cuts(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::wrapped("ab cd ef gh ij", 7., 1);
            let (text, runs) = fixture.truncate_with_runs(&scenario, &[3, 5, 6]);
            assert_eq!(text, "ab cd\u{2026}");
            assert_eq!(runs, [3, 5]);
        });
    }

    #[crate::test]
    fn runs_after_a_cut_in_the_middle_of_the_text_stay(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let scenario = Scenario::unwrapped("abcdefghij\nxy", 6.);
            let (text, runs) = fixture.truncate_with_runs(&scenario, &[4, 6, 3]);
            assert_eq!(text, "abcde\u{2026}\nxy");
            assert_eq!(runs, [4, 4, 3]);
        });
    }

    #[crate::test]
    fn the_affix_is_the_size_of_the_line_whatever_the_run_it_replaces(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let text: SharedString = "aaaaBBBBBBBB".into();
            let run = |len, font_size| TextRun {
                font_size,
                ..fixture.run(len)
            };
            let runs = [run(4, None), run(8, Some(px(32.)))];
            // The B's are two spaces wide each. The affix measured at the size of the
            // line is a space, and in the size of the B's it would be two.
            let line_width = fixture.space * 9.5;
            let truncation = EndTruncation {
                font_size: px(16.),
                line_width,
                wraps: false,
                max_lines: None,
                affix: AFFIX,
                affix_width: fixture.space,
                options: TextLayoutOptions::default(),
                insets: &[],
            };
            let (cut, cut_runs) = fixture
                .text_system
                .truncate_end(&text, &runs, &truncation)
                .unwrap();
            assert_eq!(cut, "aaaaBB\u{2026}");
            assert_eq!(cut_runs.iter().map(|run| run.len).sum::<usize>(), cut.len());
            let lines = fixture
                .text_system
                .shape_text(cut.clone(), px(16.), &cut_runs, None, None)
                .unwrap();
            assert!(
                lines[0].layout.width() <= line_width,
                "{cut:?} is {:?} wide in {line_width:?}",
                lines[0].layout.width()
            );
            let affix = cut_runs.last().unwrap();
            assert_eq!(affix.len, AFFIX.len());
            assert_eq!(affix.font_size, None);
        });
    }

    #[crate::test]
    fn the_rows_kept_are_the_rows_of_the_layout(cx: &mut TestAppContext) {
        with_fixture(cx, |fixture| {
            let texts = [
                "aa\tbb\tcc dd ee ff gg hh ii jj kk ll mm nn",
                "aaaaaa   bbbbbb cc dd   ee ff gg hh ii jj kk",
                "abcdefghij klm nopqrstuvwxyz abc def ghi jkl mno",
                "ab      cd   ef  gh ij kl mn op qr st uv wx yz",
                "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c} abc \u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d} def ghi",
            ];
            let options = [
                TextLayoutOptions::default(),
                tabs(4.),
                hanging(),
                breaking_spaces(),
                word_break(WordBreak::BreakAll),
                word_break(WordBreak::KeepAll),
                overflow_wrap(OverflowWrap::Normal),
                css(),
                TextLayoutOptions {
                    word_break: WordBreak::BreakAll,
                    ..css()
                },
            ];
            for text in texts {
                for options in options {
                    // Spaces that count in the width can push a word to the next
                    // line, and cutting the spaces after it takes that away.
                    if text.contains("  ")
                        && !(options.hang_trailing_spaces || options.break_spaces)
                    {
                        continue;
                    }
                    for columns in [6., 9., 12.] {
                        for clamp in [1, 2, 3] {
                            fixture.assert_keeps_the_rows_of_the_layout(
                                &Scenario::wrapped(text, columns, clamp).with(options),
                            );
                        }
                    }
                }
            }
        });
    }
}
