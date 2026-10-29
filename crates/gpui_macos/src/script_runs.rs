//! The script runs of a text, cut as Blink cuts them (`ScriptRunIterator`,
//! `platform/fonts/script_run_iterator.cc`): the stretches that one font is looked for. A character of
//! the Common or Inherited script (a space, a digit, a mark) is in the run around it, a character that
//! belongs to several scripts (the CJK punctuation) stays in a run that has one of them and starts the
//! next otherwise, and a closing bracket takes the script of its opening one.
//!
//! Positions are byte offsets, where Blink's are UTF-16 offsets. Blink's fast path for a run of one
//! script is left out: it only skips the work of the general path.

use icu_properties::{
    CodePointMapData,
    props::{BidiMirroringGlyph, BidiPairedBracketType, EastAsianWidth, Script},
    script::{ScriptWithExtensions, ScriptWithExtensionsBorrowed},
};
use std::{collections::VecDeque, mem, ops::Range};

/// The deepest stack of open brackets that is kept (`kMaxBrackets`).
const MAX_BRACKETS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScriptRun {
    pub range: Range<usize>,
    pub script: Script,
}

pub(crate) fn script_runs(text: &str) -> Vec<ScriptRun> {
    let mut iterator = ScriptRunIterator::new(text);
    let mut runs = Vec::new();
    let mut start = 0;
    while let Some((limit, script)) = iterator.consume() {
        runs.push(ScriptRun {
            range: start..limit,
            script,
        });
        start = limit;
    }
    runs
}

/// The scripts that make the text of a character, in the order Blink tries them in
/// (`IcuScriptData::GetScripts`): the first is the one it prefers.
fn scripts_of(character: char) -> Vec<Script> {
    let data = ScriptWithExtensions::new();
    let primary = match data.get_script_val(character) {
        // Both kana are `kana` in OpenType, and one script keeps the runs from splitting between them.
        Script::Katakana | Script::KatakanaOrHiragana => Script::Hiragana,
        script => script,
    };
    if primary == Script::Hiragana {
        return vec![Script::Hiragana];
    }
    let mut set: Vec<Script> = data.get_script_extensions_val(character).iter().collect();
    if set.is_empty() || primary == set[0] {
        return set;
    }
    if primary != Script::Inherited && primary != Script::Common && primary != Script::Unknown {
        // The extensions are not in order: the script the character has goes first.
        match set.iter().skip(1).position(|script| *script == primary) {
            Some(ix) => set.swap(0, ix + 1),
            None => {
                set.push(primary);
                let last = set.len() - 1;
                set.swap(0, last);
            }
        }
        return set;
    }
    if primary == Script::Common {
        if set.len() == 1 {
            // Common with a preferred script.
            set.insert(0, Script::Common);
            return set;
        }
        // Common is ignored: the preferred of the scripts that remain goes first.
        for ix in 1..set.len() {
            if set[0] == Script::Latin || set[ix] < set[0] {
                set.swap(0, ix);
            }
        }
        return set;
    }
    // Inherited, with other scripts: it goes first, then the preferred of the others.
    set.push(set[0]);
    set[0] = primary;
    for ix in 2..set.len() {
        if set[1] == Script::Latin || set[ix] < set[1] {
            set.swap(1, ix);
        }
    }
    set
}

fn is_han_script(script: Script) -> bool {
    script == Script::Han || script == Script::Hiragana || script == Script::Bopomofo
}

/// The scripts of the CJK punctuation (those of U+300C), which a halfwidth or fullwidth form of a
/// common character is given: `FixScriptsByEastAsianWidth`.
fn han_scripts() -> Vec<Script> {
    let scripts = scripts_of('\u{300C}');
    assert!(!scripts.is_empty(), "U+300C has scripts");
    scripts
}

struct Bracket {
    character: char,
    script: Script,
}

struct ScriptRunIterator<'a> {
    text: &'a str,
    brackets: VecDeque<Bracket>,
    brackets_fixup_depth: usize,
    current_set: Vec<Script>,
    next_set: Vec<Script>,
    ahead_set: Vec<Script>,
    ahead_character: char,
    /// The byte after `ahead_character`, one past the end of the text once the last character has
    /// been fetched.
    ahead_pos: usize,
    common_preferred: Script,
    scripts: ScriptWithExtensionsBorrowed<'static>,
}

impl<'a> ScriptRunIterator<'a> {
    fn new(text: &'a str) -> Self {
        let mut iterator = Self {
            text,
            brackets: VecDeque::new(),
            brackets_fixup_depth: 0,
            current_set: Vec::new(),
            next_set: Vec::new(),
            ahead_set: Vec::new(),
            ahead_character: '\0',
            ahead_pos: 0,
            common_preferred: Script::Common,
            scripts: ScriptWithExtensions::new(),
        };
        if let Some(first) = text.chars().next() {
            // Common first, so that the first character decides the script of the run.
            iterator.current_set.push(Script::Common);
            iterator.ahead_character = first;
            iterator.ahead_pos = first.len_utf8();
            iterator.ahead_set = scripts_of(first);
        }
        iterator
    }

    /// The end of the next run and its script, or `None` after the last.
    fn consume(&mut self) -> Option<(usize, Script)> {
        if self.current_set.is_empty() {
            return None;
        }
        while let Some((pos, character)) = self.fetch() {
            let bracket = CodePointMapData::<BidiMirroringGlyph>::new()
                .get(character)
                .paired_bracket_type;
            match bracket {
                BidiPairedBracketType::Open => self.open_bracket(character),
                BidiPairedBracketType::Close => self.close_bracket(character),
                _ => {}
            }
            if !self.merge_sets() {
                let script = self.resolve_current_script();
                // An open bracket belongs to the next run, so it does not take this one's script.
                self.fixup_stack(script, bracket == BidiPairedBracketType::Open);
                self.current_set = self.next_set.clone();
                return Some((pos, script));
            }
        }
        let script = self.resolve_current_script();
        self.current_set.clear();
        Some((self.text.len(), script))
    }

    fn open_bracket(&mut self, character: char) {
        if self.brackets.len() == MAX_BRACKETS {
            self.brackets.pop_front();
            if self.brackets_fixup_depth == MAX_BRACKETS {
                self.brackets_fixup_depth -= 1;
            }
        }
        self.fix_scripts_by_east_asian_width(character);
        self.brackets.push_back(Bracket {
            character,
            script: Script::Common,
        });
        self.brackets_fixup_depth += 1;
    }

    /// Halfwidth and fullwidth forms have the properties of the character they are the form of, and
    /// that splits a run of CJK punctuation in two when a Latin character comes before it.
    fn fix_scripts_by_east_asian_width(&mut self, character: char) {
        if self.next_set.len() > 1 || self.next_set.first() != Some(&Script::Common) {
            return;
        }
        let width = CodePointMapData::<EastAsianWidth>::new().get(character);
        if matches!(
            width,
            EastAsianWidth::Wide | EastAsianWidth::Fullwidth | EastAsianWidth::Halfwidth
        ) {
            self.next_set = han_scripts();
        }
    }

    fn close_bracket(&mut self, character: char) {
        let Some(target) = CodePointMapData::<BidiMirroringGlyph>::new()
            .get(character)
            .mirroring_glyph
        else {
            return;
        };
        let Some(from_top) = self
            .brackets
            .iter()
            .rev()
            .position(|bracket| bracket.character == target)
        else {
            // No bracket to match: the stack stays.
            return;
        };
        let matched = self.brackets.len() - 1 - from_top;
        let mut script = self.brackets[matched].script;
        // The Han languages are several scripts, and font features apply to consecutive
        // punctuation: a closing bracket that could be any Han script of the run keeps it whole.
        if is_han_script(script)
            && let Some(current) = self.current_set.iter().copied().find(|script| is_han_script(*script))
        {
            script = current;
        }
        if script != Script::Common {
            self.next_set.clear();
            self.next_set.push(script);
        }
        // The brackets above the match are dropped; the match stays, as in Blink.
        self.brackets.truncate(matched + 1);
        self.brackets_fixup_depth = self.brackets_fixup_depth.saturating_sub(from_top);
    }

    /// Keeps the scripts of `current_set` that `next_set` has. When the two have none in common the
    /// run ends: `false`, and `current_set` is left as it was.
    fn merge_sets(&mut self) -> bool {
        if self.next_set.is_empty() || self.current_set.is_empty() {
            return false;
        }
        let mut priority = self.current_set[0];

        // A common or inherited character can only change the preferred script of common ones.
        if self.next_set[0] <= Script::Inherited {
            if self.next_set.len() == 2
                && priority <= Script::Inherited
                && self.common_preferred == Script::Common
            {
                self.common_preferred = self.next_set[1];
            }
            return true;
        }

        if priority <= Script::Inherited {
            self.current_set = self.next_set.clone();
            return true;
        }

        let mut have_priority = self.next_set.contains(&priority);
        if self.current_set.len() == 1 {
            return have_priority;
        }

        // The remaining scripts of both, after the priority one.
        let mut next_rest = &self.next_set[..];
        if !have_priority {
            priority = self.next_set[0];
            next_rest = &self.next_set[1..];
            have_priority = self.current_set[1..].contains(&priority);
        }
        let mut merged = Vec::new();
        if have_priority {
            merged.push(priority);
        }
        if !next_rest.is_empty() {
            for script in &self.current_set[1..] {
                if next_rest.contains(script) {
                    merged.push(*script);
                }
            }
        }
        if merged.is_empty() {
            return false;
        }
        self.current_set = merged;
        true
    }

    /// The run ended: the open brackets seen in it are of the script it resolved to, but for the one
    /// that ended it when `exclude_last`.
    fn fixup_stack(&mut self, resolved: Script, exclude_last: bool) {
        let mut count = self.brackets_fixup_depth.min(self.brackets.len());
        if count == 0 {
            return;
        }
        let mut skip = 0;
        if exclude_last {
            skip = 1;
            count -= 1;
            self.brackets_fixup_depth = 1;
        } else {
            self.brackets_fixup_depth = 0;
        }
        let len = self.brackets.len();
        for bracket in self.brackets.range_mut(len - skip - count..len - skip) {
            bracket.script = resolved;
        }
    }

    /// The next character and where it is. Its scripts become `next_set` and the character after it
    /// is read ahead.
    fn fetch(&mut self) -> Option<(usize, char)> {
        if self.ahead_pos > self.text.len() {
            return None;
        }
        let pos = self.ahead_pos - self.ahead_character.len_utf8();
        let character = self.ahead_character;
        mem::swap(&mut self.next_set, &mut self.ahead_set);
        if self.ahead_pos == self.text.len() {
            // The last character is still to be processed.
            self.ahead_pos += 1;
            return Some((pos, character));
        }
        self.fetch_next_character();
        Some((pos, character))
    }

    fn fetch_next_character(&mut self) {
        let next = self.text[self.ahead_pos..]
            .chars()
            .next()
            .expect("a character is left");
        self.ahead_character = next;
        self.ahead_pos += next.len_utf8();
        self.ahead_set = scripts_of(next);
        if self.ahead_set[0] == Script::Inherited && self.ahead_set.len() > 1 {
            if self.next_set[0] == Script::Common {
                // The character before takes the scripts of the mark that follows it.
                self.next_set = self.ahead_set.clone();
                self.next_set.remove(0);
            }
            self.ahead_set.truncate(1);
        }
    }

    fn resolve_current_script(&self) -> Script {
        match self.current_set[0] {
            Script::Common => self.common_preferred,
            script => script,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(text: &str) -> Vec<(&str, Script)> {
        script_runs(text)
            .into_iter()
            .map(|run| (&text[run.range], run.script))
            .collect()
    }

    #[test]
    fn test_an_empty_text_has_no_run() {
        assert_eq!(runs(""), vec![]);
    }

    #[test]
    fn test_common_characters_are_a_run_of_their_own_only_when_nothing_else_is_there() {
        assert_eq!(runs(" ... !?"), vec![(" ... !?", Script::Common)]);
        assert_eq!(runs("Unicode "), vec![("Unicode ", Script::Latin)]);
        assert_eq!(
            runs("Unicode 萬國碼"),
            vec![("Unicode ", Script::Latin), ("萬國碼", Script::Han)]
        );
        assert_eq!(
            runs("萬國碼 Unicode"),
            vec![("萬國碼 ", Script::Han), ("Unicode", Script::Latin)]
        );
        assert_eq!(runs("百家姓🌱🌲🌳🌴"), vec![("百家姓🌱🌲🌳🌴", Script::Han)]);
    }

    #[test]
    fn test_the_two_kana_are_one_script_with_the_punctuation_they_share() {
        for shared in ["あ", "ア", "\u{30FC}", "\u{303C}", "\u{3003}", "\u{3001}"] {
            for kana in ["か", "カ"] {
                for text in [
                    format!("{kana}{shared}"),
                    format!("{shared}{kana}"),
                    format!("{kana}{shared}{kana}"),
                ] {
                    assert_eq!(runs(&text), vec![(text.as_str(), Script::Hiragana)]);
                }
            }
        }
    }

    #[test]
    fn test_a_closing_bracket_without_an_opening_one_is_ignored() {
        assert_eq!(
            runs("(萬a]"),
            vec![("(萬", Script::Han), ("a]", Script::Latin)]
        );
        assert_eq!(
            runs("Unicode (萬國碼] ) Unicode\""),
            vec![
                ("Unicode (", Script::Latin),
                ("萬國碼] ", Script::Han),
                (") Unicode\"", Script::Latin)
            ]
        );
    }

    #[test]
    fn test_a_closing_bracket_takes_the_script_of_its_opening_one() {
        assert_eq!(runs("(萬國碼)"), vec![("(萬國碼)", Script::Han)]);
        assert_eq!(runs("(Unicode)"), vec![("(Unicode)", Script::Latin)]);
        assert_eq!(
            runs("Unicode (萬國碼)"),
            vec![
                ("Unicode (", Script::Latin),
                ("萬國碼", Script::Han),
                (")", Script::Latin)
            ]
        );
    }

    #[test]
    fn test_consecutive_cjk_punctuation_is_not_split() {
        assert_eq!(
            runs("「あ国。」"),
            vec![("「あ", Script::Hiragana), ("国。」", Script::Han)]
        );
        assert_eq!(
            runs("あ「あ国（国）」"),
            vec![("あ「あ", Script::Hiragana), ("国（国）」", Script::Han)]
        );
        assert_eq!(
            runs("国「国ア（A）」"),
            vec![
                ("国「国", Script::Han),
                ("ア（", Script::Hiragana),
                ("A", Script::Latin),
                ("）」", Script::Hiragana)
            ]
        );
    }

    #[test]
    fn test_cjk_punctuation_after_latin_is_a_run_of_its_own() {
        assert_eq!(
            runs("A「Aあ（国）」"),
            vec![
                ("A", Script::Latin),
                ("「", Script::Bopomofo),
                ("A", Script::Latin),
                ("あ（", Script::Hiragana),
                ("国）」", Script::Han)
            ]
        );
        assert_eq!(
            runs("「A「A」」"),
            vec![
                ("「", Script::Bopomofo),
                ("A", Script::Latin),
                ("「", Script::Bopomofo),
                ("A", Script::Latin),
                ("」」", Script::Bopomofo)
            ]
        );
    }

    #[test]
    fn test_a_closing_bracket_is_matched_through_31_open_ones() {
        let opens = "(".repeat(31);
        let closes = ")".repeat(31);
        let text = format!("[萬國碼 Unicode {opens}!{closes}]");
        assert_eq!(
            runs(&text),
            vec![
                ("[萬國碼 ", Script::Han),
                (&text["[萬國碼 ".len()..text.len() - 1], Script::Latin),
                ("]", Script::Han)
            ]
        );
    }

    #[test]
    fn test_only_the_32_latest_open_brackets_are_matched() {
        // Past 32 the oldest are forgotten: the three `(` before the `[` are, and the `[` is not.
        let opens = "(".repeat(31);
        let closes = ")".repeat(31);
        let text = format!("((([萬國碼 Unicode {opens}萬國碼!{closes}]But )))");
        assert_eq!(
            runs(&text),
            vec![
                ("((([萬國碼 ", Script::Han),
                (&format!("Unicode {opens}"), Script::Latin),
                ("萬國碼!", Script::Han),
                (&closes, Script::Latin),
                ("]", Script::Han),
                ("But )))", Script::Latin),
            ]
        );
    }
}
