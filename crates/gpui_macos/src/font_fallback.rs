//! Which font each character of a text that its font has no glyph for is set in, as Chrome picks
//! it (`HarfBuzzShaper::ShapeSegment` and `FontFallbackIterator`) and not as CoreText does when it
//! cascades. CoreText chooses a font for each stretch of the characters it has to replace, so the
//! full stop in Japanese text is PingFang's when the text starts with Han and Hiragino's when it
//! starts with kana. Chrome cuts the text in script runs ([`crate::script_runs`]), asks the system
//! for one font for the characters of a run that the font lacks, using the first of them that has
//! a script of its own as the character to find a font for, and gives that font all of the
//! characters it has glyphs for. What it does not have is asked again, with the next such character.
//! A full stop after kana belongs to the kana run, whose font is Hiragino, which has it.

use crate::script_runs::script_runs;
use icu_properties::{
    CodePointMapData, CodePointSetData,
    props::{DefaultIgnorableCodePoint, GraphemeClusterBreak, Script},
    script::ScriptWithExtensions,
};

/// The fonts of the characters of `text` that need one other than their own: the fonts, and for each
/// character the index of the one it is set in (`None` for the characters left as they are).
///
/// `primary_covers[i]` is whether the font of the text has a glyph for the `i`th character.
/// `fallback_for(c)` is the font the system picks for `c`, `None` when there is none to speak of.
/// `covers(font, c)` is whether `font` has a glyph for `c`.
pub(crate) struct Assignment<Font> {
    pub fonts: Vec<Font>,
    pub per_character: Vec<Option<usize>>,
}

pub(crate) fn assign<Font>(
    text: &str,
    primary_covers: &[bool],
    mut fallback_for: impl FnMut(char) -> Option<Font>,
    covers: impl Fn(&Font, char) -> bool,
) -> Assignment<Font> {
    let characters: Vec<char> = text.chars().collect();
    assert_eq!(characters.len(), primary_covers.len());
    let mut fonts: Vec<Font> = Vec::new();
    let mut per_character: Vec<Option<usize>> = vec![None; characters.len()];
    let scripts = ScriptWithExtensions::new();
    let has_script_of_its_own = |character: char| {
        !matches!(
            scripts.get_script_val(character),
            Script::Common | Script::Inherited
        )
    };

    let mut first_of_run = 0;
    let mut byte = 0;
    let mut starts = Vec::with_capacity(characters.len());
    for character in &characters {
        starts.push(byte);
        byte += character.len_utf8();
    }
    for run in script_runs(text) {
        let first = first_of_run;
        let end = starts[first..].partition_point(|start| *start < run.range.end) + first;
        first_of_run = end;
        let mut pending: Vec<usize> = (first..end)
            .filter(|ix| !primary_covers[*ix] && needs_a_font_of_its_own(characters[*ix]))
            .collect();
        let mut asked: Vec<char> = Vec::new();
        while !pending.is_empty() {
            let hint = pending
                .iter()
                .copied()
                .find(|ix| has_script_of_its_own(characters[*ix]))
                .unwrap_or(pending[0]);
            let hint_character = characters[hint];
            if asked.contains(&hint_character) {
                break;
            }
            asked.push(hint_character);
            let Some(font) = fallback_for(hint_character) else {
                pending.retain(|ix| *ix != hint);
                continue;
            };
            let font_ix = fonts.len();
            let mut rest = Vec::new();
            let mut any = false;
            for ix in &pending {
                if covers(&font, characters[*ix]) {
                    per_character[*ix] = Some(font_ix);
                    any = true;
                } else {
                    rest.push(*ix);
                }
            }
            if any {
                fonts.push(font);
            } else {
                // A font that has not even the character it was asked for would be asked for again.
                rest.retain(|ix| *ix != hint);
            }
            pending = rest;
        }
    }

    // The characters that go with the one before (a mark, a joiner, a variation selector) are in
    // its font when it has them: a cluster is shaped by one font.
    for ix in 1..characters.len() {
        if !goes_with_the_one_before(characters[ix]) {
            continue;
        }
        if let Some(font_ix) = per_character[ix - 1]
            && covers(&fonts[font_ix], characters[ix])
        {
            per_character[ix] = Some(font_ix);
        }
    }
    Assignment {
        fonts,
        per_character,
    }
}

/// Whether a character is one that is set in a font on its own. Control and invisible characters
/// are left to the shaper, and the characters that join the one before come with its font.
fn needs_a_font_of_its_own(character: char) -> bool {
    !character.is_control()
        && !CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(character)
        && !goes_with_the_one_before(character)
}

fn goes_with_the_one_before(character: char) -> bool {
    matches!(
        CodePointMapData::<GraphemeClusterBreak>::new().get(character),
        GraphemeClusterBreak::Extend | GraphemeClusterBreak::ZWJ | GraphemeClusterBreak::SpacingMark
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fonts by name, with the characters they have glyphs for as a predicate.
    fn covers(font: &&str, character: char) -> bool {
        match *font {
            "PingFang" => {
                matches!(character, '\u{4E00}'..='\u{9FFF}' | '。' | '、' | '「' | '」' | '（' | '）')
            }
            "Hiragino" => {
                matches!(character, '\u{3040}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}')
                    || "。、「」（）\u{3099}".contains(character)
            }
            "Emoji" => matches!(character, '\u{1F300}'..='\u{1FAFF}' | '\u{200D}' | '\u{FE0F}'),
            other => unreachable!("{other}"),
        }
    }

    /// The font of the first of a script's characters, as the system finds it.
    fn system(character: char) -> Option<&'static str> {
        match character {
            'の' | 'こ' | 'あ' | 'ア' | 'テ' => Some("Hiragino"),
            '日' | '本' | '語' | '。' | '「' | '」' | '（' | '）' | '、' => Some("PingFang"),
            '\u{1F468}' | '\u{1F469}' => Some("Emoji"),
            _ => None,
        }
    }

    fn fonts_of(text: &str, primary: impl Fn(char) -> bool) -> Vec<Option<&'static str>> {
        let primary_covers: Vec<bool> = text.chars().map(primary).collect();
        let assignment = assign(text, &primary_covers, system, covers);
        assignment
            .per_character
            .iter()
            .map(|font| font.map(|ix| assignment.fonts[ix]))
            .collect()
    }

    fn latin(character: char) -> bool {
        character.is_ascii()
    }

    #[test]
    fn test_a_text_the_font_covers_is_left_alone() {
        assert_eq!(fonts_of("Hello, world", latin), vec![None; 12]);
    }

    #[test]
    fn test_the_full_stop_after_kana_is_in_the_font_of_the_kana() {
        // CoreText sets it in the font of the Han before the kana, which is not Chrome's choice.
        let fonts = fonts_of("日本語のテキストです。", latin);
        assert_eq!(&fonts[..3], &[Some("PingFang"); 3]);
        assert_eq!(&fonts[3..], &[Some("Hiragino"); 8][..], "{fonts:?}");
    }

    #[test]
    fn test_a_full_stop_after_han_is_in_the_font_of_the_han() {
        let fonts = fonts_of("日本語。", latin);
        assert_eq!(fonts, vec![Some("PingFang"); 4]);
    }

    #[test]
    fn test_punctuation_between_latin_letters_has_a_font_of_its_own() {
        // The CJK punctuation after a Latin letter starts a run, and the system is asked for a font
        // for it alone: PingFang here. Before kana it is in their run, and so in their font.
        assert_eq!(fonts_of("A「B", latin), vec![None, Some("PingFang"), None]);
        assert_eq!(
            fonts_of("A「あ", latin),
            vec![None, Some("Hiragino"), Some("Hiragino")]
        );
    }

    #[test]
    fn test_a_character_the_found_font_lacks_is_asked_for_again() {
        // PingFang is found for the Han but has no kana in this test: the kana ask for their own.
        let fonts = fonts_of("日あ", latin);
        assert_eq!(fonts, vec![Some("PingFang"), Some("Hiragino")]);
    }

    #[test]
    fn test_a_mark_goes_with_the_font_of_the_character_before_it() {
        let fonts = fonts_of("か\u{3099}", latin);
        assert_eq!(fonts, vec![None, None], "no system font for か here");
        let fonts = fonts_of("あ\u{3099}", latin);
        assert_eq!(fonts, vec![Some("Hiragino"); 2]);
    }

    #[test]
    fn test_emoji_joined_by_a_zero_width_joiner_are_one_font() {
        let fonts = fonts_of("\u{1F468}\u{200D}\u{1F469}", latin);
        assert_eq!(fonts, vec![Some("Emoji"); 3]);
    }

    #[test]
    fn test_control_characters_are_left_to_the_shaper() {
        let fonts = fonts_of("あ\t", latin);
        assert_eq!(fonts, vec![Some("Hiragino"), None]);
    }
}
