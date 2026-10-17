//! Elements laid out inline in a text, as CSS lays out an `inline-block`, `inline-flex`
//! or `inline-grid` box: [`StyledText::with_atomic_inlines`](crate::StyledText).
//!
//! The text owns the elements. Each one is the root of a layout tree of its own, in an
//! engine the text keeps, because the text measures them while the layout of the window
//! is running, when the engine of the window is taken. The text measures a box before it
//! shapes, since the width of the box is what the line is shaped with: its min-content
//! and max-content widths, and the width it takes between them given the width the text
//! has, which is CSS's shrink-to-fit. The box is laid out again at that width, and its
//! baseline is read. In prepaint and paint the text swaps the engine of the box back in,
//! and prepaints and paints the box where its placeholder is.

use crate::{
    AbsoluteLength, AnyElement, App, AtomicExtent, AvailableSpace, DefiniteLength, Edges,
    InlineBox, InlineContent, LayoutId, Pixels, Point, Size, TaffyLayoutEngine, VerticalAlign,
    Window, px, size,
};

/// Where the baseline of an [`AtomicInline`] is.
pub enum AtomicBaseline {
    /// The bottom margin edge: what CSS does for a box with no line of text in it, or
    /// with `overflow` other than `visible`, and for a replaced element.
    BottomMarginEdge,
    /// Read from the box once it is laid out, as the distance from the top of its border
    /// box, or `None` for the bottom margin edge. It is called with the window set up for
    /// the box, so that `Window::layout_bounds` of any node inside it works, with the
    /// layout id of the root of the box and the size of its border box. CSS has the last
    /// line box of an `inline-block` and the first baseline of an `inline-flex` or
    /// `inline-grid` here.
    Measured(Box<dyn Fn(&mut Window, LayoutId, Size<Pixels>) -> Option<Pixels>>),
}

/// An element that sits in a text in place of one U+FFFC character, like an
/// `inline-block`.
pub struct AtomicInline {
    /// The byte of the text the U+FFFC character is at. The text's runs must cover it.
    pub index: usize,
    /// The element. It is laid out as a root: its width is the shrink-to-fit width,
    /// whatever its style says if that is `auto`, and its `margin` is the one below.
    pub element: AnyElement,
    /// The margin of the box, which the element must not have itself. A percentage is of
    /// the width the text is laid out in, and of nothing while that is not known.
    pub margin: Edges<DefiniteLength>,
    /// How the box is aligned on its line.
    pub vertical_align: VerticalAlign,
    /// The `line-height` of the element, which a percentage `vertical_align` is of.
    /// `None` is the line height of the text.
    pub line_height: Option<Pixels>,
    /// The index of the [`InlineBox`] of the text the box is in, if it has one.
    pub parent: Option<usize>,
    /// Where the baseline of the box is.
    pub baseline: AtomicBaseline,
}

impl AtomicInline {
    /// A box with no margin, aligned to the baseline, whose baseline is its bottom edge.
    pub fn new(index: usize, element: AnyElement) -> Self {
        Self {
            index,
            element,
            margin: Edges::default(),
            vertical_align: VerticalAlign::Baseline,
            line_height: None,
            parent: None,
            baseline: AtomicBaseline::BottomMarginEdge,
        }
    }
}

/// The width a text gives its atomic inlines to be measured at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum AtomicMeasure {
    /// Each box as narrow as it goes without overflowing.
    MinContent,
    /// Each box as wide as its content.
    MaxContent,
    /// Each box between the two, within the width the text is laid out in.
    Fit(Pixels),
}

/// How the root of a box was laid out last.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    MinContent,
    MaxContent,
    Exact(Pixels),
}

struct Slot {
    index: usize,
    element: AnyElement,
    layout_id: LayoutId,
    margin: Edges<DefiniteLength>,
    align: VerticalAlign,
    line_height: Option<Pixels>,
    parent: Option<usize>,
    baseline: AtomicBaseline,
    /// What the engine holds of the box.
    laid_out_as: Option<Layout>,
    min_width: Option<Pixels>,
    max_size: Option<Size<Pixels>>,
    /// How the box was laid out for the last measure, which is what the text shaped with.
    used: Option<Layout>,
    /// The margins in px the text shaped with.
    used_margin: Edges<Pixels>,
}

/// The atomic inlines of a text.
pub(crate) struct AtomicInlines {
    engine: Option<TaffyLayoutEngine>,
    slots: Vec<Slot>,
}

impl AtomicInlines {
    /// Requests the layout of the elements, in an engine of their own.
    pub(crate) fn new(atomics: Vec<AtomicInline>, window: &mut Window, cx: &mut App) -> Self {
        let mut engine = Some(TaffyLayoutEngine::new());
        let slots = atomics
            .into_iter()
            .map(|mut atomic| {
                let layout_id = window.with_layout_engine(&mut engine, |window| {
                    atomic.element.request_layout(window, cx)
                });
                Slot {
                    index: atomic.index,
                    element: atomic.element,
                    layout_id,
                    margin: atomic.margin,
                    align: atomic.vertical_align,
                    line_height: atomic.line_height,
                    parent: atomic.parent,
                    baseline: atomic.baseline,
                    laid_out_as: None,
                    min_width: None,
                    max_size: None,
                    used: None,
                    used_margin: Edges::default(),
                }
            })
            .collect();
        Self { engine, slots }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Measures every box for a text laid out at `measure`, and returns them as the
    /// inline boxes the text is shaped with.
    pub(crate) fn measure(
        &mut self,
        measure: AtomicMeasure,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<InlineBox> {
        let rem_size = window.rem_size();
        let mut engine = self.engine.take();
        let boxes = window.with_layout_engine(&mut engine, |window| {
            self.slots
                .iter_mut()
                .map(|slot| {
                    let extent = slot.measure(measure, rem_size, window, cx);
                    InlineBox {
                        range: slot.index
                            ..slot.index + crate::OBJECT_REPLACEMENT_CHARACTER.len_utf8(),
                        parent: slot.parent,
                        align: slot.align,
                        line_height: slot.line_height,
                        content: InlineContent::Atomic(extent),
                    }
                })
                .collect()
        });
        self.engine = engine;
        boxes
    }

    /// Prepaints each box with its margin box at the origin `origins` gives its byte.
    pub(crate) fn prepaint(
        &mut self,
        origins: &[(usize, Point<Pixels>)],
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut engine = self.engine.take();
        window.with_layout_engine(&mut engine, |window| {
            for slot in &mut self.slots {
                let Some((_, origin)) = origins.iter().find(|(index, _)| *index == slot.index)
                else {
                    continue;
                };
                slot.restore_layout(window, cx);
                let origin = *origin + crate::point(slot.used_margin.left, slot.used_margin.top);
                slot.element.prepaint_at(origin, window, cx);
            }
        });
        self.engine = engine;
    }

    pub(crate) fn paint(&mut self, window: &mut Window, cx: &mut App) {
        let mut engine = self.engine.take();
        window.with_layout_engine(&mut engine, |window| {
            for slot in &mut self.slots {
                slot.element.paint(window, cx);
            }
        });
        self.engine = engine;
    }
}

impl Slot {
    fn lay_out(&mut self, layout: Layout, window: &mut Window, cx: &mut App) -> Size<Pixels> {
        if self.laid_out_as != Some(layout) {
            let space = match layout {
                Layout::MinContent => {
                    window.set_layout_forced_width(self.layout_id, None);
                    size(AvailableSpace::MinContent, AvailableSpace::MaxContent)
                }
                Layout::MaxContent => {
                    window.set_layout_forced_width(self.layout_id, None);
                    size(AvailableSpace::MaxContent, AvailableSpace::MaxContent)
                }
                Layout::Exact(width) => {
                    window.set_layout_forced_width(self.layout_id, Some(width));
                    size(AvailableSpace::Definite(width), AvailableSpace::MaxContent)
                }
            };
            window.compute_layout(self.layout_id, space, cx);
            self.laid_out_as = Some(layout);
        }
        window.layout_bounds(self.layout_id).size
    }

    fn max_size(&mut self, window: &mut Window, cx: &mut App) -> Size<Pixels> {
        match self.max_size {
            Some(max_size) => max_size,
            None => {
                let max_size = self.lay_out(Layout::MaxContent, window, cx);
                self.max_size = Some(max_size);
                max_size
            }
        }
    }

    fn min_width(&mut self, window: &mut Window, cx: &mut App) -> Pixels {
        match self.min_width {
            Some(min_width) => min_width,
            None => {
                let min_width = self.lay_out(Layout::MinContent, window, cx).width;
                self.min_width = Some(min_width);
                min_width
            }
        }
    }

    /// The box laid out for `measure`, as the extent the text gives it.
    fn measure(
        &mut self,
        measure: AtomicMeasure,
        rem_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> AtomicExtent {
        let containing_width = match measure {
            AtomicMeasure::Fit(width) => width,
            AtomicMeasure::MinContent | AtomicMeasure::MaxContent => px(0.),
        };
        let margin = self
            .margin
            .clone()
            .map(|length| length.to_pixels(AbsoluteLength::Pixels(containing_width), rem_size));
        let max_size = self.max_size(window, cx);
        let layout = match measure {
            AtomicMeasure::MaxContent => Layout::MaxContent,
            AtomicMeasure::MinContent => Layout::Exact(self.min_width(window, cx)),
            AtomicMeasure::Fit(width) => {
                let available = (width - margin.left - margin.right).max(px(0.));
                if available >= max_size.width {
                    Layout::MaxContent
                } else {
                    let min_width = self.min_width(window, cx);
                    Layout::Exact(available.max(min_width).min(max_size.width))
                }
            }
        };
        let layout = match layout {
            Layout::Exact(width) if width == max_size.width => Layout::MaxContent,
            layout => layout,
        };
        let border_box = self.lay_out(layout, window, cx);
        self.used = Some(layout);
        self.used_margin = margin.clone();

        let from_top = match &self.baseline {
            AtomicBaseline::BottomMarginEdge => None,
            AtomicBaseline::Measured(measure) => measure(window, self.layout_id, border_box),
        };
        let advance = margin.left + border_box.width + margin.right;
        match from_top {
            Some(from_top) => AtomicExtent {
                advance,
                ascent: margin.top + from_top,
                descent: border_box.height - from_top + margin.bottom,
            },
            None => AtomicExtent {
                advance,
                ascent: margin.top + border_box.height + margin.bottom,
                descent: px(0.),
            },
        }
    }

    /// Lays the box out as the text last measured it, if the engine holds another layout
    /// of it, as a measure for another width may have left.
    fn restore_layout(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(used) = self.used {
            self.lay_out(used, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The expected numbers are from headless Chrome (chrome-headless-shell, not the
    //! Chrome app), with `body { margin: 0; font: 20px/20px Menlo }`, which the
    //! `FixedMetricsTextSystem::menlo()` of these tests mimics: a character is 12.042px
    //! wide, the font has an ascent of 19px and a descent of 5px, so a line of 20px has
    //! a strut 17px above its baseline and 3px below. Each case says which HTML it is.
    //! A box is `<span style="display:inline-block;width:Wpx;height:Hpx">`, and a box
    //! of height 0 shows where the baseline is. The numbers are relative to the top of
    //! the paragraph, which is `<div style="width:...">`.
    use super::*;
    use crate::{
        Context, FixedMetricsTextSystem, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, Styled as _, StyledText, TestApp, div,
    };
    use std::{collections::HashMap, sync::Arc};

    /// One atomic inline of a test paragraph.
    #[derive(Clone)]
    pub(super) struct Atomic {
        pub width: f32,
        pub height: f32,
        pub align: VerticalAlign,
        /// Top, right, bottom, left.
        pub margin: [f32; 4],
    }

    pub(super) fn atomic(width: f32, height: f32) -> Atomic {
        Atomic {
            width,
            height,
            align: VerticalAlign::Baseline,
            margin: [0.; 4],
        }
    }

    impl Atomic {
        pub(super) fn aligned(mut self, align: VerticalAlign) -> Self {
            self.align = align;
            self
        }

        pub(super) fn margin(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
            self.margin = [top, right, bottom, left];
            self
        }
    }

    pub(super) const OBJECT: char = crate::OBJECT_REPLACEMENT_CHARACTER;

    struct Paragraph {
        width: f32,
        text: String,
        atomics: Vec<Atomic>,
    }

    impl Render for Paragraph {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let atomics: Vec<AtomicInline> = self
                .text
                .match_indices(OBJECT)
                .zip(&self.atomics)
                .enumerate()
                .map(|(ix, ((index, _), atomic))| {
                    let selector: &'static str = Box::leak(format!("box{ix}").into_boxed_str());
                    let mut inline = AtomicInline::new(
                        index,
                        div()
                            .debug_selector(|| selector.to_string())
                            .w(crate::px(atomic.width))
                            .h(crate::px(atomic.height))
                            .into_any_element(),
                    );
                    inline.vertical_align = atomic.align;
                    let [top, right, bottom, left] = atomic.margin;
                    inline.margin = Edges {
                        top: crate::px(top).into(),
                        right: crate::px(right).into(),
                        bottom: crate::px(bottom).into(),
                        left: crate::px(left).into(),
                    };
                    inline
                })
                .collect();
            // The window's root fills the window, so the paragraph is a child of one.
            div().child(
                div()
                    .debug_selector(|| "paragraph".into())
                    .w(crate::px(self.width))
                    .text_size(crate::px(20.))
                    .line_height(crate::px(20.))
                    .child(StyledText::new(self.text.clone()).with_atomic_inlines(atomics)),
            )
        }
    }

    /// The bounds of the paragraph and of its boxes, relative to the paragraph.
    pub(super) struct Laid {
        pub paragraph: crate::Bounds<Pixels>,
        pub boxes: Vec<crate::Bounds<Pixels>>,
    }

    pub(super) fn lay_out(width: f32, text: &str, atomics: Vec<Atomic>) -> Laid {
        let mut app = TestApp::with_text_system(Arc::new(FixedMetricsTextSystem::menlo()));
        let count = atomics.len();
        let mut window = app.open_window({
            let text = text.to_string();
            move |_, _| Paragraph {
                width,
                text,
                atomics,
            }
        });
        window.draw();
        window.update(|_, window, _| {
            let bounds: HashMap<String, crate::Bounds<Pixels>> = window
                .rendered_frame
                .debug_bounds
                .iter()
                .map(|(key, value)| (key.to_string(), *value))
                .collect();
            let paragraph = bounds["paragraph"];
            Laid {
                paragraph,
                boxes: (0..count)
                    .map(|ix| {
                        let mut bounds = bounds[&format!("box{ix}")];
                        bounds.origin = bounds.origin - paragraph.origin;
                        bounds
                    })
                    .collect(),
            }
        })
    }

    pub(super) fn assert_close(actual: Pixels, expected: f32, context: &str) {
        // Positions are on whole device pixels here, and Chrome's are in 1/64.
        assert!(
            (f32::from(actual) - expected).abs() <= 0.51,
            "{context}: {actual:?} != {expected}"
        );
    }

    /// `<div id=l><span class=mark></span><span>xg</span><span class=box style="vertical-align: ...">`
    /// with a mark of `width:1px;height:0`, a box of `10px` by `10px`:
    /// (line height, baseline, box top, box bottom) from Chrome.
    #[test]
    fn vertical_align_keywords_place_a_box_as_chrome_does() {
        let em = 20.;
        let cases: Vec<(&str, VerticalAlign, [f32; 4])> = vec![
            ("baseline", VerticalAlign::Baseline, [20., 17., 7., 17.]),
            ("sub", VerticalAlign::Sub, [22., 17., 12., 22.]),
            ("super", VerticalAlign::Super, [20.656, 17.656, 0., 10.]),
            ("text-top", VerticalAlign::TextTop, [22., 19., 0., 10.]),
            (
                "text-bottom",
                VerticalAlign::TextBottom,
                [22., 17., 12., 22.],
            ),
            ("middle", VerticalAlign::Middle, [20., 17., 6.531, 16.531]),
            (
                "5px",
                VerticalAlign::Offset {
                    length: crate::px(5.),
                    line_height_fraction: 0.,
                },
                [20., 17., 2., 12.],
            ),
            (
                "50%",
                VerticalAlign::Offset {
                    length: crate::px(0.),
                    line_height_fraction: 0.5,
                },
                [23., 20., 0., 10.],
            ),
            (
                "-3px",
                VerticalAlign::Offset {
                    length: crate::px(-3.),
                    line_height_fraction: 0.,
                },
                [20., 17., 10., 20.],
            ),
            ("top", VerticalAlign::Top, [20., 17., 0., 10.]),
            ("bottom", VerticalAlign::Bottom, [20., 17., 10., 20.]),
        ];
        let _ = em;
        let text = format!("{OBJECT}xg{OBJECT}");
        for (name, align, [line_height, baseline, top, bottom]) in cases {
            let laid = lay_out(
                400.,
                &text,
                vec![atomic(1., 0.), atomic(10., 10.).aligned(align)],
            );
            assert_close(laid.paragraph.size.height, line_height, name);
            assert_close(
                laid.boxes[0].origin.y,
                baseline,
                &format!("{name} baseline"),
            );
            assert_close(laid.boxes[1].origin.y, top, &format!("{name} top"));
            assert_close(laid.boxes[1].bottom(), bottom, &format!("{name} bottom"));
        }
    }

    /// Each case is a paragraph `<div style="width:400px">xx` + boxes + a mark, or only the
    /// boxes and the mark when `text` is empty. Chrome's numbers are `height`, and
    /// the `[x, y]` of each box and the mark; `x` is after the 24.09px of "xx".
    #[test]
    fn the_line_box_grows_around_boxes_as_chrome_says() {
        struct Case {
            name: &'static str,
            text: &'static str,
            boxes: Vec<Atomic>,
            height: f32,
            origins: Vec<(f32, f32)>,
        }
        let top = |w, h| atomic(w, h).aligned(VerticalAlign::Top);
        let bottom = |w, h| atomic(w, h).aligned(VerticalAlign::Bottom);
        let mark = || atomic(1., 0.);
        let x = 24.09;
        let cases = vec![
            // <span class=b style="width:10px;height:30px"> is 30px above the baseline
            // and the strut has 3px below it.
            Case {
                name: "a baseline box taller than the strut",
                text: "xx",
                boxes: vec![atomic(10., 30.), mark()],
                height: 33.,
                origins: vec![(x, 0.), (x + 10., 30.)],
            },
            // vertical-align:top makes the line as tall as the box, below the strut.
            Case {
                name: "a top box taller than the line",
                text: "xx",
                boxes: vec![top(10., 30.), mark()],
                height: 30.,
                origins: vec![(x, 0.), (x + 10., 17.)],
            },
            // vertical-align:bottom grows the line above the strut instead.
            Case {
                name: "a bottom box taller than the line",
                text: "xx",
                boxes: vec![bottom(10., 40.), mark()],
                height: 40.,
                origins: vec![(x, 0.), (x + 10., 37.)],
            },
            Case {
                name: "a top box and a taller bottom box: the line grows up",
                text: "xx",
                boxes: vec![top(10., 30.), bottom(10., 40.), mark()],
                height: 40.,
                origins: vec![(x, 0.), (x + 10., 0.), (x + 20., 37.)],
            },
            Case {
                name: "a top box and a taller baseline box",
                text: "xx",
                boxes: vec![top(10., 30.), atomic(10., 40.), mark()],
                height: 43.,
                origins: vec![(x, 0.), (x + 10., 0.), (x + 20., 40.)],
            },
            // margin:3px 4px 5px 6px: the baseline is the bottom margin edge.
            Case {
                name: "margins are part of the box the line grows around",
                text: "xx",
                boxes: vec![atomic(10., 10.).margin(3., 4., 5., 6.), mark()],
                height: 21.,
                origins: vec![(x + 6., 3.), (x + 20., 18.)],
            },
            Case {
                name: "a line of a box alone still has its strut",
                text: "",
                boxes: vec![atomic(10., 10.), mark()],
                height: 20.,
                origins: vec![(0., 7.), (10., 17.)],
            },
            Case {
                name: "a top box and a bottom box of the same height: the line grows down",
                text: "",
                boxes: vec![top(10., 25.), bottom(10., 25.), mark()],
                height: 25.,
                origins: vec![(0., 0.), (10., 0.), (20., 17.)],
            },
            Case {
                name: "a bottom box and a taller top box: the line grows down",
                text: "xx",
                boxes: vec![bottom(10., 40.), top(10., 50.), mark()],
                height: 50.,
                origins: vec![(x, 10.), (x + 10., 0.), (x + 20., 17.)],
            },
        ];
        for case in cases {
            let text = format!(
                "{}{}",
                case.text,
                OBJECT.to_string().repeat(case.boxes.len())
            );
            let laid = lay_out(400., &text, case.boxes);
            assert_close(laid.paragraph.size.height, case.height, case.name);
            for (ix, (x, y)) in case.origins.into_iter().enumerate() {
                assert_close(
                    laid.boxes[ix].origin.x,
                    x,
                    &format!("{} x of {ix}", case.name),
                );
                assert_close(
                    laid.boxes[ix].origin.y,
                    y,
                    &format!("{} y of {ix}", case.name),
                );
            }
        }
    }
}
