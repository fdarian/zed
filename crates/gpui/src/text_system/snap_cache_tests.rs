use super::*;
use crate::{TestAppContext, font, px};
use std::time::Instant;

const PARAGRAPHS: &[&str] = &[
    "AVATAR Tyrone Wavy To Yo Vo We. The quick brown fox jumps over the lazy dog, and then waits for Typewriter fixtures.",
    "inter\u{AD}national\u{AD}ization of the ac\u{AD}com\u{AD}mo\u{AD}da\u{AD}tion fa\u{AD}cil\u{AD}i\u{AD}ties",
    "Offices affirm that fifty-five fluffy waffles of the first kind are officially efficient.",
    "Short.",
    "Rows of tabular data\tthat carry a tab and  two  spaces  in a row, with a trailing space ",
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris.",
    "(Parentheses), [brackets], \"quotes\" and \u{201C}curly\u{201D} marks: AV, LT, P., F, 1/2 -- yes!",
    "日本語のテキスト。「括弧」と、句読点が、あります。mixed with Latin text",
    "Bold and regular items in a line, so the items kern across their edges: To and Wa and Yo",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "Two\u{FFFC}atomics in\u{FFFC}the text, between words that wrap around the boxes taking their place.",
];

const FONTS: &[&str] = &["Times New Roman", "Helvetica Neue", "Menlo", "Geneva", ".SystemUIFont"];

fn run(len: usize, family: &str, bold: bool) -> TextRun {
    let font = font(family.to_string());
    TextRun {
        len,
        font: if bold { font.bold() } else { font },
        ..Default::default()
    }
}

/// A text with its runs: the odd paragraphs are split into items of alternate weights.
fn runs_of(text: &str, family: &str, split: bool) -> Vec<TextRun> {
    if !split {
        return vec![run(text.len(), family, false)];
    }
    let mut runs = Vec::new();
    let (mut start, mut bold) = (0, false);
    for (index, _) in text.char_indices().filter(|(_, ch)| *ch == ' ').step_by(3) {
        runs.push(run(index - start, family, bold));
        start = index;
        bold = !bold;
    }
    runs.push(run(text.len() - start, family, bold));
    runs.retain(|run| run.len > 0);
    runs
}

fn atomics(text: &str) -> Vec<InlineBox> {
    text.match_indices('\u{FFFC}')
        .map(|(start, _)| InlineBox {
            range: start..start + '\u{FFFC}'.len_utf8(),
            parent: None,
            align: VerticalAlign::Baseline,
            line_height: None,
            wraps: None,
            key: None,
            content: InlineContent::Atomic(AtomicExtent {
                advance: px(23.),
                ascent: px(14.),
                descent: px(2.),
            }),
        })
        .collect()
}

/// What each text is laid out at: the min-content probe, max-content, and definite widths.
fn widths() -> Vec<(Option<Pixels>, TextLayoutOptions)> {
    let min_content = TextLayoutOptions::default().for_min_content();
    let plain = TextLayoutOptions::default();
    vec![
        (Some(px(0.)), min_content),
        (None, plain),
        (Some(px(37.)), plain),
        (Some(px(90.5)), plain),
        (Some(px(133.)), plain),
        (Some(px(241.25)), plain),
        (Some(px(500.)), plain),
        (Some(px(133.)), TextLayoutOptions { text_wrap_style: TextWrapStyle::Pretty, ..plain }),
    ]
}

fn lay_out(
    text_system: &WindowTextSystem,
    text: &str,
    family: &str,
    split: bool,
    width: (Option<Pixels>, TextLayoutOptions),
) -> String {
    shape(text_system, text, family, split, width)
        .iter()
        .map(|line| format!("{:?}", line.layout))
        .collect::<Vec<_>>()
        .join("\n")
}

fn shape(
    text_system: &WindowTextSystem,
    text: &str,
    family: &str,
    split: bool,
    width: (Option<Pixels>, TextLayoutOptions),
) -> SmallVec<[WrappedLine; 1]> {
    let inline = atomics(text);
    text_system
        .shape_text_inline(
            text.to_string().into(),
            px(15.),
            &runs_of(text, family, split),
            width.0,
            None,
            width.1,
            &[],
            &inline,
            px(20.),
        )
        .unwrap()
}

#[crate::test]
fn a_text_laid_out_at_many_widths_is_what_it_is_laid_out_alone(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let warm = WindowTextSystem::new(cx.text_system().clone());
        let mut compared = 0;
        for family in FONTS {
            for (ix, text) in PARAGRAPHS.iter().enumerate() {
                for split in [false, true] {
                    // Every width is laid out in a window that has laid out the text at the
                    // others, and in one that has laid out nothing.
                    let all = widths();
                    for width in all.iter().rev().chain(all.iter()) {
                        let cold = WindowTextSystem::new(cx.text_system().clone());
                        assert_eq!(
                            lay_out(&warm, text, family, split, *width),
                            lay_out(&cold, text, family, split, *width),
                            "{family} text {ix} split {split} at {:?}",
                            width.0
                        );
                        compared += 1;
                    }
                }
            }
        }
        assert!(compared > 1000);
        let counters = warm.text_counters();
        assert!(counters.snap_cache_hits > counters.snap_computations);
        assert!(counters.end_reshape_computations > 0);
    });
}

#[crate::test]
fn the_width_independent_marks_of_a_text_are_computed_once(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let text_system = WindowTextSystem::new(cx.text_system().clone());
        let text = PARAGRAPHS[0];
        let all = widths();
        for width in &all {
            lay_out(&text_system, text, "Times New Roman", false, *width);
        }
        let counters = text_system.text_counters();
        assert_eq!(counters.snap_computations, 1);
        assert_eq!(counters.snap_cache_hits, counters.wrapped_layouts - 1);
        assert!(counters.snap_cache_hits >= 6);
    });
}

/// `cargo test -p gpui --release --features test-support --lib -- --ignored --nocapture snap_cache_bench`
#[crate::test]
#[ignore]
fn snap_cache_bench(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let all = widths();
        // The faces with a `kern` table, which take an advance for every pair of letters, are
        // where the advances cost most.
        for (name, families) in [("system font", &FONTS[4..]), ("helvetica", &FONTS[1..2]), ("all faces", FONTS)] {
            let round = || {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                for (ix, text) in PARAGRAPHS.iter().enumerate() {
                    for width in &all[..5] {
                        shape(&text_system, text, families[ix % families.len()], true, *width);
                    }
                }
                text_system.text_counters()
            };
            round();
            let rounds = 40;
            let start = Instant::now();
            let mut counters = round();
            for _ in 1..rounds {
                counters = round();
            }
            let per_round = start.elapsed() / rounds;
            println!("snap_cache_bench: {name}: {per_round:?} per round of 11 texts at 5 widths; {counters:?}");
        }
    });
}
