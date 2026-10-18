//! How text is laid out in 1/64 of a pixel: wrapping, truncation, text-box-trim and atomic inlines.
use crate::{
    AnyElement, App, Bounds, Context, FixedMetricsTextSystem, InteractiveElement as _, IntoElement,
    LayoutId, NoopTextSystem, ParentElement as _, Pixels, PlatformTextSystem, Render, StyledText,
    Styled, TestApp, TextLayout, Window, div, px,
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

type Out = Rc<RefCell<Vec<(Bounds<Pixels>, TextLayout)>>>;

struct Page {
    build: Box<dyn Fn(&Out) -> AnyElement>,
    out: Out,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.out.borrow_mut().clear();
        (self.build)(&self.out)
    }
}

pub(super) fn laid_out(
    system: Arc<dyn PlatformTextSystem>,
    build: impl Fn(&Out) -> AnyElement + 'static,
) -> Vec<(Bounds<Pixels>, TextLayout)> {
    let mut app = TestApp::with_text_system(system);
    let out = Out::default();
    let mut window = app.open_window({
        let out = out.clone();
        move |_, _| Page {
            build: Box::new(build),
            out,
        }
    });
    window.draw();
    let result = out.borrow().clone();
    result
}

fn tagged(
    out: &Out,
    layout: TextLayout,
) -> impl Fn(Bounds<Pixels>, LayoutId, &mut Window, &mut App) + 'static {
    let out = out.clone();
    move |_, id, window, _| {
        let bounds = window.layout_unsnapped_bounds(id);
        out.borrow_mut().push((bounds, layout.clone()));
    }
}

fn systems() -> Vec<(&'static str, Arc<dyn PlatformTextSystem>)> {
    vec![
        ("noop", Arc::new(NoopTextSystem::new())),
        ("menlo", Arc::new(FixedMetricsTextSystem::menlo())),
    ]
}

const TEXTS: [&str; 11] = [
    "\u{1F44D}\u{1F44D} ok \u{1F389} done",
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} family",
    "\u{5E9}\u{5DC}\u{5D5}\u{5DD} \u{5E2}\u{5D5}\u{5DC}\u{5DD} abc",
    "e\u{301}e\u{301} cafe\u{301} \u{4E2D}\u{6587}\u{5B57}\u{7B26}",
    "trailing spaces   ",
    "Fractional widths of the quick brown fox",
    "a",
    "word",
    "two words",
    "a b c d e f g h i j k l m n o p q r s t u v w x y z",
    "supercalifragilisticexpialidocious",
];

/// A text in a flex item is laid out at its own measured width. It must not wrap.
#[test]
fn own_width_never_wraps_sweep() {
    let mut failures = Vec::new();
    for (name, system) in systems() {
        for text in TEXTS {
            for step in 0..400 {
                let size = 8. + step as f32 * 0.0731;
                let found = laid_out(system.clone(), move |out| {
                    let styled = StyledText::new(text);
                    let layout = styled.layout().clone();
                    div()
                        .w(px(2000.))
                        .flex()
                        .flex_col()
                        .items_start()
                        .line_height(px(20.))
                        .child(
                            div()
                                .flex()
                                .text_size(px(size))
                                .on_prepainted_layout(tagged(out, layout))
                                .child(styled),
                        )
                        .into_any_element()
                });
                let (bounds, layout) = &found[0];
                if bounds.size.height != px(20.) || layout.wrapped_text().contains('\n') {
                    failures.push(format!(
                        "{name} {text:?} size {size}: {:?} {:?}",
                        bounds.size,
                        layout.wrapped_text()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} wrapped, first: {:?}", failures.len(), &failures[..failures.len().min(5)]);
}

fn cut_by_own_width(ellipsis: &'static str) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, system) in systems() {
        for text in TEXTS {
            for step in 0..400 {
                let size = 8. + step as f32 * 0.0731;
                let found = laid_out(system.clone(), move |out| {
                    let styled = StyledText::new(text);
                    let layout = styled.layout().clone();
                    let mut item = div()
                        .flex()
                        .text_size(px(size))
                        .whitespace_nowrap()
                        .on_prepainted_layout(tagged(out, layout));
                    item = match ellipsis {
                        "end" => item.text_ellipsis(),
                        "start" => item.text_ellipsis_start(),
                        _ => item.text_ellipsis_middle(),
                    };
                    div()
                        .w(px(2000.))
                        .flex()
                        .flex_col()
                        .items_start()
                        .line_height(px(20.))
                        .child(item.child(styled))
                        .into_any_element()
                });
                let (_, layout) = &found[0];
                if layout.text() != text {
                    failures.push(format!("{name} {text:?} size {size}: cut to {:?}", layout.text()));
                }
            }
        }
    }
    failures
}

/// A text in a box as wide as the text is not cut.
#[test]
fn ellipsis_end_own_width_is_not_cut() {
    let failures = cut_by_own_width("end");
    assert!(failures.is_empty(), "{} cut, first {:?}", failures.len(), &failures[..failures.len().min(3)]);
}

#[test]
fn ellipsis_start_own_width_is_not_cut() {
    let failures = cut_by_own_width("start");
    assert!(failures.is_empty(), "{} cut, first {:?}", failures.len(), &failures[..failures.len().min(3)]);
}

/// The same for the middle ellipsis, whose comparison tolerates the float that a measured width comes back as.
#[test]
fn ellipsis_middle_own_width_is_not_cut() {
    let failures = cut_by_own_width("middle");
    assert!(failures.is_empty(), "{} cut, first {:?}", failures.len(), &failures[..failures.len().min(3)]);
}

fn in_box(
    system: Arc<dyn PlatformTextSystem>,
    text: &'static str,
    width: f32,
    size: f32,
    style: fn(gpui_div::Div) -> gpui_div::Div,
) -> (Bounds<Pixels>, TextLayout) {
    let found = laid_out(system, move |out| {
        let styled = StyledText::new(text);
        let layout = styled.layout().clone();
        let item = style(
            div()
                .w(px(width))
                .text_size(px(size))
                .line_height(px(20.))
                .on_prepainted_layout(tagged(out, layout)),
        );
        item.child(styled).into_any_element()
    });
    found.into_iter().next().expect("the text is laid out")
}

mod gpui_div {
    pub(super) use crate::Div;
}

/// Zero, tiny, huge and odd widths and empty or odd text lay out without panicking.
#[test]
fn odd_widths_do_not_panic() {
    let styles: [fn(gpui_div::Div) -> gpui_div::Div; 8] = [
        |d| d,
        |d| d.whitespace_nowrap().text_ellipsis(),
        |d| d.whitespace_nowrap().text_ellipsis_start(),
        |d| d.whitespace_nowrap().text_ellipsis_middle(),
        |d| d.line_clamp(2).text_ellipsis(),
        |d| d.line_clamp(1),
        |d| d.flex(),
        |d| d.flex().whitespace_nowrap().text_ellipsis_middle(),
    ];
    for (_, system) in systems() {
        for style in styles {
            for text in ["", " ", "a", "ab cd ef", "\u{1F389}\u{1F389}", "x\ny", "\n"] {
                for width in [0., 0.001, 0.01, 1. / 64., 0.3, 1., 5., 1e4, 1e6] {
                    for size in [0.01, 1., 13.37, 400.] {
                        let _ = in_box(system.clone(), text, width, size, style);
                    }
                }
            }
        }
    }
}

/// A clamped text shows an ellipsis exactly when the unclamped text needs more lines.
#[test]
fn line_clamp_boundary() {
    let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh";
    let mut failures = Vec::new();
    for (name, system) in systems() {
        for size in [10., 11.3, 13., 16.37] {
            let mut width = 20.;
            while width < 300. {
                let (_, full) = in_box(system.clone(), text, width, size, |d| d);
                let lines = full.wrapped_text().split('\n').count();
                for clamp in [1usize, 2, 3] {
                    let (_, clamped) = match clamp {
                        1 => in_box(system.clone(), text, width, size, |d| d.line_clamp(1).text_ellipsis()),
                        2 => in_box(system.clone(), text, width, size, |d| d.line_clamp(2).text_ellipsis()),
                        _ => in_box(system.clone(), text, width, size, |d| d.line_clamp(3).text_ellipsis()),
                    };
                    let shown = clamped.wrapped_text();
                    let cut = shown.contains('\u{2026}');
                    let rows = shown.split('\n').count();
                    if cut != (lines > clamp) || rows > clamp {
                        failures.push(format!(
                            "{name} size {size} width {width} clamp {clamp}: unclamped {lines} lines, shown {shown:?}"
                        ));
                    }
                }
                width += 0.37;
            }
        }
    }
    assert!(failures.is_empty(), "{} wrong, first {:?}", failures.len(), &failures[..failures.len().min(4)]);
}

/// A box (its width is a device pixel, 1/2 here) holds the text on a line iff the width the text
/// measured at is not over the box.
#[test]
fn wrap_follows_measured_width() {
    let text = "alpha beta gamma";
    let mut failures = Vec::new();
    for (name, system) in systems() {
        for step in 0..300 {
            let size = 9. + step as f32 * 0.0613;
            let found = laid_out(system.clone(), move |out| {
                let styled = StyledText::new(text);
                let layout = styled.layout().clone();
                div()
                    .w(px(2000.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .line_height(px(20.))
                    .child(div().flex().text_size(px(size)).on_prepainted_layout(tagged(out, layout)).child(styled))
                    .into_any_element()
            });
            let own = found[0].0.size.width.0;
            let below = (own * 2.).floor() / 2.;
            for width in [below, below + 0.5] {
                let (_, laid) = in_box(system.clone(), text, width, size, |d| d);
                let wraps = laid.wrapped_text().contains('\n');
                let should_wrap = own > width + 1. / 1024.;
                if wraps != should_wrap {
                    failures.push(format!("{name} size {size}: measured {own}, box {width}, wraps {wraps}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} wrong, first {:?}", failures.len(), &failures[..failures.len().min(4)]);
}

/// The baseline of a run is where its row says, also for a text with `text-box-trim`, whose lines start above its bounds.
#[test]
fn baseline_for_index_agrees_with_row_for_index_under_text_box_trim() {
    use crate::{TextBoxEdge, TextBoxOver, TextBoxTrim, TextBoxUnder};
    let found = laid_out(Arc::new(FixedMetricsTextSystem::menlo()), |out| {
        let styled = StyledText::new("Hello").with_text_box_trim(TextBoxTrim {
            start: true,
            end: true,
        });
        let layout = styled.layout().clone();
        let mut outer = div()
            .w(px(600.))
            .h(px(40.))
            .flex()
            .items_center()
            .text_size(px(14.))
            .line_height(px(21.));
        outer.text_style().text_box_edge = Some(TextBoxEdge {
            over: TextBoxOver::Cap,
            under: TextBoxUnder::Alphabetic,
        });
        outer
            .child(div().on_prepainted_layout(tagged(out, layout)).child(styled))
            .into_any_element()
    });
    let layout = &found[0].1;
    let (top, _) = layout.text_box_trim_amounts();
    assert!(top > px(0.));
    let row = layout.row_for_index(0).unwrap().baseline;
    let baseline = layout.baseline_for_index(0).unwrap();
    assert_eq!(baseline, row, "trim_top is {top:?}");
}

/// The bottom edge of an atomic inline in a trimmed text is on the baseline of its row.
#[test]
fn atomic_inline_sits_on_the_baseline_under_text_box_trim() {
    use crate::{AtomicInline, TextBoxEdge, TextBoxOver, TextBoxTrim, TextBoxUnder};
    let found = laid_out(Arc::new(FixedMetricsTextSystem::menlo()), |out| {
        let object = crate::OBJECT_REPLACEMENT_CHARACTER;
        let text = format!("x{object}");
        let index = 1;
        let styled = StyledText::new(text)
            .with_text_box_trim(TextBoxTrim {
                start: true,
                end: true,
            })
            .with_atomic_inlines(vec![AtomicInline::new(
                index,
                div()
                    .w(px(10.))
                    .h(px(8.))
                    .on_prepainted_layout({
                        let out = out.clone();
                        move |bounds, _, _, _| {
                            out.borrow_mut().push((bounds, TextLayout::default()));
                        }
                    })
                    .into_any_element(),
            )]);
        let layout = styled.layout().clone();
        let mut outer = div()
            .w(px(600.))
            .h(px(40.))
            .flex()
            .items_center()
            .text_size(px(14.))
            .line_height(px(21.));
        outer.text_style().text_box_edge = Some(TextBoxEdge {
            over: TextBoxOver::Cap,
            under: TextBoxUnder::Alphabetic,
        });
        outer
            .child(div().on_prepainted_layout(tagged(out, layout)).child(styled))
            .into_any_element()
    });
    let atomic = found.iter().find(|(_, layout)| layout.layout_id().is_none()).unwrap().0;
    let layout = &found.iter().find(|(_, layout)| layout.layout_id().is_some()).unwrap().1;
    let baseline = layout.row_for_index(0).unwrap().baseline;
    assert!(
        (atomic.bottom() - baseline).abs() < px(0.51),
        "box bottom {:?}, baseline {:?}, trim {:?}",
        atomic.bottom(),
        baseline,
        layout.text_box_trim_amounts()
    );
}

mod stale {
    use super::*;
    use crate::{
        OverflowWrap, TabSize, TextBoxEdge, TextBoxOver, TextBoxTrim, TextBoxUnder, TextInset,
        TextWrapStyle, WordBreak,
    };
    use std::cell::Cell;

    #[derive(Clone, Debug)]
    pub(super) struct P {
        pub(super) width: Option<f32>,
        pub(super) size: f32,
        pub(super) line_height: f32,
        pub(super) text: &'static str,
        pub(super) nowrap: bool,
        pub(super) clamp: Option<usize>,
        pub(super) overflow: u8,
        pub(super) word_break: u8,
        pub(super) overflow_wrap: u8,
        pub(super) tab: Option<f32>,
        pub(super) hang: bool,
        pub(super) break_spaces: bool,
        pub(super) wrap_style: u8,
        pub(super) trim: bool,
        pub(super) over: u8,
        pub(super) insets: u8,
        pub(super) run_size: Option<f32>,
    }

    impl P {
        pub(super) fn base() -> P {
            P {
                width: Some(120.),
                size: 14.,
                line_height: 20.,
                text: "alpha beta\tgamma delta epsilon zeta eta theta iota",
                nowrap: false,
                clamp: None,
                overflow: 0,
                word_break: 0,
                overflow_wrap: 1,
                tab: None,
                hang: false,
                break_spaces: false,
                wrap_style: 0,
                trim: false,
                over: 0,
                insets: 0,
                run_size: None,
            }
        }
    }

    fn build(p: P, out: &Out) -> AnyElement {
        let mut styled = StyledText::new(p.text);
        match p.insets {
            1 => styled = styled.with_insets(vec![TextInset { range: 0..5, leading: px(3.), trailing: px(4.) }]),
            2 => {
                styled = styled.with_insets_for_width(7, |w| {
                    vec![TextInset {
                        range: 0..5,
                        leading: px(w.map_or(1., |w| w.0 * 0.1)),
                        trailing: px(0.),
                    }]
                })
            }
            _ => {}
        }
        if let Some(run_size) = p.run_size {
            let mut first = crate::TextRun::default();
            first.len = 5;
            first.font_size = Some(px(run_size));
            let mut rest = crate::TextRun::default();
            rest.len = p.text.len() - 5;
            styled = styled.with_runs(vec![first, rest]);
        }
        if p.trim {
            styled = styled.with_text_box_trim(TextBoxTrim { start: true, end: true });
        }
        let layout = styled.layout().clone();
        let mut item = div()
            .text_size(px(p.size))
            .line_height(px(p.line_height))
            .on_prepainted_layout(tagged(out, layout));
        if let Some(width) = p.width {
            item = item.w(px(width));
        }
        if p.nowrap {
            item = item.whitespace_nowrap();
        }
        if let Some(clamp) = p.clamp {
            item = item.line_clamp(clamp);
        }
        item = match p.overflow {
            1 => item.text_ellipsis(),
            2 => item.text_ellipsis_start(),
            3 => item.text_ellipsis_middle(),
            _ => item,
        };
        {
            let style = item.text_style();
            style.word_break = Some([WordBreak::Normal, WordBreak::BreakAll, WordBreak::KeepAll][p.word_break as usize]);
            style.overflow_wrap = Some([OverflowWrap::Normal, OverflowWrap::BreakWord, OverflowWrap::Anywhere][p.overflow_wrap as usize]);
            style.tab_size = p.tab.map(|t| TabSize::Spaces(t));
            style.hang_trailing_spaces = Some(p.hang);
            style.break_spaces = Some(p.break_spaces);
            style.text_wrap_style = Some([TextWrapStyle::Auto, TextWrapStyle::Balance, TextWrapStyle::Pretty][p.wrap_style as usize]);
            style.text_box_edge = Some(TextBoxEdge {
                over: [TextBoxOver::Text, TextBoxOver::Cap, TextBoxOver::Ex][p.over as usize],
                under: TextBoxUnder::Alphabetic,
            });
        }
        // Shrink to fit when there is no width.
        div()
            .w(px(400.))
            .flex()
            .flex_col()
            .items_start()
            .child(item.child(styled))
            .into_any_element()
    }

    struct Frames {
        variant: Rc<Cell<usize>>,
        all: Vec<P>,
        out: Out,
    }

    impl Render for Frames {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.out.borrow_mut().clear();
            build(self.all[self.variant.get()].clone(), &self.out)
        }
    }

    pub(super) type Snapshot = (Bounds<Pixels>, String, String, Option<Pixels>);

    fn snapshot(out: &Out) -> Snapshot {
        let out = out.borrow();
        let (bounds, layout) = &out[0];
        (*bounds, layout.text(), layout.wrapped_text(), layout.first_baseline())
    }

    pub(super) fn run(system: Arc<dyn PlatformTextSystem>, sequence: Vec<P>) -> Vec<Snapshot> {
        let mut app = TestApp::with_text_system(system);
        let out = Out::default();
        let variant = Rc::new(Cell::new(0));
        let mut window = app.open_window({
            let (out, variant, all) = (out.clone(), variant.clone(), sequence.clone());
            move |_, _| Frames { variant, all, out }
        });
        let mut snapshots = Vec::new();
        for ix in 0..sequence.len() {
            variant.set(ix);
            window.update(|_, _, cx| cx.notify());
            window.draw();
            snapshots.push(snapshot(&out));
        }
        snapshots
    }

    pub(super) fn perturbations() -> Vec<(&'static str, P)> {
        let b = P::base();
        let mut v = Vec::new();
        macro_rules! with {
            ($name:expr, $($field:ident = $value:expr),+) => {{
                let mut p = b.clone();
                $(p.$field = $value;)+
                v.push(($name, p));
            }};
        }
        with!("width 80", width = Some(80.));
        with!("width 121.3", width = Some(121.3));
        with!("shrink", width = None);
        with!("size 15", size = 15.);
        with!("size 14.5", size = 14.5);
        with!("line-height 24", line_height = 24.);
        with!("text", text = "alpha beta\tgamma delta epsilon zeta eta theta iotb");
        with!("text2", text = "short");
        with!("nowrap", nowrap = true);
        with!("clamp1", clamp = Some(1));
        with!("clamp2 ellipsis", clamp = Some(2), overflow = 1);
        with!("ellipsis", overflow = 1, nowrap = true);
        with!("ellipsis start", overflow = 2, nowrap = true);
        with!("ellipsis middle", overflow = 3, nowrap = true);
        with!("ellipsis wrap", overflow = 1);
        with!("word-break all", word_break = 1);
        with!("word-break keep", word_break = 2);
        with!("overflow-wrap normal", overflow_wrap = 0);
        with!("tab 2", tab = Some(2.));
        with!("tab 8", tab = Some(8.));
        with!("hang", hang = true);
        with!("break-spaces", break_spaces = true);
        with!("balance", wrap_style = 1);
        with!("pretty", wrap_style = 2);
        with!("trim cap", trim = true, over = 1);
        with!("trim ex", trim = true, over = 2);
        with!("trim text", trim = true, over = 0);
        with!("insets", insets = 1);
        with!("insets for width", insets = 2);
        with!("run size", run_size = Some(20.));
        with!("run size 9", run_size = Some(9.));
        v.push(("base", b));
        v
    }
}

/// Whatever frame came before, a frame lays out as it does in a window that had no frame before.
#[test]
fn no_stale_measure_between_frames() {
    use stale::*;
    let system: Arc<dyn PlatformTextSystem> = Arc::new(NoopTextSystem::new());
    let variants = perturbations();
    let fresh: Vec<Snapshot> = variants
        .iter()
        .map(|(_, p)| run(system.clone(), vec![p.clone()]).remove(0))
        .collect();
    let mut distinct: Vec<&Snapshot> = Vec::new();
    for snapshot in &fresh {
        if !distinct.contains(&snapshot) {
            distinct.push(snapshot);
        }
    }
    assert!(distinct.len() >= 15, "the variants must differ: {}", distinct.len());
    let mut failures = Vec::new();
    for (a, (name_a, pa)) in variants.iter().enumerate() {
        for (b, (name_b, pb)) in variants.iter().enumerate() {
            let got = run(system.clone(), vec![pa.clone(), pb.clone()]).remove(1);
            if got != fresh[b] {
                failures.push(format!("{name_a} -> {name_b}: {:?} != fresh {:?}", got, fresh[b]));
            }
            // And the same frame twice, then it again.
            let again = run(system.clone(), vec![pb.clone(), pa.clone(), pb.clone()]).remove(2);
            if again != fresh[b] {
                failures.push(format!("{name_b} -> {name_a} -> {name_b}: {:?} != fresh {:?}", again, fresh[b]));
            }
        }
    }
    assert!(failures.is_empty(), "{} stale, first {:#?}", failures.len(), &failures[..failures.len().min(3)]);
}

mod stale_atomic {
    use super::*;
    use crate::AtomicInline;
    use std::cell::Cell;

    #[derive(Clone, Debug)]
    struct A {
        outer_width: Option<f32>,
        inner_text: &'static str,
        inner_width: Option<f32>,
        inner_size: f32,
        height: f32,
        second: bool,
        pad: f32,
        text: &'static str,
    }

    fn base() -> A {
        A {
            outer_width: Some(150.),
            inner_text: "inner box",
            inner_width: None,
            inner_size: 14.,
            height: 10.,
            second: false,
            pad: 0.,
            text: "ab \u{FFFC} cd ef gh ij kl mn op",
        }
    }

    fn build(a: A, out: &Out) -> AnyElement {
        let object = crate::OBJECT_REPLACEMENT_CHARACTER;
        let text = a.text.replace('\u{FFFC}', &object.to_string());
        let at = a.text.find('\u{FFFC}').unwrap();
        let _ = text;
        let mut inner = div()
            .px(px(a.pad))
            .h(px(a.height))
            .text_size(px(a.inner_size))
            .child(StyledText::new(a.inner_text));
        if let Some(w) = a.inner_width {
            inner = inner.w(px(w));
        }
        let mut atomics = vec![AtomicInline::new(at, inner.into_any_element())];
        let mut full = a.text.to_string();
        if a.second {
            let at2 = full.len();
            full.push(object);
            atomics.push(AtomicInline::new(at2, div().w(px(17.)).h(px(a.height)).into_any_element()));
        }
        let styled = StyledText::new(full).with_atomic_inlines(atomics);
        let layout = styled.layout().clone();
        let mut item = div().text_size(px(14.)).line_height(px(20.)).on_prepainted_layout(tagged(out, layout));
        if let Some(w) = a.outer_width {
            item = item.w(px(w));
        }
        div().w(px(400.)).flex().flex_col().items_start().child(item.child(styled)).into_any_element()
    }

    struct Frames {
        variant: Rc<Cell<usize>>,
        all: Vec<A>,
        out: Out,
    }

    impl Render for Frames {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.out.borrow_mut().clear();
            build(self.all[self.variant.get()].clone(), &self.out)
        }
    }

    type Snapshot = (Bounds<Pixels>, String, Option<Pixels>);

    fn run(sequence: Vec<A>) -> Vec<Snapshot> {
        let mut app = TestApp::with_text_system(Arc::new(NoopTextSystem::new()));
        let out = Out::default();
        let variant = Rc::new(Cell::new(0));
        let mut window = app.open_window({
            let (out, variant, all) = (out.clone(), variant.clone(), sequence.clone());
            move |_, _| Frames { variant, all, out }
        });
        let mut snapshots = Vec::new();
        for ix in 0..sequence.len() {
            variant.set(ix);
            window.update(|_, _, cx| cx.notify());
            window.draw();
            let out = out.borrow();
            let (bounds, layout) = &out[0];
            snapshots.push((*bounds, layout.wrapped_text(), layout.first_baseline()));
        }
        snapshots
    }

    #[test]
    fn no_stale_measure_with_atomics() {
        let b = base();
        let mut variants = Vec::new();
        let mut push = |name: &'static str, a: A| variants.push((name, a));
        push("outer 90", A { outer_width: Some(90.), ..b.clone() });
        push("shrink", A { outer_width: None, ..b.clone() });
        push("inner text", A { inner_text: "inner box with more words", ..b.clone() });
        push("inner short", A { inner_text: "i", ..b.clone() });
        push("inner width", A { inner_width: Some(40.), ..b.clone() });
        push("inner size", A { inner_size: 18., ..b.clone() });
        push("height", A { height: 26., ..b.clone() });
        push("second", A { second: true, ..b.clone() });
        push("pad", A { pad: 7., ..b.clone() });
        push("text", A { text: "xy \u{FFFC} cd ef gh ij kl mn oq", ..b.clone() });
        push("base", b);
        let fresh: Vec<Snapshot> = variants.iter().map(|(_, a)| run(vec![a.clone()]).remove(0)).collect();
        let mut failures = Vec::new();
        for (name_a, a) in &variants {
            for (ix, (name_b, bb)) in variants.iter().enumerate() {
                let got = run(vec![a.clone(), bb.clone()]).remove(1);
                if got != fresh[ix] {
                    failures.push(format!("{name_a} -> {name_b}: {got:?} != fresh {:?}", fresh[ix]));
                }
                let same_twice = run(vec![bb.clone(), a.clone(), bb.clone(), bb.clone()]).remove(3);
                if same_twice != fresh[ix] {
                    failures.push(format!("{name_b} -> {name_a} -> {name_b} x2: {same_twice:?} != fresh {:?}", fresh[ix]));
                }
            }
        }
        assert!(failures.is_empty(), "{} stale, first {:#?}", failures.len(), &failures[..failures.len().min(3)]);
    }
}

#[test]
fn trim_of_empty_text_does_not_panic() {
    use stale::*;
    for text in ["", " ", "\n", "\n\n", "a\n"] {
        for over in 0..3 {
            for width in [Some(0.), Some(0.001), Some(120.), None] {
                let mut p = P::base();
                p.text = text;
                p.trim = true;
                p.over = over;
                p.width = width;
                let _ = run(Arc::new(NoopTextSystem::new()), vec![p.clone()]);
                p.overflow = 3;
                p.nowrap = true;
                let _ = run(Arc::new(FixedMetricsTextSystem::menlo()), vec![p]);
            }
        }
    }
}

/// A text half a pixel wider than its box is cut by every ellipsis.
fn cut_when_over(kind: &'static str) -> String {
    let text = "abcdefghij abcdefghi"; // 20 characters, 12 * size wide
    let size = 100.5 / 12.;
    let found = laid_out(Arc::new(NoopTextSystem::new()), move |out| {
        let styled = StyledText::new(text);
        let layout = styled.layout().clone();
        let mut item = div()
            .w(px(100.))
            .text_size(px(size))
            .whitespace_nowrap()
            .overflow_hidden()
            .on_prepainted_layout(tagged(out, layout));
        item = match kind {
            "end" => item.text_ellipsis(),
            "start" => item.text_ellipsis_start(),
            _ => item.text_ellipsis_middle(),
        };
        item.child(styled).into_any_element()
    });
    found[0].1.text()
}

#[test]
fn ellipsis_end_cuts_text_half_a_pixel_over() {
    assert_ne!(cut_when_over("end"), "abcdefghij abcdefghi");
}

#[test]
fn ellipsis_start_cuts_text_half_a_pixel_over() {
    assert_ne!(cut_when_over("start"), "abcdefghij abcdefghi");
}

#[test]
fn ellipsis_middle_cuts_text_half_a_pixel_over() {
    assert_ne!(cut_when_over("middle"), "abcdefghij abcdefghi");
}

fn item_width_with_atomic(nowrap: bool) -> f32 {
    use crate::AtomicInline;
    let object = crate::OBJECT_REPLACEMENT_CHARACTER;
    let found = laid_out(Arc::new(NoopTextSystem::new()), move |out| {
        let styled = StyledText::new(format!("ab {object}")).with_atomic_inlines(vec![AtomicInline::new(
            3,
            div()
                .whitespace_normal()
                .child(StyledText::new("xxxx xxxx"))
                .into_any_element(),
        )]);
        let layout = styled.layout().clone();
        let mut item = div().text_size(px(14.)).line_height(px(20.)).on_prepainted_layout(tagged(out, layout));
        if nowrap {
            item = item.whitespace_nowrap();
        }
        div().w(px(30.)).flex().child(item.child(styled)).into_any_element()
    });
    found[0].0.size.width.0
}

/// A flex item with a `nowrap` text that has an atomic inline shrinks to the min-content of its
/// boxes. The text has no break, so its min-content is its whole line with each box at its own
/// min-content, which the max-content probe, asked for the same (no) width, must not answer.
#[test]
fn nowrap_text_with_atomic_has_min_content_of_its_boxes() {
    let wrapping = item_width_with_atomic(false);
    let nowrap = item_width_with_atomic(true);
    let char_width = 14. * 0.6;
    let max_content = 12. * char_width;
    // The wrapping item is as wide as the widest of "ab" and the box at its min-content; the
    // nowrap one has "ab " and that box on one line.
    let expected = 3. * char_width + wrapping;
    assert!(nowrap < max_content, "nowrap item is {nowrap} wide, the max-content size {max_content}");
    assert!((nowrap - expected).abs() < 0.02, "nowrap item is {nowrap} wide, not {expected}");
}
