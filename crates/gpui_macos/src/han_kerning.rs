//! Which characters `text-spacing-trim: normal` trims, as Blink decides it (`HanKerning`,
//! `platform/fonts/shaping/han_kerning.{h,cc}`): fullwidth CJK punctuation has an empty half
//! (an opening bracket on its right, a closing bracket or a full stop on its left), and when two
//! such halves meet, one of them is cut off by the font's `halt` feature. The rules need the glyphs
//! of the font for a few characters (a comma sits on the left in Japanese and in the middle in
//! traditional Chinese), so the class of a character is the text's and the font's together.

use icu_properties::{
    CodePointMapData,
    props::{EastAsianWidth, GeneralCategory},
};
use std::ops::Range;

/// The class of a character for trimming (CSS Text 4, "Text Spacing Character Classes").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CharType {
    Other,
    /// A fullwidth opening bracket: empty on its left half.
    Open,
    /// A fullwidth closing bracket, or a full stop or comma on the left: empty on its right half.
    Close,
    /// Middle dots, empty on both sides.
    Middle,
    OpenNarrow,
    CloseNarrow,
    /// Which of the next four the font decides.
    Dot,
    Colon,
    Semicolon,
    OpenQuote,
    CloseQuote,
}

/// Whether a character is one that a text has to have for any trimming to be possible
/// (`MaybeHanKerningOpenOrCloseFast`).
pub(crate) fn may_apply(character: char) -> bool {
    matches!(character, '\u{2018}'..='\u{301F}' | '\u{FF08}'..='\u{FF60}')
}

/// The class of `character` without a font (`GetHanKerningCharType`).
pub(crate) fn static_type(character: char) -> CharType {
    match character {
        '\u{2018}' | '\u{201C}' => return CharType::OpenQuote,
        '\u{2019}' | '\u{201D}' => return CharType::CloseQuote,
        '\u{3000}' | '\u{00B7}' | '\u{2027}' | '\u{30FB}' => return CharType::Middle,
        '\u{3001}' | '\u{3002}' | '\u{FF0C}' | '\u{FF0E}' => return CharType::Dot,
        '\u{FF1A}' => return CharType::Colon,
        '\u{FF1B}' => return CharType::Semicolon,
        _ => {}
    }
    let category = CodePointMapData::<GeneralCategory>::new().get(character);
    let (open, close) = (
        category == GeneralCategory::OpenPunctuation,
        category == GeneralCategory::ClosePunctuation,
    );
    if !open && !close {
        return CharType::Other;
    }
    let fullwidth = matches!(character, '\u{3000}'..='\u{303F}')
        || CodePointMapData::<EastAsianWidth>::new().get(character) == EastAsianWidth::Fullwidth;
    match (open, fullwidth) {
        (true, true) => CharType::Open,
        (true, false) => CharType::OpenNarrow,
        (false, true) => CharType::Close,
        (false, false) => CharType::CloseNarrow,
    }
}

/// Whether `character` may be trimmed at the end of a line to make it fit: a closing bracket or
/// quote, known from the character alone (`MaybeHanKerningClose`). The full stop and the comma
/// are not: whether they are closing depends on the font.
pub(crate) fn may_trim_at_line_end(character: char) -> bool {
    may_apply(character)
        && matches!(static_type(character), CharType::Close | CharType::CloseQuote)
}

/// What a glyph looks like, for the classes of the characters that depend on the font.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphShape {
    pub advance: f64,
    /// The left and right edge of the ink, from the origin of the glyph.
    pub left: f64,
    pub right: f64,
}

/// The characters whose class the font decides: the ideographic comma and full stop, the fullwidth
/// comma and full stop, the colon and semicolon, and the curly quotes
/// (`HanKerning::FontData`).
pub(crate) const FONT_PROBE_CHARACTERS: [char; 10] = [
    '\u{3001}', '\u{3002}', '\u{FF0C}', '\u{FF0E}', '\u{FF1A}', '\u{FF1B}', '\u{201C}',
    '\u{2018}', '\u{201D}', '\u{2019}',
];

/// What a font with `halt` says of the characters whose class depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FontClasses {
    dot: CharType,
    colon: CharType,
    semicolon: CharType,
    quote_fullwidth: bool,
}

fn type_from_bounds(half_em: f64, glyph: GlyphShape) -> CharType {
    if glyph.right <= half_em {
        CharType::Close
    } else if glyph.left >= half_em {
        CharType::Open
    } else if glyph.right - glyph.left <= half_em && glyph.left >= half_em / 2. {
        CharType::Middle
    } else {
        CharType::Other
    }
}

/// The class a group of glyphs share: `Other` unless they have one advance and one class.
fn group_type(glyphs: &[GlyphShape]) -> CharType {
    let first = glyphs[0];
    let first_type = type_from_bounds(first.advance / 2., first);
    for glyph in &glyphs[1..] {
        if glyph.advance != first.advance || type_from_bounds(first.advance / 2., *glyph) != first_type {
            return CharType::Other;
        }
    }
    first_type
}

impl FontClasses {
    /// The classes of a font whose glyphs for [`FONT_PROBE_CHARACTERS`] are `glyphs`.
    pub(crate) fn from_glyphs(glyphs: &[GlyphShape; 10]) -> Self {
        let single = |glyph: GlyphShape| type_from_bounds(glyph.advance / 2., glyph);
        Self {
            dot: group_type(&glyphs[0..4]),
            colon: single(glyphs[4]),
            semicolon: single(glyphs[5]),
            // Japanese fonts have the proportional quotes of Latin, Chinese ones fullwidth quotes.
            quote_fullwidth: group_type(&glyphs[6..8]) == CharType::Open
                && group_type(&glyphs[8..10]) == CharType::Close,
        }
    }

    /// The class of `character` in this font.
    pub(crate) fn type_of(&self, character: char) -> CharType {
        match static_type(character) {
            CharType::Dot => self.dot,
            CharType::Colon => self.colon,
            CharType::Semicolon => self.semicolon,
            CharType::OpenQuote if self.quote_fullwidth => CharType::Open,
            CharType::OpenQuote => CharType::OpenNarrow,
            CharType::CloseQuote if self.quote_fullwidth => CharType::Close,
            CharType::CloseQuote => CharType::CloseNarrow,
            other => other,
        }
    }
}

/// Whether an opening bracket after `last` has its empty half cut (`ShouldKern`).
pub(crate) fn should_kern(ty: CharType, last: CharType) -> bool {
    ty == CharType::Open
        && matches!(
            last,
            CharType::Open | CharType::Middle | CharType::Close | CharType::OpenNarrow
        )
}

/// Whether the closing bracket `last` has its empty half cut when `ty` follows it
/// (`ShouldKernLast`).
pub(crate) fn should_kern_last(ty: CharType, last: CharType) -> bool {
    last == CharType::Close
        && matches!(ty, CharType::Close | CharType::Middle | CharType::CloseNarrow)
}

/// Which side of a glyph the cut half is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    /// An opening bracket after punctuation: the half on the left.
    Open,
    /// A closing bracket before punctuation: the half on the right.
    Close,
}

/// The characters of `chars[range]`, a stretch of text in one font whose classes are `font`, that
/// are trimmed, in order. The characters around the stretch decide for those at its edges, with
/// the classes of this font (`HanKerning::AppendFontFeatures`).
pub(crate) fn halted(chars: &[char], range: Range<usize>, font: &FontClasses) -> Vec<(usize, Side)> {
    let mut halted = Vec::new();
    if range.is_empty() {
        return halted;
    }
    let mut last = font.type_of(chars[range.start]);
    if range.start > 0 {
        let before = font.type_of(chars[range.start - 1]);
        if should_kern(last, before) {
            halted.push((range.start, Side::Open));
        }
    }
    for ix in range.start + 1..range.end {
        let ty = font.type_of(chars[ix]);
        if should_kern_last(ty, last) {
            halted.push((ix - 1, Side::Close));
        } else if should_kern(ty, last) {
            halted.push((ix, Side::Open));
        }
        last = ty;
    }
    if range.end < chars.len() && should_kern_last(font.type_of(chars[range.end]), last) {
        halted.push((range.end - 1, Side::Close));
    }
    halted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hiragino Sans at 100: the comma and the full stop sit on the left, the colon in the middle,
    /// the quotes are proportional.
    fn japanese() -> FontClasses {
        let glyph = |left: f64, right: f64| GlyphShape {
            advance: 100.,
            left,
            right,
        };
        let dot = glyph(10., 40.);
        let proportional = GlyphShape {
            advance: 55.,
            left: 10.,
            right: 40.,
        };
        FontClasses::from_glyphs(&[
            dot,
            dot,
            dot,
            dot,
            glyph(30., 70.),
            glyph(30., 70.),
            proportional,
            proportional,
            proportional,
            proportional,
        ])
    }

    fn halts(text: &str) -> Vec<(usize, Side)> {
        let chars: Vec<char> = text.chars().collect();
        halted(&chars, 0..chars.len(), &japanese())
    }

    #[test]
    fn test_the_class_of_a_character_comes_from_its_category_width_and_block() {
        assert_eq!(static_type('「'), CharType::Open);
        assert_eq!(static_type('」'), CharType::Close);
        assert_eq!(static_type('（'), CharType::Open);
        assert_eq!(static_type('）'), CharType::Close);
        assert_eq!(static_type('〈'), CharType::Open);
        assert_eq!(static_type('・'), CharType::Middle);
        assert_eq!(static_type('　'), CharType::Middle);
        assert_eq!(static_type('。'), CharType::Dot);
        assert_eq!(static_type('：'), CharType::Colon);
        assert_eq!(static_type('“'), CharType::OpenQuote);
        assert_eq!(static_type('”'), CharType::CloseQuote);
        // Not fullwidth: halfwidth brackets and the ASCII ones are narrow.
        assert_eq!(static_type('｢'), CharType::OpenNarrow);
        assert_eq!(static_type('('), CharType::OpenNarrow);
        assert_eq!(static_type(')'), CharType::CloseNarrow);
        assert_eq!(static_type('あ'), CharType::Other);
        assert_eq!(static_type('！'), CharType::Other);
    }

    #[test]
    fn test_a_font_decides_where_the_dots_and_quotes_sit() {
        let japanese = japanese();
        assert_eq!(japanese.type_of('。'), CharType::Close);
        assert_eq!(japanese.type_of('：'), CharType::Middle);
        assert_eq!(japanese.type_of('“'), CharType::OpenNarrow);
        assert_eq!(japanese.type_of('”'), CharType::CloseNarrow);
        let fullwidth_quote = |left: f64, right: f64| GlyphShape {
            advance: 100.,
            left,
            right,
        };
        let chinese = FontClasses::from_glyphs(&[
            fullwidth_quote(30., 70.),
            fullwidth_quote(30., 70.),
            fullwidth_quote(30., 70.),
            fullwidth_quote(30., 70.),
            fullwidth_quote(60., 90.),
            fullwidth_quote(60., 90.),
            fullwidth_quote(60., 90.),
            fullwidth_quote(60., 90.),
            fullwidth_quote(10., 40.),
            fullwidth_quote(10., 40.),
        ]);
        // A full stop in the middle of the em, as traditional Chinese sets it, is a middle dot.
        assert_eq!(chinese.type_of('。'), CharType::Middle);
        assert_eq!(chinese.type_of('：') , CharType::Open);
        assert_eq!(chinese.type_of('“'), CharType::Open);
        assert_eq!(chinese.type_of('”'), CharType::Close);
    }

    #[test]
    fn test_an_opening_bracket_after_punctuation_is_trimmed() {
        // The one that opens, after a full stop, a closing bracket, an opening bracket, a middle dot.
        assert_eq!(halts("。「"), vec![(1, Side::Open)]);
        assert_eq!(halts("」「"), vec![(1, Side::Open)]);
        assert_eq!(halts("「「"), vec![(1, Side::Open)]);
        assert_eq!(halts("・「"), vec![(1, Side::Open)]);
        assert_eq!(halts("：「"), vec![(1, Side::Open)]);
        // After a narrow opening bracket as well.
        assert_eq!(halts("(「"), vec![(1, Side::Open)]);
        assert_eq!(halts("“「"), vec![(1, Side::Open)]);
    }

    #[test]
    fn test_a_closing_bracket_before_a_closing_one_is_trimmed() {
        // The one that closes first: before a closing bracket, a middle dot, a narrow closing bracket.
        assert_eq!(halts("。」"), vec![(0, Side::Close)]);
        assert_eq!(halts("、）").len(), 1);
        assert_eq!(halts("。。"), vec![(0, Side::Close)]);
        assert_eq!(halts("」・"), vec![(0, Side::Close)]);
        assert_eq!(halts("」)"), vec![(0, Side::Close)]);
    }

    #[test]
    fn test_what_the_rules_leave_alone() {
        for text in ["「。", "「、", "。“", "「”", "！「", "？「", "あ「", "」あ", "「あ", "あ。", "a」"] {
            assert_eq!(halts(text), vec![], "{text}");
        }
    }

    #[test]
    fn test_a_run_of_punctuation_is_trimmed_pair_by_pair() {
        // A closing bracket both ends a pair and starts one.
        assert_eq!(
            halts("。」」"),
            vec![(0, Side::Close), (1, Side::Close)]
        );
        assert_eq!(
            halts("」「「"),
            vec![(1, Side::Open), (2, Side::Open)]
        );
        assert_eq!(halts("す。」「え"), vec![(1, Side::Close), (3, Side::Open)]);
    }

    #[test]
    fn test_the_characters_around_a_stretch_decide_for_its_edges() {
        let chars: Vec<char> = "あ。「".chars().collect();
        let font = japanese();
        // The stretch of the first font ends at the full stop, and the next character is an opening
        // bracket: nothing to cut on this side.
        assert_eq!(halted(&chars, 0..2, &font), vec![]);
        // The stretch of another font starts at the opening bracket after a full stop.
        assert_eq!(halted(&chars, 2..3, &font), vec![(2, Side::Open)]);
        // A stretch that ends before a closing bracket trims its last.
        let chars: Vec<char> = "あ。」".chars().collect();
        assert_eq!(halted(&chars, 0..2, &font), vec![(1, Side::Close)]);
    }

    #[test]
    fn test_only_closing_brackets_are_trimmed_at_the_end_of_a_line() {
        assert!(may_trim_at_line_end('」'));
        assert!(may_trim_at_line_end('）'));
        assert!(may_trim_at_line_end('”'));
        // The class of a full stop depends on the font.
        assert!(!may_trim_at_line_end('。'));
        assert!(!may_trim_at_line_end('「'));
        assert!(!may_trim_at_line_end('あ'));
        assert!(!may_trim_at_line_end('a'));
    }

    #[test]
    fn test_a_text_with_none_of_the_punctuation_cannot_be_trimmed() {
        assert!(!"こんにちは Hello".chars().any(may_apply));
        assert!(!"・".chars().any(may_apply));
        assert!("こんにちは。".chars().any(may_apply));
        assert!("（".chars().any(may_apply));
    }
}
