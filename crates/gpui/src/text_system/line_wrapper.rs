use crate::{FontId, Pixels, SharedString, TextRun, TextSystem, px};
use collections::HashMap;
use std::{borrow::Cow, iter, sync::Arc};

/// Determines whether to truncate text from the start or end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TruncateFrom {
    /// Truncate text from the start.
    Start,
    /// Truncate text from the end.
    End,
    /// Truncate text from the middle, preserving the start and end.
    Middle,
}

/// The GPUI line wrapper, used to wrap lines of text to a given width.
pub struct LineWrapper {
    text_system: Arc<TextSystem>,
    pub(crate) font_id: FontId,
    pub(crate) font_size: Pixels,
    cached_ascii_char_widths: [Option<Pixels>; 128],
    cached_other_char_widths: HashMap<char, Pixels>,
}

impl LineWrapper {
    /// The maximum indent that can be applied to a line.
    pub const MAX_INDENT: u32 = 256;

    pub(crate) fn new(font_id: FontId, font_size: Pixels, text_system: Arc<TextSystem>) -> Self {
        Self {
            text_system,
            font_id,
            font_size,
            cached_ascii_char_widths: [None; 128],
            cached_other_char_widths: HashMap::default(),
        }
    }

    /// Wrap a line of text to the given width with this wrapper's font and font size.
    pub fn wrap_line<'a>(
        &'a mut self,
        fragments: &'a [LineFragment],
        wrap_width: Pixels,
    ) -> impl Iterator<Item = Boundary> + 'a {
        let mut width = px(0.);
        let mut first_non_whitespace_ix = None;
        let mut indent = None;
        let mut last_candidate_ix = 0;
        let mut last_candidate_width = px(0.);
        let mut last_wrap_ix = 0;
        let mut prev_c = '\0';
        let mut index = 0;
        let mut candidates = fragments
            .iter()
            .flat_map(move |fragment| fragment.wrap_boundary_candidates())
            .peekable();
        iter::from_fn(move || {
            for candidate in candidates.by_ref() {
                let ix = index;
                index += candidate.len_utf8();
                let mut new_prev_c = prev_c;
                let item_width = match candidate {
                    WrapBoundaryCandidate::Char { character: c } => {
                        if c == '\n' {
                            continue;
                        }

                        if Self::is_word_char(c) {
                            if prev_c == ' ' && c != ' ' && first_non_whitespace_ix.is_some() {
                                last_candidate_ix = ix;
                                last_candidate_width = width;
                            }
                        } else {
                            // CJK may not be space separated, e.g.: `Hello world你好世界`
                            if c != ' ' && first_non_whitespace_ix.is_some() {
                                last_candidate_ix = ix;
                                last_candidate_width = width;
                            }
                        }

                        if c != ' ' && first_non_whitespace_ix.is_none() {
                            first_non_whitespace_ix = Some(ix);
                        }

                        new_prev_c = c;

                        self.width_for_char(c)
                    }
                    WrapBoundaryCandidate::Element {
                        width: element_width,
                        ..
                    } => {
                        if prev_c == ' ' && first_non_whitespace_ix.is_some() {
                            last_candidate_ix = ix;
                            last_candidate_width = width;
                        }

                        if first_non_whitespace_ix.is_none() {
                            first_non_whitespace_ix = Some(ix);
                        }

                        element_width
                    }
                };

                width += item_width;
                if width > wrap_width && ix > last_wrap_ix {
                    if let (None, Some(first_non_whitespace_ix)) = (indent, first_non_whitespace_ix)
                    {
                        indent = Some(
                            Self::MAX_INDENT.min((first_non_whitespace_ix - last_wrap_ix) as u32),
                        );
                    }

                    if last_candidate_ix > 0 {
                        last_wrap_ix = last_candidate_ix;
                        width -= last_candidate_width;
                        last_candidate_ix = 0;
                    } else {
                        last_wrap_ix = ix;
                        width = item_width;
                    }

                    if let Some(indent) = indent {
                        width += self.width_for_char(' ') * indent as f32;
                    }

                    prev_c = new_prev_c;
                    return Some(Boundary::new(last_wrap_ix, indent.unwrap_or(0)));
                }

                prev_c = new_prev_c;
            }

            None
        })
    }

    /// The width of the widest stretch of `text` between two places `wrap_line` can break, which
    /// is its CSS `min-content` width: wrapping at it breaks no word. Trailing spaces count,
    /// because `wrap_line` counts them.
    pub fn min_content_width(&mut self, text: &str) -> Pixels {
        let mut widest = px(0.);
        let mut stretch = px(0.);
        let mut prev_c = '\0';
        let mut seen_non_whitespace = false;
        for c in text.chars() {
            if c == '\n' {
                widest = widest.max(stretch);
                stretch = px(0.);
                prev_c = '\0';
                seen_non_whitespace = false;
                continue;
            }
            let breaks_before = seen_non_whitespace
                && c != ' '
                && (!Self::is_word_char(c) || prev_c == ' ');
            if breaks_before {
                widest = widest.max(stretch);
                stretch = px(0.);
            }
            seen_non_whitespace |= c != ' ';
            stretch += self.width_for_char(c);
            prev_c = c;
        }
        widest.max(stretch)
    }

    /// Determines if a line should be truncated based on its width.
    ///
    /// Returns the truncation index in `line`.
    pub fn should_truncate_line(
        &mut self,
        line: &str,
        truncate_width: Pixels,
        truncation_affix: &str,
        truncate_from: TruncateFrom,
    ) -> Option<usize> {
        let mut width = px(0.);
        let suffix_width = truncation_affix
            .chars()
            .map(|c| self.width_for_char(c))
            .fold(px(0.0), |a, x| a + x);
        let mut truncate_ix = 0;

        match truncate_from {
            TruncateFrom::Start => {
                for (ix, c) in line.char_indices().rev() {
                    if width + suffix_width < truncate_width {
                        truncate_ix = ix;
                    }

                    let char_width = self.width_for_char(c);
                    width += char_width;

                    if width.floor() > truncate_width {
                        return Some(truncate_ix);
                    }
                }
            }
            TruncateFrom::End => {
                for (ix, c) in line.char_indices() {
                    if width + suffix_width < truncate_width {
                        truncate_ix = ix;
                    }

                    let char_width = self.width_for_char(c);
                    width += char_width;

                    if width.floor() > truncate_width {
                        return Some(truncate_ix);
                    }
                }
            }
            TruncateFrom::Middle => {}
        }

        None
    }

    fn should_truncate_line_middle(
        &mut self,
        line: &str,
        truncate_width: Pixels,
        truncation_affix: &str,
    ) -> Option<(usize, usize)> {
        let suffix_width = truncation_affix
            .chars()
            .map(|c| self.width_for_char(c))
            .fold(px(0.0), |a, x| a + x);

        let total_width: Pixels = line
            .chars()
            .map(|c| self.width_for_char(c))
            .fold(px(0.0), |a, x| a + x);

        if total_width <= truncate_width {
            return None;
        }

        let content_budget = truncate_width - suffix_width;
        if content_budget <= px(0.) {
            return Some((0, line.len()));
        }

        let front_budget = content_budget * (2.0 / 3.0);
        let back_budget = content_budget - front_budget;

        let mut front_width = px(0.);
        let mut front_end_ix = 0usize;
        for (ix, c) in line.char_indices() {
            let char_width = self.width_for_char(c);
            if front_width + char_width > front_budget {
                break;
            }
            front_width += char_width;
            front_end_ix = ix + c.len_utf8();
        }

        let mut back_width = px(0.);
        let mut back_start_ix = line.len();
        for (ix, c) in line.char_indices().rev() {
            let char_width = self.width_for_char(c);
            if back_width + char_width > back_budget {
                break;
            }
            back_width += char_width;
            back_start_ix = ix;
        }

        if front_end_ix >= back_start_ix {
            return Some((0, line.len()));
        }

        Some((front_end_ix, back_start_ix))
    }

    /// Truncate a line of text to the given width with this wrapper's font and font size.
    pub fn truncate_line<'a>(
        &mut self,
        line: SharedString,
        truncate_width: Pixels,
        truncation_affix: &str,
        runs: &'a [TextRun],
        truncate_from: TruncateFrom,
    ) -> (SharedString, Cow<'a, [TextRun]>) {
        if truncate_from == TruncateFrom::Middle {
            if let Some((front_end_ix, back_start_ix)) =
                self.should_truncate_line_middle(&line, truncate_width, truncation_affix)
            {
                let result = SharedString::from(format!(
                    "{}{truncation_affix}{}",
                    &line[..front_end_ix],
                    &line[back_start_ix..]
                ));
                let mut runs = runs.to_vec();
                update_runs_after_middle_truncation(
                    truncation_affix,
                    &mut runs,
                    front_end_ix,
                    back_start_ix,
                );
                return (result, Cow::Owned(runs));
            } else {
                return (line, Cow::Borrowed(runs));
            }
        }

        if let Some(truncate_ix) =
            self.should_truncate_line(&line, truncate_width, truncation_affix, truncate_from)
        {
            let result = match truncate_from {
                TruncateFrom::Start => SharedString::from(format!(
                    "{truncation_affix}{}",
                    &line[line.ceil_char_boundary(truncate_ix + 1)..]
                )),
                TruncateFrom::End => SharedString::from(format!(
                    "{}{truncation_affix}",
                    line[..truncate_ix]
                        .trim_end_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
                )),
                TruncateFrom::Middle => unreachable!("Middle truncation is handled above"),
            };
            let mut runs = runs.to_vec();
            update_runs_after_truncation(&result, truncation_affix, &mut runs, truncate_from);
            (result, Cow::Owned(runs))
        } else {
            (line, Cow::Borrowed(runs))
        }
    }

    /// Any character in this list should be treated as a word character,
    /// meaning it can be part of a word that should not be wrapped.
    pub(crate) fn is_word_char(c: char) -> bool {
        // ASCII alphanumeric characters, for English, numbers: `Hello123`, etc.
        c.is_ascii_alphanumeric() ||
        // Latin script in Unicode for French, German, Spanish, etc.
        // Latin-1 Supplement
        // https://en.wikipedia.org/wiki/Latin-1_Supplement
        matches!(c, '\u{00C0}'..='\u{00FF}') ||
        // Latin Extended-A
        // https://en.wikipedia.org/wiki/Latin_Extended-A
        matches!(c, '\u{0100}'..='\u{017F}') ||
        // Latin Extended-B
        // https://en.wikipedia.org/wiki/Latin_Extended-B
        matches!(c, '\u{0180}'..='\u{024F}') ||
        // Cyrillic for Russian, Ukrainian, etc.
        // https://en.wikipedia.org/wiki/Cyrillic_script_in_Unicode
        matches!(c, '\u{0400}'..='\u{04FF}') ||

        // Vietnamese (https://vietunicode.sourceforge.net/charset/)
        matches!(c, '\u{1E00}'..='\u{1EFF}') || // Latin Extended Additional
        matches!(c, '\u{0300}'..='\u{036F}') || // Combining Diacritical Marks

        // Bengali (https://en.wikipedia.org/wiki/Bengali_(Unicode_block))
        matches!(c, '\u{0980}'..='\u{09FF}') ||

        // Some other known special characters that should be treated as word characters,
        // e.g. `a-b`, `var_name`, `I'm`/`won’t`, '@mention`, `#hashtag`, `100%`, `3.1415`,
        // `2^3`, `a~b`, `a=1`, `Self::new`, etc. Trailing punctuation like `,`, `.`, `:`, `;`
        // is included so it stays attached to the preceding word when wrapping.
        matches!(c, '-' | '_' | '.' | '\'' | '’' | '‘' | '$' | '%' | '@' | '#' | '^' | '~' | ',' | '=' | ':' | ';') ||
        // Closing punctuation never starts a line (UAX #14 LB13: no break
        // before `!`, `)`, `]`, `}`, closing quotes or an ellipsis) — `plz!`,
        // `see)`, `quoted”` wrap as one word instead of orphaning the mark on
        // the next line. `/` and `?` stay break opportunities so long paths
        // and URLs (`a/b`, `foo?b=2`) can wrap.
        matches!(c, '!' | ')' | ']' | '}' | '"' | '”' | '»' | '…') ||
        // `⋯` character is special used in Zed, to keep this at the end of the line.
        matches!(c, '⋯') ||

        // Non-breaking glue characters
        matches!(c, '\u{202F}' | '\u{00A0}' | '\u{2011}')
    }

    /// The sum of the advances of the characters of `text`.
    pub(crate) fn width_of(&mut self, text: &str) -> Pixels {
        text.chars().map(|c| self.width_for_char(c)).sum()
    }

    #[inline(always)]
    fn width_for_char(&mut self, c: char) -> Pixels {
        if (c as u32) < 128 {
            if let Some(cached_width) = self.cached_ascii_char_widths[c as usize] {
                cached_width
            } else {
                let width = self
                    .text_system
                    .layout_width(self.font_id, self.font_size, c);
                self.cached_ascii_char_widths[c as usize] = Some(width);
                width
            }
        } else if let Some(cached_width) = self.cached_other_char_widths.get(&c) {
            *cached_width
        } else {
            let width = self
                .text_system
                .layout_width(self.font_id, self.font_size, c);
            self.cached_other_char_widths.insert(c, width);
            width
        }
    }
}

fn update_runs_after_truncation(
    result: &str,
    ellipsis: &str,
    runs: &mut Vec<TextRun>,
    truncate_from: TruncateFrom,
) {
    let mut truncate_at = result.len() - ellipsis.len();
    match truncate_from {
        TruncateFrom::Start => {
            for (run_index, run) in runs.iter_mut().enumerate().rev() {
                if run.len <= truncate_at {
                    truncate_at -= run.len;
                } else {
                    run.len = truncate_at + ellipsis.len();
                    runs.splice(..run_index, std::iter::empty());
                    break;
                }
            }
        }
        TruncateFrom::End => {
            for (run_index, run) in runs.iter_mut().enumerate() {
                if run.len <= truncate_at {
                    truncate_at -= run.len;
                } else {
                    run.len = truncate_at + ellipsis.len();
                    runs.truncate(run_index + 1);
                    break;
                }
            }
        }
        TruncateFrom::Middle => {
            unreachable!("Middle truncation calls this function with TruncateFrom::End directly")
        }
    }
}

fn update_runs_after_middle_truncation(
    ellipsis: &str,
    runs: &mut Vec<TextRun>,
    front_end_ix: usize,
    back_start_ix: usize,
) {
    let original_runs = std::mem::take(runs);
    let mut result_runs: Vec<TextRun> = Vec::with_capacity(original_runs.len());

    // Front segment [0, front_end_ix) + ellipsis: walk forward until the run
    // that straddles or ends at front_end_ix, then extend that run's length
    // to include the ellipsis.
    let mut front_remaining = front_end_ix;
    let mut front_done = false;
    for run in &original_runs {
        if front_done {
            break;
        }
        if run.len <= front_remaining {
            result_runs.push(run.clone());
            front_remaining -= run.len;
        } else {
            let mut partial = run.clone();
            partial.len = front_remaining + ellipsis.len();
            result_runs.push(partial);
            front_done = true;
        }
    }
    if !front_done {
        // front_end_ix landed exactly on a run boundary; append ellipsis to
        // the last front run (or, if the front is empty, to the first back run).
        if let Some(last) = result_runs.last_mut() {
            last.len += ellipsis.len();
        } else if let Some(first) = original_runs.first() {
            let mut affix_run = first.clone();
            affix_run.len = ellipsis.len();
            result_runs.push(affix_run);
        }
    }

    // Back segment [back_start_ix, original.len()): skip runs entirely in the
    // removed middle, keep the rest.
    let mut byte_pos = 0usize;
    for run in &original_runs {
        let run_end = byte_pos + run.len;
        if run_end > back_start_ix {
            if byte_pos < back_start_ix {
                // Run straddles back_start_ix; keep only the tail.
                let mut partial = run.clone();
                partial.len = run_end - back_start_ix;
                result_runs.push(partial);
            } else {
                result_runs.push(run.clone());
            }
        }
        byte_pos = run_end;
    }

    *runs = result_runs;
}

/// A fragment of a line that can be wrapped.
pub enum LineFragment<'a> {
    /// A text fragment consisting of characters.
    Text {
        /// The text content of the fragment.
        text: &'a str,
    },
    /// A non-text element with a fixed width.
    Element {
        /// The width of the element in pixels.
        width: Pixels,
        /// The UTF-8 encoded length of the element.
        len_utf8: usize,
    },
}

impl<'a> LineFragment<'a> {
    /// Creates a new text fragment from the given text.
    pub fn text(text: &'a str) -> Self {
        LineFragment::Text { text }
    }

    /// Creates a new non-text element with the given width and UTF-8 encoded length.
    pub fn element(width: Pixels, len_utf8: usize) -> Self {
        LineFragment::Element { width, len_utf8 }
    }

    fn wrap_boundary_candidates(&self) -> impl Iterator<Item = WrapBoundaryCandidate> {
        let text = match self {
            LineFragment::Text { text } => text,
            LineFragment::Element { .. } => "\0",
        };
        text.chars().map(move |character| {
            if let LineFragment::Element { width, len_utf8 } = self {
                WrapBoundaryCandidate::Element {
                    width: *width,
                    len_utf8: *len_utf8,
                }
            } else {
                WrapBoundaryCandidate::Char { character }
            }
        })
    }
}

enum WrapBoundaryCandidate {
    Char { character: char },
    Element { width: Pixels, len_utf8: usize },
}

impl WrapBoundaryCandidate {
    pub fn len_utf8(&self) -> usize {
        match self {
            WrapBoundaryCandidate::Char { character } => character.len_utf8(),
            WrapBoundaryCandidate::Element { len_utf8: len, .. } => *len,
        }
    }
}

/// A boundary between two lines of text.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// The index of the last character in a line
    pub ix: usize,
    /// The indent of the next line.
    pub next_indent: u32,
}

impl Boundary {
    fn new(ix: usize, next_indent: u32) -> Self {
        Self { ix, next_indent }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Font, FontFeatures, FontStyle, FontWeight, TestAppContext, TestDispatcher, font};
    #[cfg(target_os = "macos")]
    use crate::{
        OverflowWrap, TabSize, TextLayoutOptions, TextRun, WindowTextSystem, WordBreak,
        WrapBoundary, WrappedLine, point,
    };

    fn build_wrapper() -> LineWrapper {
        let dispatcher = TestDispatcher::new(0);
        let cx = TestAppContext::build(dispatcher, None);
        let id = cx.text_system().resolve_font(&font(".ZedMono"));
        LineWrapper::new(id, px(16.), cx.text_system().clone())
    }

    fn generate_test_runs(input_run_len: &[usize]) -> Vec<TextRun> {
        input_run_len
            .iter()
            .map(|run_len| TextRun {
                len: *run_len,
                font: Font {
                    family: "Dummy".into(),
                    features: FontFeatures::default(),
                    fallbacks: None,
                    weight: FontWeight::default(),
                    style: FontStyle::Normal,
                },
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn test_min_content_width_is_the_widest_stretch_between_break_opportunities() {
        let mut wrapper = build_wrapper();
        let char_width = wrapper.width_for_char('a');
        let mut chars = |text: &str, count: f32| {
            let width = wrapper.min_content_width(text);
            assert!(
                (width - char_width * count).abs() < px(0.01),
                "{text:?} is {width:?} wide, expected {count} characters"
            );
        };

        chars("", 0.);
        chars("abc", 3.);
        // The trailing space belongs to the word it follows.
        chars("aa bbb cccc ddddd eeee", 6.);
        chars("  ab cd", 5.);
        chars("abc\nde fg", 3.);

        let text = "aa bbb cccc ddddd eeee";
        let narrowest = wrapper.min_content_width(text);
        let breaks: Vec<_> = wrapper
            .wrap_line(&[LineFragment::text(text)], narrowest)
            .map(|boundary| boundary.ix)
            .collect();
        assert!(
            breaks
                .iter()
                .all(|ix| text.as_bytes()[*ix - 1] == b' ' && text.as_bytes()[*ix] != b' '),
            "wrapping at the min-content width broke a word: {breaks:?}"
        );
    }

    #[test]
    fn test_wrap_line() {
        let mut wrapper = build_wrapper();

        assert_eq!(
            wrapper
                .wrap_line(&[LineFragment::text("aa bbb cccc ddddd eeee")], px(72.))
                .collect::<Vec<_>>(),
            &[
                Boundary::new(7, 0),
                Boundary::new(12, 0),
                Boundary::new(18, 0)
            ],
        );
        assert_eq!(
            wrapper
                .wrap_line(&[LineFragment::text("aaa aaaaaaaaaaaaaaaaaa")], px(72.0))
                .collect::<Vec<_>>(),
            &[
                Boundary::new(4, 0),
                Boundary::new(11, 0),
                Boundary::new(18, 0)
            ],
        );
        assert_eq!(
            wrapper
                .wrap_line(&[LineFragment::text("     aaaaaaa")], px(72.))
                .collect::<Vec<_>>(),
            &[
                Boundary::new(7, 5),
                Boundary::new(9, 5),
                Boundary::new(11, 5),
            ]
        );
        assert_eq!(
            wrapper
                .wrap_line(
                    &[LineFragment::text("                            ")],
                    px(72.)
                )
                .collect::<Vec<_>>(),
            &[
                Boundary::new(7, 0),
                Boundary::new(14, 0),
                Boundary::new(21, 0)
            ]
        );
        assert_eq!(
            wrapper
                .wrap_line(&[LineFragment::text("          aaaaaaaaaaaaaa")], px(72.))
                .collect::<Vec<_>>(),
            &[
                Boundary::new(7, 0),
                Boundary::new(14, 3),
                Boundary::new(18, 3),
                Boundary::new(22, 3),
            ]
        );

        // Test wrapping multiple text fragments
        assert_eq!(
            wrapper
                .wrap_line(
                    &[
                        LineFragment::text("aa bbb "),
                        LineFragment::text("cccc ddddd eeee")
                    ],
                    px(72.)
                )
                .collect::<Vec<_>>(),
            &[
                Boundary::new(7, 0),
                Boundary::new(12, 0),
                Boundary::new(18, 0)
            ],
        );

        // Test wrapping with a mix of text and element fragments
        assert_eq!(
            wrapper
                .wrap_line(
                    &[
                        LineFragment::text("aa "),
                        LineFragment::element(px(20.), 1),
                        LineFragment::text(" bbb "),
                        LineFragment::element(px(30.), 1),
                        LineFragment::text(" cccc")
                    ],
                    px(72.)
                )
                .collect::<Vec<_>>(),
            &[
                Boundary::new(5, 0),
                Boundary::new(9, 0),
                Boundary::new(11, 0)
            ],
        );

        // Test with element at the beginning and text afterward
        assert_eq!(
            wrapper
                .wrap_line(
                    &[
                        LineFragment::element(px(50.), 1),
                        LineFragment::text(" aaaa bbbb cccc dddd")
                    ],
                    px(72.)
                )
                .collect::<Vec<_>>(),
            &[
                Boundary::new(2, 0),
                Boundary::new(7, 0),
                Boundary::new(12, 0),
                Boundary::new(17, 0)
            ],
        );

        // Test with a large element that forces wrapping by itself
        assert_eq!(
            wrapper
                .wrap_line(
                    &[
                        LineFragment::text("short text "),
                        LineFragment::element(px(100.), 1),
                        LineFragment::text(" more text")
                    ],
                    px(72.)
                )
                .collect::<Vec<_>>(),
            &[
                Boundary::new(6, 0),
                Boundary::new(11, 0),
                Boundary::new(12, 0),
                Boundary::new(18, 0)
            ],
        );

        // Test with non-breaking glue characters
        assert_eq!(
            wrapper
                .wrap_line(
                    &[LineFragment::text("a\u{202F}b\u{00A0}c\u{2011}d e")],
                    px(72.0)
                )
                .collect::<Vec<_>>(),
            &[Boundary::new(12, 0),], // special chars above take up 3, 2 and 3 bytes, so boundary ends up at 12
        );
    }

    #[test]
    fn test_truncate_line_end() {
        let mut wrapper = build_wrapper();

        fn perform_test(
            wrapper: &mut LineWrapper,
            text: &'static str,
            expected: &'static str,
            ellipsis: &str,
        ) {
            let dummy_run_lens = vec![text.len()];
            let dummy_runs = generate_test_runs(&dummy_run_lens);
            let (result, dummy_runs) = wrapper.truncate_line(
                text.into(),
                px(220.),
                ellipsis,
                &dummy_runs,
                TruncateFrom::End,
            );
            assert_eq!(result, expected);
            assert_eq!(dummy_runs.first().unwrap().len, result.len());
        }

        perform_test(
            &mut wrapper,
            "aa bbb cccc ddddd eeee ffff gggg",
            "aa bbb cccc ddddd eeee",
            "",
        );
        perform_test(
            &mut wrapper,
            "aa bbb cccc ddddd eeee ffff gggg",
            "aa bbb cccc ddddd eee…",
            "…",
        );
        perform_test(
            &mut wrapper,
            "aa bbb cccc ddddd eeee ffff gggg",
            "aa bbb cccc dddd......",
            "......",
        );
        perform_test(
            &mut wrapper,
            "aa bbb cccc 🦀🦀🦀🦀🦀 eeee ffff gggg",
            "aa bbb cccc 🦀🦀🦀🦀…",
            "…",
        );
    }

    #[test]
    fn test_truncate_line_start() {
        let mut wrapper = build_wrapper();

        #[track_caller]
        fn perform_test(
            wrapper: &mut LineWrapper,
            text: &'static str,
            expected: &'static str,
            ellipsis: &str,
        ) {
            let dummy_run_lens = vec![text.len()];
            let dummy_runs = generate_test_runs(&dummy_run_lens);
            let (result, dummy_runs) = wrapper.truncate_line(
                text.into(),
                px(220.),
                ellipsis,
                &dummy_runs,
                TruncateFrom::Start,
            );
            assert_eq!(result, expected);
            assert_eq!(dummy_runs.first().unwrap().len, result.len());
        }

        perform_test(
            &mut wrapper,
            "aaaa bbbb cccc ddddd eeee fff gg",
            "cccc ddddd eeee fff gg",
            "",
        );
        perform_test(
            &mut wrapper,
            "aaaa bbbb cccc ddddd eeee fff gg",
            "…ccc ddddd eeee fff gg",
            "…",
        );
        perform_test(
            &mut wrapper,
            "aaaa bbbb cccc ddddd eeee fff gg",
            "......dddd eeee fff gg",
            "......",
        );
        perform_test(
            &mut wrapper,
            "aaaa bbbb cccc 🦀🦀🦀🦀🦀 eeee fff gg",
            "…🦀🦀🦀🦀 eeee fff gg",
            "…",
        );
    }

    #[test]
    fn test_truncate_multiple_runs_end() {
        let mut wrapper = build_wrapper();

        fn perform_test(
            wrapper: &mut LineWrapper,
            text: &'static str,
            expected: &str,
            run_lens: &[usize],
            result_run_len: &[usize],
            line_width: Pixels,
        ) {
            let dummy_runs = generate_test_runs(run_lens);
            let (result, dummy_runs) =
                wrapper.truncate_line(text.into(), line_width, "…", &dummy_runs, TruncateFrom::End);
            assert_eq!(result, expected);
            for (run, result_len) in dummy_runs.iter().zip(result_run_len) {
                assert_eq!(run.len, *result_len);
            }
        }
        // Case 0: Normal
        // Text: abcdefghijkl
        // Runs: Run0 { len: 12, ... }
        //
        // Truncate res: abcd… (truncate_at = 4)
        // Run res: Run0 { string: abcd…, len: 7, ... }
        perform_test(&mut wrapper, "abcdefghijkl", "abcd…", &[12], &[7], px(50.));
        // Case 1: Drop some runs
        // Text: abcdefghijkl
        // Runs: Run0 { len: 4, ... }, Run1 { len: 4, ... }, Run2 { len: 4, ... }
        //
        // Truncate res: abcdef… (truncate_at = 6)
        // Runs res: Run0 { string: abcd, len: 4, ... }, Run1 { string: ef…, len:
        // 5, ... }
        perform_test(
            &mut wrapper,
            "abcdefghijkl",
            "abcdef…",
            &[4, 4, 4],
            &[4, 5],
            px(70.),
        );
        // Case 2: Truncate at start of some run
        // Text: abcdefghijkl
        // Runs: Run0 { len: 4, ... }, Run1 { len: 4, ... }, Run2 { len: 4, ... }
        //
        // Truncate res: abcdefgh… (truncate_at = 8)
        // Runs res: Run0 { string: abcd, len: 4, ... }, Run1 { string: efgh, len:
        // 4, ... }, Run2 { string: …, len: 3, ... }
        perform_test(
            &mut wrapper,
            "abcdefghijkl",
            "abcdefgh…",
            &[4, 4, 4],
            &[4, 4, 3],
            px(90.),
        );
    }

    #[test]
    fn test_truncate_multiple_runs_start() {
        let mut wrapper = build_wrapper();

        #[track_caller]
        fn perform_test(
            wrapper: &mut LineWrapper,
            text: &'static str,
            expected: &str,
            run_lens: &[usize],
            result_run_len: &[usize],
            line_width: Pixels,
        ) {
            let dummy_runs = generate_test_runs(run_lens);
            let (result, dummy_runs) = wrapper.truncate_line(
                text.into(),
                line_width,
                "…",
                &dummy_runs,
                TruncateFrom::Start,
            );
            assert_eq!(result, expected);
            for (run, result_len) in dummy_runs.iter().zip(result_run_len) {
                assert_eq!(run.len, *result_len);
            }
        }
        // Case 0: Normal
        // Text: abcdefghijkl
        // Runs: Run0 { len: 12, ... }
        //
        // Truncate res: …ijkl (truncate_at = 9)
        // Run res: Run0 { string: …ijkl, len: 7, ... }
        perform_test(&mut wrapper, "abcdefghijkl", "…ijkl", &[12], &[7], px(50.));
        // Case 1: Drop some runs
        // Text: abcdefghijkl
        // Runs: Run0 { len: 4, ... }, Run1 { len: 4, ... }, Run2 { len: 4, ... }
        //
        // Truncate res: …ghijkl (truncate_at = 7)
        // Runs res: Run0 { string: …gh, len: 5, ... }, Run1 { string: ijkl, len:
        // 4, ... }
        perform_test(
            &mut wrapper,
            "abcdefghijkl",
            "…ghijkl",
            &[4, 4, 4],
            &[5, 4],
            px(70.),
        );
        // Case 2: Truncate at start of some run
        // Text: abcdefghijkl
        // Runs: Run0 { len: 4, ... }, Run1 { len: 4, ... }, Run2 { len: 4, ... }
        //
        // Truncate res: abcdefgh… (truncate_at = 3)
        // Runs res: Run0 { string: …, len: 3, ... }, Run1 { string: efgh, len:
        // 4, ... }, Run2 { string: ijkl, len: 4, ... }
        perform_test(
            &mut wrapper,
            "abcdefghijkl",
            "…efghijkl",
            &[4, 4, 4],
            &[3, 4, 4],
            px(90.),
        );
    }

    #[test]
    fn test_update_run_after_truncation_end() {
        fn perform_test(result: &str, run_lens: &[usize], result_run_lens: &[usize]) {
            let mut dummy_runs = generate_test_runs(run_lens);
            update_runs_after_truncation(result, "…", &mut dummy_runs, TruncateFrom::End);
            for (run, result_len) in dummy_runs.iter().zip(result_run_lens) {
                assert_eq!(run.len, *result_len);
            }
        }
        // Case 0: Normal
        // Text: abcdefghijkl
        // Runs: Run0 { len: 12, ... }
        //
        // Truncate res: abcd… (truncate_at = 4)
        // Run res: Run0 { string: abcd…, len: 7, ... }
        perform_test("abcd…", &[12], &[7]);
        // Case 1: Drop some runs
        // Text: abcdefghijkl
        // Runs: Run0 { len: 4, ... }, Run1 { len: 4, ... }, Run2 { len: 4, ... }
        //
        // Truncate res: abcdef… (truncate_at = 6)
        // Runs res: Run0 { string: abcd, len: 4, ... }, Run1 { string: ef…, len:
        // 5, ... }
        perform_test("abcdef…", &[4, 4, 4], &[4, 5]);
        // Case 2: Truncate at start of some run
        // Text: abcdefghijkl
        // Runs: Run0 { len: 4, ... }, Run1 { len: 4, ... }, Run2 { len: 4, ... }
        //
        // Truncate res: abcdefgh… (truncate_at = 8)
        // Runs res: Run0 { string: abcd, len: 4, ... }, Run1 { string: efgh, len:
        // 4, ... }, Run2 { string: …, len: 3, ... }
        perform_test("abcdefgh…", &[4, 4, 4], &[4, 4, 3]);
    }

    #[test]
    fn test_is_word_char() {
        #[track_caller]
        fn assert_word(word: &str) {
            for c in word.chars() {
                assert!(
                    LineWrapper::is_word_char(c),
                    "assertion failed for '{}' (unicode 0x{:x})",
                    c,
                    c as u32
                );
            }
        }

        #[track_caller]
        fn assert_not_word(word: &str) {
            let found = word.chars().any(|c| !LineWrapper::is_word_char(c));
            assert!(found, "assertion failed for '{}'", word);
        }

        assert_word("Hello123");
        assert_word("non-English");
        assert_word("var_name");
        assert_word("123456");
        assert_word("3.1415");
        assert_word("10^2");
        assert_word("1~2");
        assert_word("100%");
        assert_word("@mention");
        assert_word("#hashtag");
        assert_word("$variable");
        assert_word("a=1");
        assert_word("Self::is_word_char");
        assert_word("on;");
        assert_word("more⋯");
        assert_word("won’t");
        assert_word("‘twas");
        assert_word("plz!");
        assert_word("see)");
        assert_word("quoted”");
        assert_word("well…");

        // Space
        assert_not_word("foo bar");

        // URL case
        assert_word("github.com");
        assert_not_word("zed-industries/zed");
        assert_not_word("zed-industries\\zed");
        assert_not_word("a=1&b=2");
        assert_not_word("foo?b=2");

        // Latin-1 Supplement
        assert_word("ÀÁÂÃÄÅÆÇÈÉÊËÌÍÎÏ");
        // Latin Extended-A
        assert_word("ĀāĂăĄąĆćĈĉĊċČčĎď");
        // Latin Extended-B
        assert_word("ƀƁƂƃƄƅƆƇƈƉƊƋƌƍƎƏ");
        // Cyrillic
        assert_word("АБВГДЕЖЗИЙКЛМНОП");
        // Vietnamese (https://github.com/zed-industries/zed/issues/23245)
        assert_word("ThậmchíđếnkhithuachạychúngcònnhẫntâmgiếtnốtsốđôngtùchínhtrịởYênBáivàCaoBằng");
        // Bengali
        assert_word("গিয়েছিলেন");
        assert_word("ছেলে");
        assert_word("হচ্ছিল");

        // non-word characters
        assert_not_word("你好");
        assert_not_word("안녕하세요");
        assert_not_word("こんにちは");
        assert_not_word("😀😁😂");
        assert_not_word("()[]{}<>");

        // Non-breaking ("Glue") characters, see https://www.unicode.org/reports/tr14/
        // (https://github.com/zed-industries/zed/issues/59664)
        assert_word("\u{202F}"); // NNBSP " "
        assert_word("\u{00A0}"); // NBSP " "
        assert_word("\u{2011}"); // NBH "‑"
    }

    // For compatibility with the test macro
    #[cfg(target_os = "macos")]
    use crate as gpui;

    // These seem to vary wildly based on the text system.
    #[cfg(target_os = "macos")]
    #[crate::test]
    fn test_wrap_shaped_line(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());

            let normal = TextRun {
                len: 0,
                font: font("Helvetica"),
                color: Default::default(),
                underline: Default::default(),
                ..Default::default()
            };
            let bold = TextRun {
                len: 0,
                font: font("Helvetica").bold(),
                ..Default::default()
            };

            let text = "aa bbb cccc ddddd eeee".into();
            let lines = text_system
                .shape_text(
                    text,
                    px(16.),
                    &[
                        normal.with_len(4),
                        bold.with_len(5),
                        normal.with_len(6),
                        bold.with_len(1),
                        normal.with_len(7),
                    ],
                    Some(px(72.)),
                    None,
                )
                .unwrap();

            assert_eq!(
                lines[0].layout.wrap_boundaries(),
                &[
                    WrapBoundary {
                        run_ix: 0,
                        glyph_ix: 7
                    },
                    WrapBoundary {
                        run_ix: 0,
                        glyph_ix: 12
                    },
                    WrapBoundary {
                        run_ix: 0,
                        glyph_ix: 18
                    }
                ],
            );
        });
    }

    // CSS has no hanging indent: a line that starts with spaces wraps back to the left edge.
    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_wraps_continuation_lines_to_the_left_edge(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let text: SharedString = "        aaa bbb ccc ddd eee".into();
            let run = TextRun {
                len: text.len(),
                font: font("Helvetica"),
                color: Default::default(),
                underline: Default::default(),
                ..Default::default()
            };
            let lines = text_system
                .shape_text(text.clone(), px(16.), &[run], Some(px(100.)), None)
                .unwrap();
            let layout = &lines[0].layout;
            let rows = layout.wrap_boundaries().len() + 1;
            assert!(rows >= 3, "the text should wrap onto several lines, got {rows}");
            for boundary in layout.wrap_boundaries() {
                let index = layout.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index;
                // The index of a boundary is the end of the row above, so look one glyph in.
                let position = layout.position_for_index(index + 1, px(20.)).unwrap();
                assert!(
                    position.x < px(12.),
                    "a continuation line starts at the left edge, but its second glyph is at {}",
                    position.x
                );
            }
        });
    }

    #[test]
    fn test_truncate_line_middle() {
        let mut wrapper = build_wrapper();

        // No truncation when text fits within a very wide budget.
        let short_text = "hello world";
        let runs = generate_test_runs(&[short_text.len()]);
        let (result, result_runs) = wrapper.truncate_line(
            short_text.into(),
            px(10000.),
            "…",
            &runs,
            TruncateFrom::Middle,
        );
        assert_eq!(result.as_ref(), short_text);
        assert_eq!(result_runs.len(), 1);
        assert_eq!(result_runs[0].len, short_text.len());

        // Basic middle truncation: long string with px(100.) budget.
        let long_text = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz";
        let runs = generate_test_runs(&[long_text.len()]);
        let (result, _result_runs) =
            wrapper.truncate_line(long_text.into(), px(100.), "…", &runs, TruncateFrom::Middle);
        assert!(
            result.contains('…'),
            "Middle-truncated result should contain '…', got: '{}'",
            result
        );
        assert!(
            result.chars().count() < long_text.chars().count(),
            "Middle-truncated result should be shorter than original"
        );
        assert_eq!(
            result.chars().next(),
            long_text.chars().next(),
            "Result should start with the same first character as original"
        );
        assert_eq!(
            result.chars().last(),
            long_text.chars().last(),
            "Result should end with the same last character as original"
        );

        // Degenerate case: budget so narrow that middle truncation cannot find a valid split.
        // Still show the truncation affix instead of returning the original overflowing text.
        let text = "abcdef";
        let runs = generate_test_runs(&[text.len()]);
        let (result, result_runs) =
            wrapper.truncate_line(text.into(), px(1.), "…", &runs, TruncateFrom::Middle);
        assert_eq!(result.as_ref(), "…");
        assert_eq!(result_runs.len(), 1);
        assert_eq!(result_runs[0].len, "…".len());

        // Run adjustment correctness: multiple runs across the string.
        // Verify that the returned runs' lengths sum to result.len().
        let multi_run_text = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz";
        let run_lens = [20, 20, multi_run_text.len() - 40];
        let runs = generate_test_runs(&run_lens);
        let (result, result_runs) = wrapper.truncate_line(
            multi_run_text.into(),
            px(100.),
            "…",
            &runs,
            TruncateFrom::Middle,
        );
        let total_run_len: usize = result_runs.iter().map(|r| r.len).sum();
        assert_eq!(
            total_run_len,
            result.len(),
            "Sum of run lengths ({}) should equal result byte length ({})",
            total_run_len,
            result.len()
        );
    }

    /// Menlo has the same advance for every character, so tab stops land on
    /// multiples of it.
    #[cfg(target_os = "macos")]
    struct TabbedText {
        line: WrappedLine,
        space: Pixels,
    }

    #[cfg(target_os = "macos")]
    impl TabbedText {
        const LINE_HEIGHT: Pixels = px(20.);

        fn shape(
            cx: &mut TestAppContext,
            text: &str,
            wrap_width: Option<Pixels>,
            tab_size: Option<TabSize>,
        ) -> Self {
            Self::shape_with(
                cx,
                text,
                wrap_width,
                TextLayoutOptions {
                    tab_size,
                    ..Default::default()
                },
            )
        }

        fn shape_with(
            cx: &mut TestAppContext,
            text: &str,
            wrap_width: Option<Pixels>,
            options: TextLayoutOptions,
        ) -> Self {
            cx.update(|cx| {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                let menlo = font("Menlo");
                let space = cx
                    .text_system()
                    .advance(cx.text_system().resolve_font(&menlo), px(16.), ' ')
                    .unwrap()
                    .width;
                let text: SharedString = text.to_string().into();
                let run = TextRun {
                    len: text.len(),
                    font: menlo,
                    color: Default::default(),
                    underline: Default::default(),
                    ..Default::default()
                };
                let mut lines = text_system
                    .shape_text_with_options(text, px(16.), &[run], wrap_width, None, options)
                    .unwrap();
                Self {
                    line: lines.remove(0),
                    space,
                }
            })
        }

        /// Where the character at `index` starts, in spaces from the start of its
        /// wrapped line, and the wrapped line.
        fn column(&self, index: usize) -> (f32, usize) {
            let position = self
                .line
                .layout
                .position_for_index(index, Self::LINE_HEIGHT)
                .unwrap();
            (
                f32::from(position.x / self.space),
                (position.y / Self::LINE_HEIGHT) as usize,
            )
        }
    }

    #[cfg(target_os = "macos")]
    fn assert_columns(text: &TabbedText, expected: &[(usize, f32, usize)]) {
        for (index, column, line) in expected {
            let (actual_column, actual_line) = text.column(*index);
            assert!(
                (actual_column - column).abs() < 0.01 && actual_line == *line,
                "index {index} is at column {actual_column} of line {actual_line}, expected column {column} of line {line}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_tab_aligns_the_next_glyph_to_a_multiple_of_the_tab_size(cx: &mut TestAppContext) {
        let text = TabbedText::shape(cx, "ab\tc\tdefg\th", None, Some(TabSize::Spaces(4.)));
        // a b [tab to 4] c [tab to 8] d e f g [tab to 16] h
        assert_columns(
            &text,
            &[
                (0, 0., 0),
                (1, 1., 0),
                (2, 2., 0),
                (3, 4., 0),
                (5, 8., 0),
                (9, 12., 0),
                (10, 16., 0),
            ],
        );
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_tab_on_a_stop_goes_to_the_next_stop(cx: &mut TestAppContext) {
        let text = TabbedText::shape(cx, "abcd\te", None, Some(TabSize::Spaces(4.)));
        assert_columns(&text, &[(5, 8., 0)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_two_tabs_are_two_stops(cx: &mut TestAppContext) {
        let text = TabbedText::shape(cx, "a\t\tb", None, Some(TabSize::Spaces(4.)));
        assert_columns(&text, &[(1, 1., 0), (2, 4., 0), (3, 8., 0)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_tab_length_is_a_distance(cx: &mut TestAppContext) {
        let text = TabbedText::shape(cx, "a\tb", None, Some(TabSize::Length(px(60.))));
        let position = text
            .line
            .layout
            .position_for_index(2, TabbedText::LINE_HEIGHT);
        assert_eq!(position.unwrap().x, px(60.));
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_tab_after_a_wrap_counts_from_the_new_line(cx: &mut TestAppContext) {
        // Text wraps in 6.5 spaces: "aaaa " then "bb\tc". Counted from the start of
        // the unwrapped line the tab would end at column 8, which is 3 on the second
        // line.
        let width = TabbedText::shape(cx, "a", None, None).space * 6.5;
        let text = TabbedText::shape(cx, "aaaa bb\tc", Some(width), Some(TabSize::Spaces(4.)));
        assert_eq!(
            text.line.layout.wrap_boundaries(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 5
            }]
        );
        assert_columns(&text, &[(6, 1., 1), (7, 2., 1), (8, 4., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_tab_stops_decide_where_the_line_wraps(cx: &mut TestAppContext) {
        // Eight characters fit in 9 spaces, but the tab makes the second word end at 10.
        let width = TabbedText::shape(cx, "a", None, None).space * 9.;
        let tabbed = TabbedText::shape(cx, "aa bb\tcc", Some(width), Some(TabSize::Spaces(8.)));
        assert_eq!(tabbed.line.layout.wrap_boundaries().len(), 1);
        let untabbed = TabbedText::shape(cx, "aa bb\tcc", Some(width), None);
        assert_eq!(untabbed.line.layout.wrap_boundaries().len(), 0);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_line_ending_in_a_tab_ends_at_the_stop(cx: &mut TestAppContext) {
        let text = TabbedText::shape(cx, "ab\t", None, Some(TabSize::Spaces(4.)));
        let width = f32::from(text.line.layout.width() / text.space);
        assert!((width - 4.).abs() < 0.01, "width is {width} spaces");
        assert!(
            (f32::from(text.line.layout.unwrapped_layout.width / text.space) - 4.).abs() < 0.01
        );
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_index_and_position_round_trip_across_a_tab(cx: &mut TestAppContext) {
        let text = TabbedText::shape(cx, "ab\tcd", None, Some(TabSize::Spaces(4.)));
        let layout = &text.line.layout;
        for index in 0..5 {
            let position = layout
                .position_for_index(index, TabbedText::LINE_HEIGHT)
                .unwrap();
            let inside = position + point(px(0.5), px(0.));
            assert_eq!(
                layout.index_for_position(inside, TabbedText::LINE_HEIGHT),
                Ok(index)
            );
        }
        // The tab covers the columns from 2 to 4.
        for column in [2.1, 3., 3.9] {
            let position = point(text.space * column, px(0.));
            assert_eq!(
                layout.index_for_position(position, TabbedText::LINE_HEIGHT),
                Ok(2)
            );
        }
        let position = point(text.space * 3.6, px(0.));
        assert_eq!(
            layout.closest_index_for_position(position, TabbedText::LINE_HEIGHT),
            Ok(3)
        );
        let position = point(text.space * 2.4, px(0.));
        assert_eq!(
            layout.closest_index_for_position(position, TabbedText::LINE_HEIGHT),
            Ok(2)
        );
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_without_a_tab_size_keeps_the_width_of_the_tab_it_was_shaped_with(
        cx: &mut TestAppContext,
    ) {
        let plain = TabbedText::shape(cx, "ab\tc", None, None);
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let run = TextRun {
                len: 4,
                font: font("Menlo"),
                ..Default::default()
            };
            let shaped = text_system.layout_line("ab\tc", px(16.), &[run], None);
            assert_eq!(plain.line.layout.unwrapped_layout.width, shaped.width);
            let positions: Vec<Pixels> = shaped
                .runs
                .iter()
                .flat_map(|run| run.glyphs.iter())
                .map(|glyph| glyph.position.x)
                .collect();
            let plain_positions: Vec<Pixels> = plain
                .line
                .layout
                .runs()
                .iter()
                .flat_map(|run| run.glyphs.iter())
                .map(|glyph| glyph.position.x)
                .collect();
            assert_eq!(plain_positions, positions);
        });
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_without_a_tab_shares_its_layout_whatever_the_tab_size(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let shape = |tab_size| {
                let run = TextRun {
                    len: 6,
                    font: font("Menlo"),
                    ..Default::default()
                };
                text_system
                    .shape_text_with_options(
                        "abc de".into(),
                        px(16.),
                        &[run],
                        Some(px(40.)),
                        None,
                        TextLayoutOptions {
                            tab_size,
                            ..Default::default()
                        },
                    )
                    .unwrap()
                    .remove(0)
            };
            let with = shape(Some(TabSize::Spaces(4.)));
            let without = shape(None);
            assert!(Arc::ptr_eq(&with.layout, &without.layout));
        });
    }

    #[cfg(target_os = "macos")]
    const HANGING: TextLayoutOptions = TextLayoutOptions {
        tab_size: None,
        hang_trailing_spaces: true,
        break_spaces: false,
        word_break: WordBreak::Normal,
        overflow_wrap: OverflowWrap::BreakWord,
    };

    #[cfg(target_os = "macos")]
    const BREAKING: TextLayoutOptions = TextLayoutOptions {
        tab_size: None,
        hang_trailing_spaces: false,
        break_spaces: true,
        word_break: WordBreak::Normal,
        overflow_wrap: OverflowWrap::BreakWord,
    };

    #[cfg(target_os = "macos")]
    fn spaces_wide(cx: &mut TestAppContext, spaces: f32) -> Pixels {
        TabbedText::shape(cx, "a", None, None).space * spaces
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_space_at_a_soft_wrap_hangs(cx: &mut TestAppContext) {
        // "aaaa " is 5 spaces wide, in 4.5: the space overflows without wrapping.
        let width = spaces_wide(cx, 4.5);
        let hung = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), HANGING);
        assert_eq!(
            hung.line.layout.wrap_boundaries(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 5
            }]
        );
        let hanging = hung.line.layout.hanging_widths[0];
        assert!((f32::from(hanging / hung.space) - 1.).abs() < 0.01);
        assert_eq!(hung.line.layout.hanging_widths[1], px(0.));
        assert_columns(&hung, &[(4, 4., 0), (6, 1., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_space_at_a_soft_wrap_wraps_unless_it_hangs(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 4.5);
        let plain = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), Default::default());
        // The space wraps by itself and begins a line.
        assert_eq!(plain.line.layout.wrap_boundaries().len(), 2);
        assert!(plain.line.layout.hanging_widths.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_long_run_of_spaces_at_the_wrap_edge_makes_no_line_of_its_own(
        cx: &mut TestAppContext,
    ) {
        let width = spaces_wide(cx, 5.);
        let text = TabbedText::shape_with(cx, "aaaa           bbbb", Some(width), HANGING);
        assert_eq!(text.line.layout.wrap_boundaries().len(), 1);
        assert_columns(&text, &[(16, 1., 1)]);
        let hanging = f32::from(text.line.layout.hanging_widths[0] / text.space);
        assert!((hanging - 11.).abs() < 0.01, "{hanging} spaces hang");
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_last_line_keeps_its_width_with_its_trailing_spaces(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 20.);
        let text = TabbedText::shape_with(cx, "aaa  ", Some(width), HANGING);
        assert!(text.line.layout.wrap_boundaries().is_empty());
        let measured = f32::from(text.line.layout.width() / text.space);
        assert!((measured - 5.).abs() < 0.01, "{measured} spaces wide");
        let hanging = f32::from(text.line.layout.hanging_widths[0] / text.space);
        assert!((hanging - 2.).abs() < 0.01, "{hanging} spaces hang");
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_hanging_space_can_still_be_hit(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 4.5);
        let text = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), HANGING);
        let layout = &text.line.layout;
        // The space covers columns 4 to 5, past the edge of the 4.5 wide line.
        for column in [4.2, 4.8] {
            let position = point(text.space * column, px(0.));
            assert_eq!(
                layout.index_for_position(position, TabbedText::LINE_HEIGHT),
                Ok(4)
            );
        }
        let position = point(text.space * 5.2, px(0.));
        assert_eq!(
            layout.index_for_position(position, TabbedText::LINE_HEIGHT),
            Err(5)
        );
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_tab_hangs_at_a_soft_wrap(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 5.);
        let options = TextLayoutOptions {
            tab_size: Some(TabSize::Spaces(8.)),
            hang_trailing_spaces: true,
            break_spaces: false,
            ..Default::default()
        };
        let text = TabbedText::shape_with(cx, "aaaa\tbb", Some(width), options);
        assert_eq!(text.line.layout.wrap_boundaries().len(), 1);
        let hanging = f32::from(text.line.layout.hanging_widths[0] / text.space);
        assert!((hanging - 4.).abs() < 0.01, "{hanging} spaces hang");
        assert_columns(&text, &[(6, 1., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_with_spaces_that_hang_has_a_layout_of_its_own(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let shape = |text: &str, options| {
                let run = TextRun {
                    len: text.len(),
                    font: font("Menlo"),
                    ..Default::default()
                };
                text_system
                    .shape_text_with_options(
                        text.to_string().into(),
                        px(16.),
                        &[run],
                        Some(px(40.)),
                        None,
                        options,
                    )
                    .unwrap()
                    .remove(0)
            };
            let spaced = shape("abc de", Default::default());
            assert!(!Arc::ptr_eq(
                &spaced.layout,
                &shape("abc de", HANGING).layout
            ));
            // Nothing in a word can hang, so it is the same layout either way.
            let word = shape("abcde", Default::default());
            assert!(Arc::ptr_eq(&word.layout, &shape("abcde", HANGING).layout));
        });
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_space_that_does_not_fit_starts_the_next_line_when_spaces_break(
        cx: &mut TestAppContext,
    ) {
        // "a bb " is 5 spaces wide and the next space would end at 6, in 5.5.
        let width = spaces_wide(cx, 5.5);
        let text = TabbedText::shape_with(cx, "a bb   c", Some(width), BREAKING);
        assert_eq!(
            text.line.layout.wrap_boundaries(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 5
            }]
        );
        assert!(text.line.layout.hanging_widths.is_empty());
        assert_columns(&text, &[(4, 4., 0), (6, 1., 1), (7, 2., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_run_of_spaces_breaks_between_any_two_of_them(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 3.5);
        let text = TabbedText::shape_with(cx, "a        b", Some(width), BREAKING);
        assert_eq!(text.line.layout.wrap_boundaries().len(), 3);
        assert_columns(&text, &[(4, 1., 1), (7, 1., 2)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_space_at_the_wrap_edge_starts_a_line_unlike_when_it_hangs(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 4.5);
        let breaking = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), BREAKING);
        // The space is a line of its own, and then the word.
        assert_eq!(breaking.line.layout.wrap_boundaries().len(), 2);
        assert_columns(&breaking, &[(6, 1., 2)]);
        let hanging = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), HANGING);
        assert_columns(&hanging, &[(4, 4., 0), (6, 1., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_spaces_that_break_count_in_the_width_of_a_line(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 20.);
        let text = TabbedText::shape_with(cx, "aaa  ", Some(width), BREAKING);
        assert!(text.line.layout.wrap_boundaries().is_empty());
        let measured = f32::from(text.line.layout.width() / text.space);
        assert!((measured - 5.).abs() < 0.01, "{measured} spaces wide");
        // Nothing hangs, so alignment measures the spaces too.
        assert!(text.line.layout.hanging_widths.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_that_breaks_spaces_ignores_hanging_them(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 4.5);
        let both = TextLayoutOptions {
            hang_trailing_spaces: true,
            ..BREAKING
        };
        let text = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), both);
        assert_eq!(text.line.layout.wrap_boundaries().len(), 2);
        assert_columns(&text, &[(6, 1., 2)]);
        assert!(text.line.layout.hanging_widths.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_that_breaks_spaces_has_a_layout_of_its_own(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let shape = |text: &str, options| {
                let run = TextRun {
                    len: text.len(),
                    font: font("Menlo"),
                    ..Default::default()
                };
                text_system
                    .shape_text_with_options(
                        text.to_string().into(),
                        px(16.),
                        &[run],
                        Some(px(40.)),
                        None,
                        options,
                    )
                    .unwrap()
                    .remove(0)
            };
            let spaced = shape("abc de", Default::default());
            assert!(!Arc::ptr_eq(
                &spaced.layout,
                &shape("abc de", BREAKING).layout
            ));
            // Nothing in a word breaks at a space, so it is one layout either way.
            let word = shape("abcde", Default::default());
            assert!(Arc::ptr_eq(&word.layout, &shape("abcde", BREAKING).layout));
            // Hanging that breaking overrides is no different from not hanging.
            let both = TextLayoutOptions {
                hang_trailing_spaces: true,
                ..BREAKING
            };
            assert!(Arc::ptr_eq(
                &shape("abc de", BREAKING).layout,
                &shape("abc de", both).layout
            ));
        });
    }

    #[cfg(target_os = "macos")]
    fn overflowing(overflow_wrap: OverflowWrap) -> TextLayoutOptions {
        TextLayoutOptions {
            overflow_wrap,
            ..Default::default()
        }
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_breaks_a_word_that_does_not_fit_by_default(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 3.5);
        let text = TabbedText::shape_with(cx, "aaaaaaaa bbbb", Some(width), Default::default());
        // Three letters to a line: "aaa" "aaa" "aa " "bbb" "b".
        assert_eq!(text.line.layout.wrap_boundaries().len(), 4);
        assert_columns(&text, &[(4, 1., 1), (7, 1., 2), (10, 1., 3)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_normal_overflow_wrap_keeps_a_word_whole(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 3.5);
        let options = overflowing(OverflowWrap::Normal);
        let text = TabbedText::shape_with(cx, "aaaaaaaa bbbb", Some(width), options);
        assert_eq!(
            text.line.layout.wrap_boundaries(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 9
            }]
        );
        assert_columns(&text, &[(7, 7., 0), (10, 1., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_break_word_and_anywhere_break_only_without_an_earlier_break(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 5.5);
        for overflow_wrap in [OverflowWrap::BreakWord, OverflowWrap::Anywhere] {
            let text =
                TabbedText::shape_with(cx, "aa bbbbbbbb", Some(width), overflowing(overflow_wrap));
            // "aa " then "bbbbb" and "bbb": the word goes down whole before it is cut.
            assert_eq!(text.line.layout.wrap_boundaries().len(), 2);
            assert_columns(&text, &[(4, 1., 1), (9, 1., 2)]);
        }
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_overflowing_word_when_spaces_break_leaves_no_line_of_a_space(
        cx: &mut TestAppContext,
    ) {
        let width = spaces_wide(cx, 4.5);
        let options = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Normal,
            ..BREAKING
        };
        let text = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), options);
        // "aaaa " overflows by its space, which stays at the end of its line.
        assert_eq!(text.line.layout.wrap_boundaries().len(), 1);
        assert_columns(&text, &[(4, 4., 0), (6, 1., 1)]);
        let broken = TextLayoutOptions {
            overflow_wrap: OverflowWrap::BreakWord,
            ..BREAKING
        };
        let text = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), broken);
        assert_eq!(text.line.layout.wrap_boundaries().len(), 2);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_has_a_layout_for_each_overflow_wrap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let shape = |wrap_width, options| {
                let run = TextRun {
                    len: 9,
                    font: font("Menlo"),
                    ..Default::default()
                };
                text_system
                    .shape_text_with_options(
                        "aaaa bbbb".into(),
                        px(16.),
                        &[run],
                        wrap_width,
                        None,
                        options,
                    )
                    .unwrap()
                    .remove(0)
            };
            let wrap_width = Some(px(40.));
            let default = shape(wrap_width, Default::default());
            assert!(!Arc::ptr_eq(
                &default.layout,
                &shape(wrap_width, overflowing(OverflowWrap::Normal)).layout
            ));
            // Without a wrap width there is nothing to break.
            assert!(Arc::ptr_eq(
                &shape(None, Default::default()).layout,
                &shape(None, overflowing(OverflowWrap::Normal)).layout
            ));
        });
    }

    #[cfg(target_os = "macos")]
    fn word_breaking(word_break: WordBreak, overflow_wrap: OverflowWrap) -> TextLayoutOptions {
        TextLayoutOptions {
            word_break,
            overflow_wrap,
            ..Default::default()
        }
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_break_all_fills_the_line(cx: &mut TestAppContext) {
        let width = spaces_wide(cx, 6.5);
        let normal = word_breaking(WordBreak::Normal, OverflowWrap::Normal);
        let text = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), normal);
        assert_eq!(text.line.layout.wrap_boundaries().len(), 1);
        assert_columns(&text, &[(5, 5., 0), (6, 1., 1)]);
        let all = word_breaking(WordBreak::BreakAll, OverflowWrap::Normal);
        let text = TabbedText::shape_with(cx, "aaaa bbbb", Some(width), all);
        // "aaaa b" fills the line and "bbb" is the next.
        assert_eq!(text.line.layout.wrap_boundaries().len(), 1);
        assert_columns(&text, &[(5, 5., 0), (7, 1., 1)]);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_keep_all_keeps_cjk_together(cx: &mut TestAppContext) {
        // Menlo has no CJK, so the characters come from a fallback font, each as
        // wide as a space here: two of them fit in this and a third does not.
        let width = px(25.);
        let lines = |cx: &mut TestAppContext, options| {
            TabbedText::shape_with(cx, "中中中中", Some(width), options)
                .line
                .layout
                .wrap_boundaries()
                .len()
        };
        let normal = word_breaking(WordBreak::Normal, OverflowWrap::Normal);
        assert_eq!(lines(cx, normal), 1);
        let kept = word_breaking(WordBreak::KeepAll, OverflowWrap::Normal);
        assert_eq!(lines(cx, kept), 0);
        // Unless the word is cut where it overflows.
        let cut = word_breaking(WordBreak::KeepAll, OverflowWrap::BreakWord);
        assert_eq!(lines(cx, cut), 1);
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_has_a_layout_for_each_word_break(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let shape = |wrap_width, options| {
                let run = TextRun {
                    len: 9,
                    font: font("Menlo"),
                    ..Default::default()
                };
                text_system
                    .shape_text_with_options(
                        "aaaa bbbb".into(),
                        px(16.),
                        &[run],
                        wrap_width,
                        None,
                        options,
                    )
                    .unwrap()
                    .remove(0)
            };
            let wrap_width = Some(px(40.));
            let default = shape(wrap_width, Default::default());
            for word_break in [WordBreak::BreakAll, WordBreak::KeepAll] {
                let options = word_breaking(word_break, OverflowWrap::BreakWord);
                assert!(!Arc::ptr_eq(
                    &default.layout,
                    &shape(wrap_width, options).layout
                ));
            }
            // Without a wrap width there is nothing to break.
            let all = word_breaking(WordBreak::BreakAll, OverflowWrap::BreakWord);
            assert!(Arc::ptr_eq(
                &shape(None, Default::default()).layout,
                &shape(None, all).layout
            ));
        });
    }

    #[cfg(target_os = "macos")]
    #[crate::test]
    fn shaped_text_has_a_layout_for_each_line_clamp(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text_system = WindowTextSystem::new(cx.text_system().clone());
            let wrapped_lines = |line_clamp| {
                let run = TextRun {
                    len: 17,
                    font: font("Menlo"),
                    ..Default::default()
                };
                text_system
                    .shape_text_with_options(
                        "aa bb cc dd ee ff".into(),
                        px(16.),
                        &[run],
                        Some(px(48.)),
                        line_clamp,
                        Default::default(),
                    )
                    .unwrap()
                    .remove(0)
                    .layout
                    .wrap_boundaries()
                    .len()
                    + 1
            };
            assert_eq!(wrapped_lines(Some(2)), 2);
            assert!(wrapped_lines(None) > 2);
        });
    }
}
