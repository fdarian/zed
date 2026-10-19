//! Where a line of text may break, for `word-break: normal` and `keep-all`, the way
//! Blink decides it (`text_break_iterator.cc`, `LazyLineBreakIterator::NextBreakablePosition`).
//!
//! A break is allowed before a character, never before a space, and always before the
//! first character after a run of spaces. Between two other characters Blink asks
//! three sources, cheapest first:
//!
//! 1. A table for two characters in `!`..=`ÿ`. Between ASCII characters it is
//!    hand-written (`LineBreakData::FillAscii`): a break after `-` or `?` and before `(`,
//!    `<`, `[` and `{`, with a list of exceptions, and nothing else. The rest of the
//!    square is what ICU says of the two characters on their own, measured from Chrome.
//!    `-` followed by a digit breaks only after a letter or a digit, so `ABCD-1234`
//!    breaks and a minus sign in `x -5` does not, and `-` followed by a character beyond
//!    ASCII is left to ICU.
//! 2. `word-break: keep-all` keeps two letters or numbers together.
//! 3. The Unicode line breaking algorithm (UAX #14) over the whole text, from ICU4X, at
//!    the strictness Chrome uses for `line-break: auto`.
//!
//! The editor's wrapper (`line_wrapper.rs`) has its own simpler rules and is not
//! covered by this.

use std::sync::LazyLock;

use icu_properties::{
    CodePointMapData,
    props::{GeneralCategory, GeneralCategoryGroup, LineBreak},
};
use icu_segmenter::{
    LineSegmenter, LineSegmenterBorrowed,
    options::{LineBreakOptions, LineBreakStrictness, LineBreakWordOption},
};

/// The places a text may break, as the byte offsets of the characters a line may start at.
pub(super) struct LineBreaks {
    offsets: Vec<usize>,
}

impl LineBreaks {
    pub(super) fn new(text: &str, keep_all: bool) -> Self {
        let mut offsets = Vec::new();
        let mut icu = Icu::new(text);
        let mut before_last = None;
        let mut last = None;
        for (offset, current) in text.char_indices() {
            if let Some(last) = last
                && breaks_between(before_last, last, current, keep_all, offset, &mut icu)
            {
                offsets.push(offset);
            }
            before_last = last;
            last = Some(current);
        }
        Self { offsets }
    }

    /// Whether a line may start at the character at `offset`.
    pub(super) fn contains(&self, offset: usize) -> bool {
        self.offsets.binary_search(&offset).is_ok()
    }
}

/// What Blink calls white space that collapses, the characters a break comes after.
fn is_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n')
}

fn breaks_between(
    before_last: Option<char>,
    last: char,
    current: char,
    keep_all: bool,
    offset: usize,
    icu: &mut Icu,
) -> bool {
    if is_space(current) {
        return false;
    }
    if is_space(last) {
        return true;
    }
    match fast_break(before_last, last, current) {
        Fast::Break => true,
        Fast::NoBreak => false,
        Fast::Unknown => {
            !(keep_all && keeps_together(before_last, last, current)) && icu.is_boundary(offset)
        }
    }
}

enum Fast {
    Break,
    NoBreak,
    Unknown,
}

/// Blink's `Context::ShouldBreakFast`.
fn fast_break(before_last: Option<char>, last: char, current: char) -> Fast {
    if last < '!' || current < '!' {
        return Fast::NoBreak;
    }
    if last == '-' {
        if !current.is_ascii() {
            return Fast::Unknown;
        }
        if current.is_ascii_digit() {
            return if before_last.is_some_and(|c| c.is_ascii_alphanumeric()) {
                Fast::Break
            } else {
                Fast::NoBreak
            };
        }
    }
    if last > '\u{FF}' || current > '\u{FF}' {
        return Fast::Unknown;
    }
    if pair_breaks(last, current) {
        Fast::Break
    } else {
        Fast::NoBreak
    }
}

/// `word-break: keep-all` keeps two characters together that are letters or numbers, and
/// not of a script that is broken by a dictionary (Thai, Lao, Khmer, Burmese). A mark
/// counts as the character it is on. Blink reads the characters as UTF-16 units, so one
/// beyond the BMP is no letter to it.
fn keeps_together(before_last: Option<char>, last: char, current: char) -> bool {
    let general_category = CodePointMapData::<GeneralCategory>::new();
    let before = if GeneralCategoryGroup::Mark.contains(general_category.get(last)) {
        before_last
    } else {
        Some(last)
    };
    let is_word_part = |character: char| {
        character <= '\u{FFFF}'
            && (GeneralCategoryGroup::Letter.contains(general_category.get(character))
                || GeneralCategoryGroup::Number.contains(general_category.get(character)))
            && CodePointMapData::<LineBreak>::new().get(character) != LineBreak::ComplexContext
    };
    before.is_some_and(is_word_part) && is_word_part(current)
}

/// The boundaries of the whole text, which are only computed when a pair of characters
/// needs them: a text in Latin-1 never does.
struct Icu<'a> {
    text: &'a str,
    boundaries: Option<Vec<usize>>,
}

impl<'a> Icu<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            boundaries: None,
        }
    }

    fn is_boundary(&mut self, offset: usize) -> bool {
        self.boundaries
            .get_or_insert_with(|| SEGMENTER.segment_str(self.text).collect())
            .binary_search(&offset)
            .is_ok()
    }
}

/// `line-break: auto` is ICU's `line_normal` in Chromium, and so is `Normal` here. Thai,
/// Lao, Khmer and Burmese are split with their dictionaries, as Chromium does.
static SEGMENTER: LazyLock<LineSegmenterBorrowed<'static>> = LazyLock::new(|| {
    let mut options = LineBreakOptions::default();
    options.strictness = Some(LineBreakStrictness::Normal);
    options.word_option = Some(LineBreakWordOption::Normal);
    let mut segmenter = LineSegmenter::new_17_for_non_complex_scripts(options);
    segmenter.load_dictionary();
    segmenter
});

/// Blink's `kFastLineBreakTable` at `(last, current)`, both in `!`..=`ÿ`.
fn pair_breaks(last: char, current: char) -> bool {
    if last.is_ascii() && current.is_ascii() {
        return ASCII_BREAKS[last as usize - ASCII_FIRST][current as usize - ASCII_FIRST];
    }
    (LATIN1_BREAKS[latin1_class(last)] >> latin1_class(current)) & 1 == 1
}

const ASCII_FIRST: usize = '!' as usize;
const ASCII_SIDE: usize = 0x7F - ASCII_FIRST + 1;

static ASCII_BREAKS: [[bool; ASCII_SIDE]; ASCII_SIDE] = {
    let mut table = [[false; ASCII_SIDE]; ASCII_SIDE];
    let mut last = 0;
    while last < ASCII_SIDE {
        let mut current = 0;
        while current < ASCII_SIDE {
            table[last][current] =
                ascii_breaks((last + ASCII_FIRST) as u8, (current + ASCII_FIRST) as u8);
            current += 1;
        }
        last += 1;
    }
    table
};

/// Blink's `LineBreakData::FillAscii`. Every rule it applies after the first two,
/// which allow a break, takes one away.
const fn ascii_breaks(last: u8, current: u8) -> bool {
    let opens = matches!(current, b'(' | b'<' | b'[' | b'{');
    let after_dash = matches!(last, b'-' | b'?');
    let ends_word = matches!(
        current,
        b'!' | b')' | b',' | b'.' | b'/' | b':' | b';' | b'?' | b']' | b'}'
    );
    let exception = ends_word
        || (last == b'-' && (current == b'$' || current.is_ascii_digit()))
        || (last == b'?' && matches!(current, b'"' | b'\''))
        // The characters that go with what follows them.
        || matches!(last, b'$' | b'\'' | b'(' | b'/' | b'<' | b'@' | b'[' | b'^' | b'_' | b'`' | b'{' | 0x7F)
        || last.is_ascii_alphanumeric();
    (opens || after_dash) && !exception
}

/// The classes of characters in `!`..=`ÿ` that ICU treats alike next to the characters
/// beyond ASCII, which is all [`LATIN1_BREAKS`] tells apart.
const fn latin1_class(character: char) -> usize {
    match character {
        '!' | '/' | '?' | '}' => 0,
        '"' | '\'' => 1,
        '$' | '%' | '+' | '\\' => 3,
        '(' | '[' | '{' => 4,
        ')' | ',' | '.' | ':' | ';' | ']' => 5,
        '-' => 6,
        '|' => 7,
        '\u{7F}' => 8,
        '\u{85}' => 10,
        '\u{80}'..='\u{9F}' => 9,
        '\u{A0}' => 11,
        '\u{A1}' | '\u{BF}' => 12,
        '\u{A2}'..='\u{A5}' | '\u{B0}' | '\u{B1}' => 13,
        '\u{AB}' | '\u{BB}' => 15,
        '\u{AD}' => 16,
        '\u{B4}' => 17,
        '\u{A6}'.. => 14,
        _ => 2,
    }
}

/// For each class of [`latin1_class`], the classes it breaks before when one of the two
/// is not ASCII. Chrome's table for these pairs is ICU's answer for the pair on its own
/// and does not follow the ASCII rules, which is why `¢(` breaks and `a(` does not.
/// Between two ASCII classes the table is [`ASCII_BREAKS`] and these bits are unused.
const LATIN1_BREAKS: [u32; 18] = [
    0b100111000000000000,
    0b000000000000000000,
    0b100000000000000000,
    0b100011000000000000,
    0b000000000000000000,
    0b100011000000000000,
    0b100011100000000000,
    0b100111100000000000,
    0b100000000000000000,
    0b100000000000000000,
    0b111111111111111111,
    0b000000000000000000,
    0b000000000000000000,
    0b100011000000011000,
    0b100000000000000000,
    0b000000000000000000,
    0b100111100000011100,
    0b000000000000000000,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows a text is cut into at every place a line may break, which is what
    /// Chrome lays a text out in at a width of zero. The expected rows are Chrome's.
    fn rows(text: &str, keep_all: bool) -> Vec<&str> {
        let breaks = LineBreaks::new(text, keep_all);
        let mut rows = Vec::new();
        let mut start = 0;
        for (offset, _) in text.char_indices().skip(1) {
            if breaks.contains(offset) {
                rows.push(&text[start..offset]);
                start = offset;
            }
        }
        rows.push(&text[start..]);
        rows
    }

    #[test]
    fn the_ascii_table_is_blinks() {
        // Rows of the matrix `FillAscii` makes, from `!` to U+007F; `1` breaks.
        let rows = [
            (
                '-',
                ".11.1111.11.1..............111.11111111111111111111111111111.1111111111111111111111111111111.11",
            ),
            (
                '?',
                "..1111.1.11.1..1111111111..111.11111111111111111111111111111.1111111111111111111111111111111.11",
            ),
            (
                '!',
                ".......1...................1..............................1...............................1....",
            ),
            (
                '|',
                ".......1...................1..............................1...............................1....",
            ),
            (
                'a',
                "...............................................................................................",
            ),
            (
                '(',
                "...............................................................................................",
            ),
        ];
        for (last, expected) in rows {
            let actual: String = ('!'..='\u{7F}')
                .map(|current| if pair_breaks(last, current) { '1' } else { '.' })
                .collect();
            assert_eq!(actual, expected, "after {last:?}");
        }
    }

    #[test]
    fn a_pair_beyond_ascii_is_what_icu_says_of_it() {
        // Chrome: a currency sign breaks before an opening bracket and a letter does not,
        // a break comes before U+00B4 after anything but a quote, an opening bracket and
        // a no-break space, and NEL breaks before everything.
        for (last, current, breaks) in [
            ('\u{A2}', '(', true),
            ('a', '(', false),
            ('\u{A3}', '[', true),
            ('a', '\u{B4}', true),
            ('"', '\u{B4}', false),
            ('(', '\u{B4}', false),
            ('\u{A0}', '\u{B4}', false),
            ('\u{E9}', 'a', false),
            ('a', '\u{E9}', false),
            ('\u{85}', 'a', true),
            ('\u{AD}', 'a', true),
            ('\u{AD}', '\u{E9}', true),
            ('!', '\u{A1}', true),
            ('a', '\u{A1}', false),
            ('|', '\u{C0}', true),
            ('a', '\u{C0}', false),
        ] {
            assert_eq!(pair_breaks(last, current), breaks, "{last:?} {current:?}");
        }
    }

    #[test]
    fn a_line_breaks_before_an_opening_bracket_only_after_punctuation() {
        assert_eq!(rows("art(x)", false), ["art(x)"]);
        assert_eq!(rows("foo(bar)", false), ["foo(bar)"]);
        assert_eq!(rows("a<b>c", false), ["a<b>c"]);
        assert_eq!(
            rows("(a) and [b] and {c}", false),
            ["(a) ", "and ", "[b] ", "and ", "{c}"]
        );
        assert_eq!(rows("pre-(fix)", false), ["pre-", "(fix)"]);
    }

    #[test]
    fn a_line_breaks_after_a_hyphen_unless_a_closing_character_follows() {
        assert_eq!(
            rows("well-known state-of-the-art", false),
            ["well-", "known ", "state-", "of-", "the-", "art"]
        );
        assert_eq!(rows("art-\"x\"", false), ["art-", "\"x\""]);
        assert_eq!(rows("art-%x", false), ["art-", "%x"]);
        assert_eq!(rows("ab-.cd", false), ["ab-.cd"]);
        assert_eq!(rows("ab-)", false), ["ab-)"]);
        assert_eq!(rows("$-1", false), ["$-1"]);
        assert_eq!(rows("ab--", false), ["ab-", "-"]);
    }

    #[test]
    fn a_hyphen_before_a_digit_breaks_after_a_letter_or_a_digit() {
        assert_eq!(rows("x-1", false), ["x-", "1"]);
        assert_eq!(rows("ABCD-1234", false), ["ABCD-", "1234"]);
        assert_eq!(rows("2026-10-19", false), ["2026-", "10-", "19"]);
        // A minus sign: after a space or at the start of the text, or after a dollar sign.
        assert_eq!(rows("-5", false), ["-5"]);
        assert_eq!(rows("a -5 b", false), ["a ", "-5 ", "b"]);
    }

    #[test]
    fn a_hyphen_before_a_character_beyond_ascii_is_left_to_unicode() {
        assert_eq!(rows("pre-\u{E9}", false), ["pre-", "\u{E9}"]);
        assert_eq!(
            rows("pre-\u{432}\u{432}", false),
            ["pre-", "\u{432}\u{432}"]
        );
        // Alone, with no letter in front of it, the hyphen holds on to what follows.
        assert_eq!(rows("-\u{E9}", false), ["-\u{E9}"]);
    }

    #[test]
    fn the_non_breaking_hyphen_is_glue() {
        assert_eq!(rows("a\u{2011}b", false), ["a\u{2011}b"]);
        assert_eq!(rows("re-\u{2011}entry", false), ["re-", "\u{2011}entry"]);
        assert_eq!(rows("a\u{2010}b", false), ["a\u{2010}", "b"]);
        assert_eq!(rows("a\u{2013}b", false), ["a\u{2013}", "b"]);
    }

    #[test]
    fn a_line_breaks_after_a_slash_only_where_a_question_mark_follows() {
        assert_eq!(rows("a/b?c", false), ["a/b?", "c"]);
        assert_eq!(rows("~/dir/file", false), ["~/dir/file"]);
        assert_eq!(
            rows(
                "https://example.com/path/file.html?query=1&other=2#frag",
                false
            ),
            [
                "https://example.com/path/file.html?",
                "query=1&other=2#frag"
            ]
        );
        assert_eq!(rows("what?no", false), ["what?", "no"]);
        assert_eq!(rows("why?!", false), ["why?!"]);
    }

    #[test]
    fn a_percent_sign_and_a_currency_sign_keep_to_their_number() {
        assert_eq!(rows("50% off", false), ["50% ", "off"]);
        assert_eq!(
            rows("price: $100.00 or \u{20AC}50", false),
            ["price: ", "$100.00 ", "or ", "\u{20AC}50"]
        );
    }

    #[test]
    fn quotes_stay_with_the_words_they_hold() {
        assert_eq!(
            rows("the \u{201C}quoted\u{201D} text", false),
            ["the ", "\u{201C}quoted\u{201D} ", "text"]
        );
        assert_eq!(
            rows("He said: \u{201C}Go!\u{201D}", false),
            ["He ", "said: ", "\u{201C}Go!\u{201D}"]
        );
    }

    #[test]
    fn a_tab_is_a_space() {
        assert_eq!(rows("a\tb", false), ["a\t", "b"]);
        assert_eq!(rows("tab\tand space", false), ["tab\t", "and ", "space"]);
    }

    #[test]
    fn a_break_comes_after_every_run_of_spaces_and_never_before_one() {
        assert_eq!(rows("a b", false), ["a ", "b"]);
        assert_eq!(rows("a (b", false), ["a ", "(b"]);
        assert_eq!(rows("a )b", false), ["a ", ")b"]);
        assert_eq!(rows("a - b", false), ["a ", "- ", "b"]);
    }

    #[test]
    fn cjk_breaks_between_characters_and_keeps_its_punctuation() {
        assert_eq!(
            rows("Mixed Latin\u{4E2D}\u{6587}Text\u{6D4B}\u{8BD5}more", false),
            [
                "Mixed ", "Latin", "\u{4E2D}", "\u{6587}", "Text", "\u{6D4B}", "\u{8BD5}", "more"
            ]
        );
        assert_eq!(
            rows(
                "Hello \u{4E16}\u{754C}\u{FF01}\u{4F60}\u{597D}\u{3002}",
                false
            ),
            [
                "Hello ",
                "\u{4E16}",
                "\u{754C}\u{FF01}",
                "\u{4F60}",
                "\u{597D}\u{3002}"
            ]
        );
        assert_eq!(
            rows("\u{3053}\u{3093}\u{306B}\u{3061}\u{306F}", false),
            ["\u{3053}", "\u{3093}", "\u{306B}", "\u{3061}", "\u{306F}"]
        );
        // Small kana and the prolonged sound mark may start a line, as `line-break: auto` has it.
        assert_eq!(rows("\u{3042}\u{3041}", false), ["\u{3042}", "\u{3041}"]);
    }

    #[test]
    fn emoji_break_between_one_another_but_not_inside_a_sequence() {
        assert_eq!(
            rows(
                "\u{1F600}\u{1F601}\u{1F602} emoji \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} family",
                false
            ),
            [
                "\u{1F600}",
                "\u{1F601}",
                "\u{1F602} ",
                "emoji ",
                "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} ",
                "family"
            ]
        );
        assert_eq!(
            rows("\u{1F1FA}\u{1F1F8}\u{1F1EF}\u{1F1F5} flags", false),
            ["\u{1F1FA}\u{1F1F8}", "\u{1F1EF}\u{1F1F5} ", "flags"]
        );
    }

    #[test]
    fn thai_breaks_between_the_words_of_its_dictionary() {
        assert_eq!(
            rows("ภาษาไทยเป็นภาษาที่มีวรรณยุกต์", false),
            ["ภาษา", "ไทย", "เป็น", "ภาษา", "ที่", "มี", "วรรณยุกต์"]
        );
    }

    #[test]
    fn keep_all_keeps_letters_and_numbers_together() {
        assert_eq!(
            rows("Mixed Latin\u{4E2D}\u{6587}Text\u{6D4B}\u{8BD5}more", true),
            ["Mixed ", "Latin\u{4E2D}\u{6587}Text\u{6D4B}\u{8BD5}more"]
        );
        assert_eq!(
            rows("\u{D55C}\u{AD6D}\u{C5B4} \u{D14D}\u{C2A4}\u{D2B8}", true),
            ["\u{D55C}\u{AD6D}\u{C5B4} ", "\u{D14D}\u{C2A4}\u{D2B8}"]
        );
        // Punctuation, and the places the fast table breaks, stay as they were.
        assert_eq!(
            rows(
                "Hello \u{4E16}\u{754C}\u{FF01}\u{4F60}\u{597D}\u{3002}",
                true
            ),
            [
                "Hello ",
                "\u{4E16}\u{754C}\u{FF01}",
                "\u{4F60}\u{597D}\u{3002}"
            ]
        );
        assert_eq!(
            rows("well-known \u{4E16}-\u{754C}", true),
            ["well-", "known ", "\u{4E16}-", "\u{754C}"]
        );
    }
}
