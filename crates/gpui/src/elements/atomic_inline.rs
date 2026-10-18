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

use std::{
    cell::{Cell, RefCell},
    hash::{DefaultHasher, Hash as _, Hasher as _},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    AbsoluteLength, AnyElement, App, AtomicExtent, AvailableSpace, DefiniteLength, Edges,
    InlineBox, InlineContent, LayoutId, Pixels, Point, Size, TaffyLayoutEngine, VerticalAlign,
    Window, px, size, taffy::RequestSignature,
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
    /// The `width` of the box when it is a percentage of the width the text is laid out in
    /// (or a `calc()` with one), which the element cannot resolve as the root of a layout
    /// of its own: the element must have an `auto` width then. While the text has no width
    /// to resolve it against, as when it is measured for its min-content or max-content,
    /// the box is `auto`, which is what CSS makes of it.
    pub width: Option<DefiniteLength>,
    /// How the box is aligned on its line.
    pub vertical_align: VerticalAlign,
    /// The `line-height` of the element, which a percentage `vertical_align` is of.
    /// `None` is the line height of the text.
    pub line_height: Option<Pixels>,
    /// The index of the [`InlineBox`] of the text the box is in, if it has one.
    pub parent: Option<usize>,
    /// Where the baseline of the box is.
    pub baseline: AtomicBaseline,
    /// How far the box is painted from where the text puts it, like the offsets of
    /// `position: relative`: nothing else on the line moves with it. A fraction in `x` is of
    /// the width the text is laid out in, and one in `y` of [`Self::containing_height`].
    pub offset: Point<DefiniteLength>,
    /// The height the fractions in the `y` of [`Self::offset`] are of: the content height of
    /// the containing block, written by the owner of the text before the text is prepainted.
    /// An owner whose containing block has no definite height (CSS then computes the offset
    /// as `auto`) leaves a fraction out of `offset`.
    pub containing_height: Rc<Cell<Pixels>>,
}

impl AtomicInline {
    /// A box with no margin, aligned to the baseline, whose baseline is its bottom edge.
    pub fn new(index: usize, element: AnyElement) -> Self {
        Self {
            index,
            element,
            margin: Edges::default(),
            width: None,
            vertical_align: VerticalAlign::Baseline,
            line_height: None,
            parent: None,
            baseline: AtomicBaseline::BottomMarginEdge,
            offset: Point::default(),
            containing_height: Rc::default(),
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

/// How the root of a box was laid out last. The second field is the width of the containing block,
/// which the percentages of the box are of, for a box with percentage limits. A box laid out at a
/// width of its own cannot tell it from its available space.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    MinContent(Option<Pixels>),
    MaxContent(Option<Pixels>),
    Exact(Pixels, Option<Pixels>),
}

struct Slot {
    index: usize,
    element: AnyElement,
    layout_id: LayoutId,
    margin: Edges<DefiniteLength>,
    width: Option<DefiniteLength>,
    align: VerticalAlign,
    line_height: Option<Pixels>,
    parent: Option<usize>,
    baseline: AtomicBaseline,
    offset: Point<DefiniteLength>,
    containing_height: Rc<Cell<Pixels>>,
    /// Whether the `min-width` or `max-width` of the box is a percentage, which depends on the
    /// width of the containing block and so on the width the text is measured at.
    percent_limits: bool,
    /// What the engine holds of the box.
    laid_out_as: Option<Layout>,
    /// The min-content width, for the containing width it was measured at.
    min_width: Option<(Option<Pixels>, Pixels)>,
    /// The max-content size, for the containing width it was measured at.
    max_size: Option<(Option<Pixels>, Size<Pixels>)>,
    /// How the box was laid out for the last measure, which is what the text shaped with.
    used: Option<Layout>,
    /// The margins in px the text shaped with.
    used_margin: Edges<Pixels>,
    /// The width the text shaped with, which a fraction of the offset is of.
    used_containing_width: Pixels,
}

/// What the atomic inlines of one text keep from one frame to the next, so that a text whose boxes
/// did not change is not measured again: the engine they are laid out in, which finds the node of
/// each element by its key and changes only what differs, and how the text last measured each box.
/// The text keeps it on its layout node, see [`StyledText`](crate::StyledText).
pub(crate) struct AtomicCarry {
    /// Tells this carry from every other one, and so a measure of another text's boxes from this
    /// text's.
    id: u64,
    engine: Option<TaffyLayoutEngine>,
    /// How each box was laid out for the last measure, which prepaint lays it out as again when the
    /// text keeps that measure.
    boxes: Vec<BoxUse>,
    /// Changes whenever a frame asks for other boxes than the frame before.
    epoch: u64,
    seen: Vec<Seen>,
    /// The pass of the engine that laid out the text which has it, so that two texts do not use it.
    claimed_in: Option<(u64, u64)>,
}

#[derive(Clone)]
struct BoxUse {
    used: Option<Layout>,
    used_margin: Edges<Pixels>,
    used_containing_width: Pixels,
}

/// What a frame asked for, for one box.
#[derive(Clone, PartialEq)]
struct Seen {
    layout: RequestSignature,
    index: usize,
    margin: Edges<DefiniteLength>,
    width: Option<DefiniteLength>,
    align: VerticalAlign,
    line_height: Option<Pixels>,
    parent: Option<usize>,
    offset: Point<DefiniteLength>,
    percent_limits: bool,
    /// Whether the baseline is read off the box: what it reads is part of the box.
    measured_baseline: bool,
}

impl AtomicCarry {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            engine: None,
            boxes: Vec::new(),
            epoch: 0,
            seen: Vec::new(),
            claimed_in: None,
        }
    }
}

enum Store {
    /// Kept for the next frame's text to find again.
    Carried(Rc<RefCell<AtomicCarry>>),
    /// Used by this frame alone, because a box in it cannot be found again by a key.
    Own(Option<TaffyLayoutEngine>),
}

/// The atomic inlines of a text.
pub(crate) struct AtomicInlines {
    store: Store,
    slots: Vec<Slot>,
}

impl AtomicInlines {
    /// Requests the layout of the elements, in an engine of their own: the one `previous` holds,
    /// the carry of the frame before, if that is there to be used.
    pub(crate) fn new(
        atomics: Vec<AtomicInline>,
        previous: Option<Rc<RefCell<AtomicCarry>>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let pass = window.layout_engine_pass();
        let carry = previous
            .filter(|carry| carry.borrow().claimed_in != Some(pass))
            .unwrap_or_else(|| Rc::new(RefCell::new(AtomicCarry::new())));
        carry.borrow_mut().claimed_in = Some(pass);
        let mut engine = Some(match carry.borrow_mut().engine.take() {
            Some(mut engine) => {
                // The pass of the frame before is over.
                engine.end_pass();
                engine
            }
            None => TaffyLayoutEngine::new(),
        });
        let unstable_before = engine.as_ref().expect("just set").unstable_requests();
        let mut slots: Vec<Slot> = atomics
            .into_iter()
            .map(|mut atomic| {
                let (layout_id, percent_limits) = window.with_layout_engine(&mut engine, |window| {
                    let layout_id = atomic.element.request_layout(window, cx);
                    (layout_id, window.layout_has_percent_width_limits(layout_id))
                });
                Slot {
                    index: atomic.index,
                    element: atomic.element,
                    layout_id,
                    margin: atomic.margin,
                    width: atomic.width,
                    align: atomic.vertical_align,
                    line_height: atomic.line_height,
                    parent: atomic.parent,
                    baseline: atomic.baseline,
                    offset: atomic.offset,
                    containing_height: atomic.containing_height,
                    percent_limits,
                    laid_out_as: None,
                    min_width: None,
                    max_size: None,
                    used: None,
                    used_margin: Edges::default(),
                    used_containing_width: Pixels::ZERO,
                }
            })
            .collect();

        // A box that was found again by its key, and whose subtree asked for what it asked for
        // the frame before, is laid out as it was. One that was made anew every frame, or whose
        // layout is rewritten after the request, is not, and neither is a box that is a leaf.
        let engine_ref = engine.as_ref().expect("just set");
        let signatures: Option<Vec<RequestSignature>> = (engine_ref.unstable_requests()
            == unstable_before)
            .then(|| {
                slots
                    .iter()
                    .map(|slot| engine_ref.request_signature(slot.layout_id))
                    .collect()
            })
            .flatten();
        let Some(signatures) = signatures else {
            return Self {
                store: Store::Own(engine),
                slots,
            };
        };
        let seen: Vec<Seen> = slots
            .iter()
            .zip(signatures)
            .map(|(slot, layout)| Seen {
                layout,
                index: slot.index,
                margin: slot.margin.clone(),
                width: slot.width,
                align: slot.align,
                line_height: slot.line_height,
                parent: slot.parent,
                offset: slot.offset,
                percent_limits: slot.percent_limits,
                measured_baseline: matches!(slot.baseline, AtomicBaseline::Measured(_)),
            })
            .collect();
        {
            let mut carried = carry.borrow_mut();
            if carried.seen != seen {
                carried.epoch += 1;
                carried.seen = seen;
                carried.boxes.clear();
            }
            if carried.boxes.len() == slots.len() {
                for (slot, used) in slots.iter_mut().zip(&carried.boxes) {
                    slot.used = used.used;
                    slot.used_margin = used.used_margin.clone();
                    slot.used_containing_width = used.used_containing_width;
                }
            }
            carried.engine = engine;
        }
        Self {
            store: Store::Carried(carry),
            slots,
        }
    }

    /// The carry that the text keeps for the next frame's, if the boxes can be found again.
    pub(crate) fn carry(&self) -> Option<Rc<RefCell<AtomicCarry>>> {
        match &self.store {
            Store::Carried(carry) => Some(carry.clone()),
            Store::Own(_) => None,
        }
    }

    /// Stands for the boxes as they are laid out, which two frames that make the text measure the
    /// same have in common, or `None` when a box cannot be told from the one before. What a text
    /// measured with boxes that have it is the same as long as the rest of its key is.
    pub(crate) fn digest(&self) -> Option<u64> {
        let Store::Carried(carry) = &self.store else {
            return None;
        };
        let carry = carry.borrow();
        let mut hasher = DefaultHasher::new();
        carry.id.hash(&mut hasher);
        carry.epoch.hash(&mut hasher);
        Some(hasher.finish())
    }

    fn take_engine(&mut self) -> Option<TaffyLayoutEngine> {
        match &mut self.store {
            Store::Carried(carry) => carry.borrow_mut().engine.take(),
            Store::Own(engine) => engine.take(),
        }
    }

    fn put_engine(&mut self, engine: Option<TaffyLayoutEngine>) {
        match &mut self.store {
            Store::Carried(carry) => carry.borrow_mut().engine = engine,
            Store::Own(own) => *own = engine,
        }
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
        let mut engine = self.take_engine();
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
        self.put_engine(engine);
        if let Store::Carried(carry) = &self.store {
            carry.borrow_mut().boxes = self
                .slots
                .iter()
                .map(|slot| BoxUse {
                    used: slot.used,
                    used_margin: slot.used_margin.clone(),
                    used_containing_width: slot.used_containing_width,
                })
                .collect();
        }
        boxes
    }

    /// Prepaints each box with its margin box at the origin `origins` gives its byte.
    pub(crate) fn prepaint(
        &mut self,
        origins: &[(usize, Point<Pixels>)],
        window: &mut Window,
        cx: &mut App,
    ) {
        let rem_size = window.rem_size();
        let mut engine = self.take_engine();
        window.with_layout_engine(&mut engine, |window| {
            for slot in &mut self.slots {
                let Some((_, origin)) = origins.iter().find(|(index, _)| *index == slot.index)
                else {
                    continue;
                };
                slot.restore_layout(window, cx);
                let offset = crate::point(
                    slot.offset.x.to_pixels(slot.used_containing_width.into(), rem_size),
                    slot.offset.y.to_pixels(slot.containing_height.get().into(), rem_size),
                );
                let origin = *origin
                    + crate::point(slot.used_margin.left, slot.used_margin.top)
                    + offset;
                slot.element.prepaint_at(origin, window, cx);
            }
        });
        self.put_engine(engine);
    }

    pub(crate) fn paint(&mut self, window: &mut Window, cx: &mut App) {
        let mut engine = self.take_engine();
        window.with_layout_engine(&mut engine, |window| {
            for slot in &mut self.slots {
                slot.element.paint(window, cx);
            }
        });
        self.put_engine(engine);
    }
}

impl Slot {
    fn lay_out(&mut self, layout: Layout, window: &mut Window, cx: &mut App) -> Size<Pixels> {
        if self.laid_out_as != Some(layout) {
            let (space, containing_width) = match layout {
                Layout::MinContent(containing_width) => {
                    window.set_layout_forced_width(self.layout_id, None);
                    (
                        size(AvailableSpace::MinContent, AvailableSpace::MaxContent),
                        containing_width,
                    )
                }
                Layout::MaxContent(containing_width) => {
                    window.set_layout_forced_width(self.layout_id, None);
                    (
                        size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                        containing_width,
                    )
                }
                Layout::Exact(width, containing_width) => {
                    window.set_layout_forced_width(self.layout_id, Some(width));
                    (
                        size(AvailableSpace::Definite(width), AvailableSpace::MaxContent),
                        containing_width,
                    )
                }
            };
            if self.percent_limits {
                window.compute_layout_in_containing_width(
                    self.layout_id,
                    space,
                    containing_width,
                    cx,
                );
            } else {
                window.compute_layout(self.layout_id, space, cx);
            }
            #[cfg(test)]
            tests::LAYOUT_PASSES.with(|passes| passes.set(passes.get() + 1));
            self.laid_out_as = Some(layout);
        }
        // Not snapped to device pixels: the text is shaped with this width.
        window.layout_unsnapped_bounds(self.layout_id).size
    }

    fn max_size(
        &mut self,
        containing_width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Size<Pixels> {
        match self.max_size {
            Some((measured_at, max_size)) if measured_at == containing_width => max_size,
            _ => {
                let max_size = self.lay_out(Layout::MaxContent(containing_width), window, cx);
                self.max_size = Some((containing_width, max_size));
                max_size
            }
        }
    }

    fn min_width(
        &mut self,
        containing_width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Pixels {
        match self.min_width {
            Some((measured_at, min_width)) if measured_at == containing_width => min_width,
            _ => {
                let min_width = self
                    .lay_out(Layout::MinContent(containing_width), window, cx)
                    .width;
                self.min_width = Some((containing_width, min_width));
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
        // Only a box with percentage limits can tell the width it is measured at from any other.
        let limits_width = match measure {
            AtomicMeasure::Fit(width) if self.percent_limits => Some(width),
            _ => None,
        };
        let max_size = self.max_size(limits_width, window, cx);
        let layout = match measure {
            AtomicMeasure::MaxContent => Layout::MaxContent(limits_width),
            AtomicMeasure::MinContent => {
                Layout::Exact(self.min_width(limits_width, window, cx), limits_width)
            }
            AtomicMeasure::Fit(width) if self.width.is_some() => {
                let length = self.width.expect("checked above");
                Layout::Exact(
                    length
                        .to_pixels(AbsoluteLength::Pixels(width), rem_size)
                        .max(px(0.)),
                    limits_width,
                )
            }
            AtomicMeasure::Fit(width) => {
                let available = (width - margin.left - margin.right).max(px(0.));
                if available >= max_size.width {
                    Layout::Exact(max_size.width, limits_width)
                } else {
                    let min_width = self.min_width(limits_width, window, cx);
                    Layout::Exact(
                        available.max(min_width).min(max_size.width),
                        limits_width,
                    )
                }
            }
        };
        let border_box = self.lay_out(layout, window, cx);
        self.used = Some(layout);
        self.used_margin = margin.clone();
        self.used_containing_width = containing_width;

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
        Render, StatefulInteractiveElement as _, Styled as _, StyledText, TestApp, TextLayout,
        TextRun, div,
    };
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
        ops::Range,
        rc::Rc,
        sync::Arc,
    };

    /// One atomic inline of a test paragraph: a box of a size, or a box that holds text.
    #[derive(Clone)]
    pub(super) struct Atomic {
        pub width: Option<f32>,
        /// The width as a fraction of the width of the paragraph.
        pub width_fraction: Option<f32>,
        /// `min-width` and `max-width` as fractions of the width of the paragraph.
        pub min_width_fraction: Option<f32>,
        pub max_width_fraction: Option<f32>,
        /// `width: fit-content`, with the `width` above not set.
        pub fit_content: bool,
        pub height: Option<f32>,
        pub text: Option<&'static str>,
        /// Top, right, bottom, left.
        pub padding: [f32; 4],
        pub align: VerticalAlign,
        /// Top, right, bottom, left.
        pub margin: [f32; 4],
        /// Counts the clicks on the box, and gives it a background.
        pub clicks: Option<Rc<Cell<usize>>>,
        /// The box is a flex row of boxes of these sizes, and its baseline is the bottom of
        /// the first of them, as the first baseline of a flex container is.
        pub flex_items: Vec<(f32, f32)>,
        /// Painted this far from where the text puts the box.
        pub offset: (f32, f32),
        /// The index of the span the box is in.
        pub parent: Option<usize>,
        /// Added to `offset`: a fraction of the width of the paragraph and one of
        /// `containing_height`.
        pub offset_fraction: (f32, f32),
        pub containing_height: f32,
    }

    pub(super) fn atomic(width: f32, height: f32) -> Atomic {
        Atomic {
            width: Some(width),
            width_fraction: None,
            min_width_fraction: None,
            max_width_fraction: None,
            fit_content: false,
            height: Some(height),
            text: None,
            padding: [0.; 4],
            align: VerticalAlign::Baseline,
            margin: [0.; 4],
            clicks: None,
            flex_items: Vec::new(),
            offset: (0., 0.),
            parent: None,
            containing_height: 0.,
            offset_fraction: (0., 0.),
        }
    }

    /// A box with the text in it and the size of the text, whose baseline is the one of
    /// its last line.
    pub(super) fn text_box(text: &'static str) -> Atomic {
        Atomic {
            width: None,
            width_fraction: None,
            height: None,
            text: Some(text),
            ..atomic(0., 0.)
        }
    }

    impl Atomic {
        pub(super) fn in_span(mut self, span: usize) -> Self {
            self.parent = Some(span);
            self
        }

        pub(super) fn offset_of(mut self, x: f32, y: f32, containing_height: f32) -> Self {
            self.offset_fraction = (x, y);
            self.containing_height = containing_height;
            self
        }

        pub(super) fn aligned(mut self, align: VerticalAlign) -> Self {
            self.align = align;
            self
        }

        pub(super) fn margin(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
            self.margin = [top, right, bottom, left];
            self
        }

        pub(super) fn offset(mut self, x: f32, y: f32) -> Self {
            self.offset = (x, y);
            self
        }

        pub(super) fn padding(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
            self.padding = [top, right, bottom, left];
            self
        }

        pub(super) fn width(mut self, width: f32) -> Self {
            self.width = Some(width);
            self
        }

        fn inline(&self, index: usize, selector: &'static str) -> AtomicInline {
            let mut child = div().id(selector).debug_selector(|| selector.to_string());
            if let Some(clicks) = self.clicks.clone() {
                child = child
                    .bg(crate::red())
                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1));
            }
            if let Some(width) = self.width {
                child = child.w(crate::px(width));
            }
            if self.fit_content {
                child = child.w_fit_content();
            }
            if let Some(fraction) = self.min_width_fraction {
                child = child.min_w(crate::relative(fraction));
            }
            if let Some(fraction) = self.max_width_fraction {
                child = child.max_w(crate::relative(fraction));
            }
            if let Some(height) = self.height {
                child = child.h(crate::px(height));
            }
            let [top, right, bottom, left] = self.padding;
            child = child
                .pt(crate::px(top))
                .pr(crate::px(right))
                .pb(crate::px(bottom))
                .pl(crate::px(left));
            let mut baseline = AtomicBaseline::BottomMarginEdge;
            if let Some((_, first_height)) = self.flex_items.first().copied() {
                child = child.flex();
                for (width, height) in &self.flex_items {
                    child = child.child(div().w(crate::px(*width)).h(crate::px(*height)));
                }
                baseline = AtomicBaseline::Measured(Box::new(move |_, _, _| {
                    Some(crate::px(first_height))
                }));
            }
            if let Some(text) = self.text {
                let text = StyledText::new(text);
                let layout = text.layout().clone();
                child = child.child(text);
                baseline = AtomicBaseline::Measured(Box::new(move |window, root, _| {
                    let text_top = window
                        .layout_bounds(layout.layout_id().expect("the text requested its layout"))
                        .origin
                        .y;
                    Some(text_top - window.layout_bounds(root).origin.y + layout.last_baseline()?)
                }));
            }
            let mut inline = AtomicInline::new(index, child.into_any_element());
            inline.width = self.width_fraction.map(crate::relative);
            inline.vertical_align = self.align;
            inline.parent = self.parent;
            inline.baseline = baseline;
            inline.offset = crate::point(
                DefiniteLength::calc(crate::px(self.offset.0), self.offset_fraction.0),
                DefiniteLength::calc(crate::px(self.offset.1), self.offset_fraction.1),
            );
            inline.containing_height.set(crate::px(self.containing_height));
            let [top, right, bottom, left] = self.margin;
            inline.margin = Edges {
                top: crate::px(top).into(),
                right: crate::px(right).into(),
                bottom: crate::px(bottom).into(),
                left: crate::px(left).into(),
            };
            inline
        }
    }

    thread_local! {
        /// How many times a box was laid out by its text.
        pub(super) static LAYOUT_PASSES: Cell<usize> = const { Cell::new(0) };
        /// How many times of them the first frame of the window was.
        pub(super) static FIRST_FRAME_PASSES: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) const OBJECT: char = crate::OBJECT_REPLACEMENT_CHARACTER;

    /// An inline box that holds text, for the vertical alignment of spans.
    #[derive(Clone)]
    pub(super) struct Span {
        pub range: Range<usize>,
        pub parent: Option<usize>,
        pub align: VerticalAlign,
        pub line_height: Option<f32>,
        pub font_size: f32,
    }

    #[derive(Clone, Default)]
    pub(super) struct Setup {
        pub width: f32,
        pub text: String,
        pub atomics: Vec<Atomic>,
        /// The length of each run of the text and its own line height, if it has one.
        /// Without runs the text is one.
        pub runs: Vec<(usize, Option<f32>)>,
        pub spans: Vec<Span>,
        pub text_align: Option<crate::TextAlign>,
        /// The paragraph is the only item of a flex row of this width, and has no width of
        /// its own.
        pub flex_row: Option<f32>,
        pub insets: Vec<crate::TextInset>,
    }

    struct Paragraph {
        setup: Setup,
        layout: Rc<RefCell<Option<TextLayout>>>,
    }

    impl Render for Paragraph {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let setup = &self.setup;
            let atomics: Vec<AtomicInline> = setup
                .text
                .match_indices(OBJECT)
                .zip(&setup.atomics)
                .enumerate()
                .map(|(ix, ((index, _), atomic))| {
                    let selector: &'static str = Box::leak(format!("box{ix}").into_boxed_str());
                    atomic.inline(index, selector)
                })
                .collect();
            let boxes: Vec<InlineBox> = setup
                .spans
                .iter()
                .map(|span| InlineBox {
                    range: span.range.clone(),
                    parent: span.parent,
                    align: span.align,
                    line_height: span.line_height.map(crate::px),
                    content: InlineContent::Text {
                        font: crate::font("Menlo"),
                        font_size: crate::px(span.font_size),
                    },
                })
                .collect();
            let mut text = StyledText::new(setup.text.clone())
                .with_insets(setup.insets.clone())
                .with_inline_boxes(boxes)
                .with_atomic_inlines(atomics);
            if !setup.runs.is_empty() {
                text = text.with_runs(
                    setup
                        .runs
                        .iter()
                        .map(|(len, line_height)| TextRun {
                            len: *len,
                            font: crate::font("Menlo"),
                            line_height: line_height.map(crate::px),
                            ..Default::default()
                        })
                        .collect(),
                );
            }
            *self.layout.borrow_mut() = Some(text.layout().clone());
            let mut paragraph = div()
                .debug_selector(|| "paragraph".into())
                .text_size(crate::px(20.))
                .line_height(crate::px(20.))
                .child(text);
            if setup.flex_row.is_none() {
                paragraph = paragraph.w(crate::px(setup.width));
            }
            // CSS's initial `overflow-wrap` and the spaces that hang at the end of a line,
            // which gpui does not default to.
            paragraph.text_style().overflow_wrap = Some(crate::OverflowWrap::Normal);
            paragraph.text_style().hang_trailing_spaces = Some(true);
            paragraph.text_style().text_align = setup.text_align;
            match setup.flex_row {
                Some(width) => div().child(div().flex().w(crate::px(width)).child(paragraph)),
                None => div().child(paragraph),
            }
        }
    }

    /// The bounds of the paragraph and of its boxes, relative to the paragraph.
    pub(super) struct Laid {
        pub paragraph: crate::Bounds<Pixels>,
        pub boxes: Vec<crate::Bounds<Pixels>>,
        pub layout: TextLayout,
    }

    pub(super) fn lay_out(width: f32, text: &str, atomics: Vec<Atomic>) -> Laid {
        lay_out_with(Setup {
            width,
            text: text.to_string(),
            atomics,
            ..Default::default()
        })
    }

    pub(super) fn lay_out_with(setup: Setup) -> Laid {
        let mut app = TestApp::with_text_system(Arc::new(FixedMetricsTextSystem::menlo()));
        let count = setup.atomics.len();
        let layout = Rc::new(RefCell::new(None));
        LAYOUT_PASSES.with(|passes| passes.set(0));
        let mut window = app.open_window({
            let layout = layout.clone();
            move |_, _| Paragraph { setup, layout }
        });
        // Opening the window drew a frame already.
        FIRST_FRAME_PASSES.with(|first| first.set(LAYOUT_PASSES.with(Cell::get)));
        LAYOUT_PASSES.with(|passes| passes.set(0));
        window.draw();
        let laid = window.update(|_, window, _| {
            let bounds: HashMap<String, crate::Bounds<Pixels>> = window
                .rendered_frame
                .debug_bounds
                .iter()
                .map(|(key, value)| (key.to_string(), *value))
                .collect();
            let paragraph = bounds["paragraph"];
            (
                paragraph,
                (0..count)
                    .map(|ix| {
                        let mut bounds = bounds[&format!("box{ix}")];
                        bounds.origin = bounds.origin - paragraph.origin;
                        bounds
                    })
                    .collect::<Vec<_>>(),
            )
        });
        let layout = layout.borrow().clone().expect("the paragraph was rendered");
        Laid {
            paragraph: laid.0,
            boxes: laid.1,
            layout,
        }
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

    /// `<div style="width:150px">` holding `text` and boxes of `width:60px;height:10px`
    /// (the first of case "a" is `<span class=b>` between "xxxx" and "xxxx"): Chrome's
    /// height of the div, and the `[x, y]` of each box.
    #[test]
    fn a_line_breaks_on_either_side_of_a_box_as_chrome_does() {
        let b = || atomic(60., 10.);
        let o = OBJECT;
        let cases: Vec<(&str, String, Vec<Atomic>, f32, Vec<(f32, f32)>)> = vec![
            (
                "text and boxes without spaces",
                format!("xxxx{o}xxxx{o}xxxx"),
                vec![b(), b()],
                60.,
                vec![(48.17, 7.), (48.17, 27.)],
            ),
            (
                "spaces on both sides of a box",
                format!("xx {o} xx {o} xx"),
                vec![b(), b()],
                40.,
                vec![(36.13, 7.), (0., 27.)],
            ),
            (
                "a box wider than the line overflows it alone",
                format!("{o}xx{o}"),
                vec![atomic(200., 10.), b()],
                40.,
                vec![(0., 7.), (24.09, 27.)],
            ),
            (
                "boxes alone fill a line and the next starts a new one",
                format!("{o}{o}{o}{o}"),
                vec![b(), b(), b(), b()],
                40.,
                vec![(0., 7.), (60., 7.), (0., 27.), (60., 27.)],
            ),
            (
                "a word that overflows is followed by a box on a line of its own",
                format!("xxxxxxxxxxxxxxxxx{o}"),
                vec![b()],
                40.,
                vec![(0., 27.)],
            ),
            (
                "a trailing space hangs before a box that wraps",
                format!("xxxxxxxxxxxxx {o}"),
                vec![b()],
                40.,
                vec![(0., 27.)],
            ),
        ];
        for (name, text, boxes, height, origins) in cases {
            let laid = lay_out(150., &text, boxes);
            assert_close(laid.paragraph.size.height, height, name);
            for (ix, (x, y)) in origins.into_iter().enumerate() {
                assert_close(laid.boxes[ix].origin.x, x, &format!("{name}: x of {ix}"));
                assert_close(laid.boxes[ix].origin.y, y, &format!("{name}: y of {ix}"));
            }
        }
    }

    /// `<span style="display:inline-block">` holding text, in `<div style="width:Wpx">`:
    /// Chrome's `[x, y, width, height]` of the box, and of a mark after it when there is
    /// one. The mark is `width:1px;height:0`, so its y is the baseline of the line.
    /// gpui measures a text as wide as the next whole pixel, so a width that comes from
    /// text is Chrome's rounded up (48.17 is 49), and what follows it is that much later.
    #[test]
    fn a_box_with_text_takes_its_shrink_to_fit_width_and_its_last_baseline() {
        let mark = || atomic(1., 0.);
        let o = OBJECT;
        struct Case {
            name: &'static str,
            width: f32,
            text: String,
            boxes: Vec<Atomic>,
            height: f32,
            // [x, y, width, height] of each box, in the order of `boxes`.
            rects: Vec<[f32; 4]>,
        }
        let cases = vec![
            Case {
                name: "wider than the line: wraps to the width of the line",
                width: 150.,
                text: format!("{o}"),
                boxes: vec![text_box("xxxx xxxx xxxx")],
                height: 40.,
                rects: vec![[0., 0., 150., 40.]],
            },
            Case {
                name: "the width of the line, not the rest of it",
                width: 150.,
                text: format!("yy{o}"),
                boxes: vec![text_box("xxxx xxxx xxxx")],
                height: 60.,
                rects: vec![[0., 20., 150., 40.]],
            },
            Case {
                name: "narrower than a word: the width of the word",
                width: 40.,
                text: format!("{o}"),
                boxes: vec![text_box("xxxx xxxx")],
                height: 40.,
                rects: vec![[0., 0., 48.19, 40.]],
            },
            Case {
                name: "padding above the text raises the baseline",
                width: 150.,
                text: format!("xx{o}{o}"),
                boxes: vec![text_box("yy zz").padding(10., 0., 0., 0.), mark()],
                height: 30.,
                rects: vec![[24.09, 0., 60.23, 30.], [84.32, 27., 1., 0.]],
            },
            Case {
                name: "the baseline is the last line of the text",
                width: 150.,
                text: format!("xx{o}{o}"),
                boxes: vec![text_box("xxxx xxxx").width(80.), mark()],
                height: 40.,
                rects: vec![[24.09, 0., 80., 40.], [104.09, 37., 1., 0.]],
            },
            Case {
                name: "a hard line break and padding below",
                width: 150.,
                text: format!("xx{o}{o}"),
                boxes: vec![text_box("xxxx\nyyyy").padding(0., 0., 6., 0.), mark()],
                height: 46.,
                rects: vec![[24.09, 0., 48.19, 46.], [72.28, 37., 1., 0.]],
            },
            Case {
                name: "a box with no text has its bottom edge for a baseline",
                width: 150.,
                text: format!("xx{o}{o}"),
                boxes: vec![atomic(30., 30.), mark()],
                height: 33.,
                rects: vec![[24.09, 0., 30., 30.], [54.09, 30., 1., 0.]],
            },
            // border 1px, padding 1px 2px: the same as padding of 2px 3px.
            Case {
                name: "margin and padding around the text",
                width: 150.,
                text: format!("xx{o}{o}"),
                boxes: vec![
                    text_box("xx yy")
                        .padding(2., 3., 2., 3.)
                        .margin(2., 3., 4., 5.),
                    mark(),
                ],
                height: 30.,
                rects: vec![[29.09, 2., 66.23, 24.], [98.32, 21., 1., 0.]],
            },
        ];
        for case in cases {
            let laid = lay_out(case.width, &case.text, case.boxes);
            assert_close(laid.paragraph.size.height, case.height, case.name);
            for (ix, [x, y, width, height]) in case.rects.into_iter().enumerate() {
                let rect = laid.boxes[ix];
                let name = format!("{} (box {ix})", case.name);
                assert_close(rect.origin.x, x, &format!("{name}: x"));
                assert_close(rect.origin.y, y, &format!("{name}: y"));
                assert_close(rect.size.width, width, &format!("{name}: width"));
                assert_close(rect.size.height, height, &format!("{name}: height"));
            }
        }
    }

    /// `<div style="width:400px">xx<span style="vertical-align:...">yy</span>xx<mark></div>`:
    /// Chrome's height of the div, the baseline of the line (where the mark is) and the
    /// baseline of the span, from the top of the div.
    #[test]
    fn a_span_is_raised_or_lowered_from_the_baseline_as_chrome_does() {
        let offset = |px: f32, fraction: f32| VerticalAlign::Offset {
            length: crate::px(px),
            line_height_fraction: fraction,
        };
        // (name, align, line height of the span, [height, baseline, span baseline])
        let cases: Vec<(&str, VerticalAlign, Option<f32>, [f32; 3])> = vec![
            ("super", VerticalAlign::Super, None, [27.656, 24.656, 17.]),
            ("sub", VerticalAlign::Sub, None, [25., 17., 22.]),
            ("text-top", VerticalAlign::TextTop, None, [22., 19., 17.]),
            (
                "text-bottom",
                VerticalAlign::TextBottom,
                None,
                [22., 17., 19.],
            ),
            ("middle", VerticalAlign::Middle, None, [21.531, 17., 18.531]),
            ("6px", offset(6., 0.), None, [26., 23., 17.]),
            ("top", VerticalAlign::Top, None, [20., 17., 17.]),
            (
                "bottom with a line height of 40px",
                VerticalAlign::Bottom,
                Some(40.),
                [40., 37., 27.],
            ),
            (
                "super with a line height of 40px",
                VerticalAlign::Super,
                Some(40.),
                [40., 34.656, 27.],
            ),
            (
                "50% of a line height of 40px",
                offset(0., 0.5),
                Some(40.),
                [50., 47., 27.],
            ),
        ];
        for (name, align, line_height, [height, baseline, span_baseline]) in cases {
            let text = format!("xxyyxx{OBJECT}");
            let laid = lay_out_with(Setup {
                width: 400.,
                text,
                atomics: vec![atomic(1., 0.)],
                runs: vec![(2, None), (2, line_height), (2, None), (3, None)],
                spans: vec![Span {
                    range: 2..4,
                    parent: None,
                    align,
                    line_height,
                    font_size: 20.,
                }],
                text_align: None,
                flex_row: None,
                insets: Vec::new(),
            });
            assert_close(laid.paragraph.size.height, height, name);
            assert_close(
                laid.boxes[0].origin.y,
                baseline,
                &format!("{name}: baseline"),
            );
            let span = laid
                .layout
                .baseline_for_index(2)
                .expect("the span has glyphs")
                - laid.paragraph.origin.y;
            assert_close(
                span,
                span_baseline,
                &format!("{name}: baseline of the span"),
            );
        }
    }

    /// A span in a span, both `vertical-align:super`: the raises add up.
    #[test]
    fn nested_spans_add_their_baseline_shifts_as_in_chrome() {
        let span = |parent| Span {
            range: 2..4,
            parent,
            align: VerticalAlign::Super,
            line_height: None,
            font_size: 20.,
        };
        let laid = lay_out_with(Setup {
            width: 400.,
            text: format!("xxyyxx{OBJECT}"),
            atomics: vec![atomic(1., 0.)],
            spans: vec![span(None), span(Some(0))],
            ..Default::default()
        });
        assert_close(laid.paragraph.size.height, 35.3125, "height");
        assert_close(laid.boxes[0].origin.y, 32.313, "baseline");
        let inner = laid.layout.baseline_for_index(2).unwrap() - laid.paragraph.origin.y;
        assert_close(inner, 17., "baseline of the inner span");
    }

    /// `<div style="width:150px;text-align:center">xx<box60x10></div>`, and right
    /// aligned with `margin-left:7px`: the box is where the aligned line puts it.
    #[test]
    fn an_aligned_line_puts_a_box_where_its_placeholder_is() {
        let o = OBJECT;
        let laid = lay_out_with(Setup {
            width: 150.,
            text: format!("xx{o}"),
            atomics: vec![atomic(60., 10.)],
            text_align: Some(crate::TextAlign::Center),
            ..Default::default()
        });
        assert_close(laid.boxes[0].origin.x, 57.05, "centered");
        let laid = lay_out_with(Setup {
            width: 150.,
            text: format!("xx{o}"),
            atomics: vec![atomic(60., 10.).margin(0., 0., 0., 7.)],
            text_align: Some(crate::TextAlign::Right),
            ..Default::default()
        });
        assert_close(laid.boxes[0].origin.x, 90., "right aligned");
    }

    /// `<div style="display:flex;width:Wpx"><div>xx<box60x10>xx</div></div>`: the item is
    /// as wide as its content, then as wide as the flex container, and not narrower than
    /// its longest unbreakable part, as Chrome sizes it: [width, height] of the item.
    #[test]
    fn a_flex_item_is_sized_by_the_min_and_max_content_of_its_boxes() {
        let o = OBJECT;
        for (container, width, height, box_origin) in [
            (400., 108.19, 20., (24.09, 7.)),
            (80., 80., 60., (0., 27.)),
            (40., 60., 60., (0., 27.)),
        ] {
            let laid = lay_out_with(Setup {
                text: format!("xx{o}xx"),
                atomics: vec![atomic(60., 10.)],
                flex_row: Some(container),
                ..Default::default()
            });
            let name = format!("in a flex row of {container}px");
            assert_close(laid.paragraph.size.width, width, &format!("{name}: width"));
            assert_close(
                laid.paragraph.size.height,
                height,
                &format!("{name}: height"),
            );
            // The box is laid out for the width that was measured last, which is not
            // the one it is placed for unless it is laid out again.
            assert_close(
                laid.boxes[0].origin.x,
                box_origin.0,
                &format!("{name}: box x"),
            );
            assert_close(
                laid.boxes[0].origin.y,
                box_origin.1,
                &format!("{name}: box y"),
            );
            assert_close(laid.boxes[0].size.width, 60., &format!("{name}: box width"));
        }
    }

    /// The box is a real element: it is painted where its placeholder is, it is hit where
    /// it is painted, and the placeholder paints no glyph.
    #[test]
    fn a_box_is_painted_hit_and_hovered_at_its_placeholder() {
        use crate::{MouseButton, point};
        let clicks = Rc::new(Cell::new(0));
        let mut app = TestApp::with_text_system(Arc::new(FixedMetricsTextSystem::menlo()));
        let mut window = app.open_window({
            let clicks = clicks.clone();
            move |_, _| Paragraph {
                setup: Setup {
                    width: 400.,
                    text: format!("xx{OBJECT}xx"),
                    atomics: vec![Atomic {
                        clicks: Some(clicks),
                        ..atomic(30., 30.)
                    }],
                    ..Default::default()
                },
                layout: Rc::default(),
            }
        });
        window.draw();
        let (bounds, quads, sprites, scale) = window.update(|_, window, _| {
            let scene = &window.rendered_frame.scene;
            (
                window.rendered_frame.debug_bounds["box0"],
                scene
                    .quads
                    .iter()
                    .map(|quad| quad.bounds)
                    .collect::<Vec<_>>(),
                scene.monochrome_sprites.len(),
                window.scale_factor(),
            )
        });
        assert!(
            quads
                .iter()
                .any(|quad| quad.origin.x.0 / scale == bounds.origin.x.0
                    && quad.origin.y.0 / scale == bounds.origin.y.0
                    && quad.size.width.0 / scale == 30.),
            "no quad at {bounds:?} in {quads:?}"
        );
        assert_eq!(
            sprites, 4,
            "one glyph for each of the four characters, none for the box"
        );

        let inside = bounds.center();
        window.simulate_click(inside, MouseButton::Left);
        assert_eq!(clicks.get(), 1);
        window.simulate_click(point(bounds.right() + px(1.), inside.y), MouseButton::Left);
        window.simulate_click(point(inside.x, bounds.bottom() + px(1.)), MouseButton::Left);
        assert_eq!(
            clicks.get(),
            1,
            "a click beside the box is not a click on it"
        );
    }

    struct Nested;

    impl Render for Nested {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let o = OBJECT;
            let inner = AtomicInline::new(
                2,
                div()
                    .debug_selector(|| "inner".into())
                    .w(crate::px(10.))
                    .h(crate::px(10.))
                    .into_any_element(),
            );
            let middle = AtomicInline::new(
                2,
                div()
                    .debug_selector(|| "middle".into())
                    .child(StyledText::new(format!("yy{o}")).with_atomic_inlines(vec![inner]))
                    .into_any_element(),
            );
            div().child(
                div()
                    .debug_selector(|| "paragraph".into())
                    .w(crate::px(400.))
                    .text_size(crate::px(20.))
                    .line_height(crate::px(20.))
                    .child(StyledText::new(format!("xx{o}")).with_atomic_inlines(vec![middle])),
            )
        }
    }

    /// An atomic inline holds a text with an atomic inline of its own, and the engines
    /// of the two do not mix.
    #[test]
    fn atomic_inlines_nest() {
        let mut app = TestApp::with_text_system(Arc::new(FixedMetricsTextSystem::menlo()));
        let mut window = app.open_window(|_, _| Nested);
        // A second frame builds all of it again.
        window.draw();
        window.draw();
        let bounds = window.update(|_, window, _| window.rendered_frame.debug_bounds.clone());
        let [paragraph, middle, inner] = ["paragraph", "middle", "inner"].map(|key| bounds[key]);
        // "xx" is 24.09px, and "yy" before the inner box too.
        assert_close(
            middle.origin.x - paragraph.origin.x,
            24.09,
            "x of the middle box",
        );
        assert_close(middle.size.width, 34.09, "width of the middle box");
        assert_close(
            inner.origin.x - middle.origin.x,
            24.09,
            "x of the inner box",
        );
        assert_close(inner.size.width, 10., "width of the inner box");
        // The middle box has the baseline of its bottom edge, and so has the inner one,
        // which is on the baseline of the middle box's line.
        assert_close(middle.size.height, 20., "height of the middle box");
        assert_close(
            inner.bottom() - middle.origin.y,
            17.,
            "the inner box is on the baseline",
        );
    }

    /// `<span style="display:inline-block;position:relative;left:7px;top:5px;width:30px;height:10px">`:
    /// Chrome paints the box 7px right and 5px down of where it sits, and the text after it is where it
    /// was.
    #[test]
    fn an_offset_moves_the_box_and_nothing_else() {
        let text = format!("xx{OBJECT}yy");
        let plain = lay_out(400., &text, vec![atomic(30., 10.)]);
        let moved = lay_out(400., &text, vec![atomic(30., 10.).offset(7., 5.)]);
        assert_close(moved.boxes[0].origin.x - plain.boxes[0].origin.x, 7., "x");
        assert_close(moved.boxes[0].origin.y - plain.boxes[0].origin.y, 5., "y");
        assert_close(
            moved.paragraph.size.height,
            f32::from(plain.paragraph.size.height),
            "the line",
        );
        assert_close(
            moved.layout.position_for_index(5).unwrap().x,
            f32::from(plain.layout.position_for_index(5).unwrap().x),
            "the text after the box",
        );
    }

    /// `<div style="width:400px"><span style="line-height:40px"><span style="display:inline-block;width:30px;height:10px">`
    /// (and `vertical-align:5px` or `top` on the outer span): Chrome makes the line as tall as the strut of
    /// the span, 19px of ascent and 5px of descent with 8px of half-leading each side, and the box sits on its
    /// baseline, whatever the span holds.
    #[test]
    fn a_span_with_only_a_box_in_it_has_its_strut_in_the_line() {
        let span = |align| Span {
            range: 0..OBJECT.len_utf8(),
            parent: None,
            align,
            line_height: Some(40.),
            font_size: 20.,
        };
        let laid = |align| {
            lay_out_with(Setup {
                width: 400.,
                text: OBJECT.to_string(),
                atomics: vec![atomic(30., 10.).in_span(0)],
                spans: vec![span(align)],
                ..Default::default()
            })
        };
        let offset = |length: f32| VerticalAlign::Offset {
            length: crate::px(length),
            line_height_fraction: 0.,
        };
        // The strut is 27px above the baseline and 13px below.
        let plain = laid(VerticalAlign::Baseline);
        assert_close(plain.paragraph.size.height, 40., "baseline: the line");
        assert_close(plain.boxes[0].origin.y, 17., "baseline: the box");
        let raised = laid(offset(5.));
        assert_close(raised.paragraph.size.height, 40., "raised: the line");
        // The box is in the span, so it is raised with it: 5px up from a baseline 32px down.
        assert_close(raised.boxes[0].origin.y, 17., "raised: the box");
        let lowered = laid(offset(-15.));
        assert_close(lowered.paragraph.size.height, 45., "lowered: the line");
        assert_close(lowered.boxes[0].origin.y, 22., "lowered: the box");
        for align in [VerticalAlign::Top, VerticalAlign::Bottom] {
            let aligned = laid(align);
            assert_close(aligned.paragraph.size.height, 40., &format!("{align:?}: the line"));
            assert_close(aligned.boxes[0].origin.y, 17., &format!("{align:?}: the box"));
        }
    }

    /// `<span style="line-height:40px"><span><span style="line-height:10px"><span ib>`: the strut of every
    /// span around the box is in the line, and the box is on the baseline they share.
    #[test]
    fn every_span_around_a_box_has_its_strut_in_the_line() {
        let len = OBJECT.len_utf8();
        let span = |parent, line_height| Span {
            range: 0..len,
            parent,
            align: VerticalAlign::Baseline,
            line_height: Some(line_height),
            font_size: 20.,
        };
        let laid = lay_out_with(Setup {
            width: 400.,
            text: OBJECT.to_string(),
            atomics: vec![atomic(30., 10.).in_span(1)],
            spans: vec![span(None, 40.), span(Some(0), 10.)],
            ..Default::default()
        });
        assert_close(laid.paragraph.size.height, 40., "the line");
        assert_close(laid.boxes[0].origin.y, 17., "the box");
    }

    /// `<div style="width:140px">xxxxxxxxxx<span style="display:inline-block;width:30px;height:10px"></span></div>`:
    /// the box wraps to the second row, and the byte of its placeholder is a wrap boundary, which
    /// is the end of the row above like any other index there. Text before the box ends at the
    /// end of its own row, not at the start of the box's.
    #[test]
    fn the_placeholder_of_a_box_that_wrapped_is_the_end_of_the_row_above() {
        let text = format!("xxxxxxxxxx{OBJECT}");
        let laid = lay_out(140., &text, vec![atomic(30., 10.)]);
        assert_close(laid.paragraph.size.height, 40., "two rows");
        assert_close(laid.boxes[0].origin.y, 20. + 7., "the box is on the second row");
        let end = laid.layout.position_for_index(10).unwrap();
        let before = laid.layout.position_for_index(9).unwrap();
        assert_eq!(end.y, before.y, "on the row of the text");
        assert!(end.x > before.x, "after the last glyph of the row");
        assert_eq!(laid.layout.row_for_index(10).unwrap().top, end.y);
    }

    /// `<span style="line-height:40px">` around a box that wraps to a line of its own after text outside the
    /// span: that line has the strut of the span.
    #[test]
    fn the_line_a_box_wraps_to_has_the_strut_of_the_span_around_it() {
        let text = format!("xxxxxxxxxx{OBJECT}");
        let laid = lay_out_with(Setup {
            width: 140.,
            text,
            atomics: vec![atomic(30., 10.).in_span(0)],
            spans: vec![Span {
                range: 10..10 + OBJECT.len_utf8(),
                parent: None,
                align: VerticalAlign::Baseline,
                line_height: Some(40.),
                font_size: 20.,
            }],
            ..Default::default()
        });
        assert_close(laid.paragraph.size.height, 60., "the lines");
        assert_close(laid.boxes[0].origin.y, 20. + 17., "the box");
    }

    /// `<div style="width:400px"><span style="display:inline-block;position:relative;left:10%;top:25%;...">`
    /// in a block 80px tall: Chrome moves the box by a tenth of the width and a quarter of the
    /// height of the containing block.
    #[test]
    fn a_fraction_of_the_offset_is_of_the_width_of_the_text_and_the_containing_height() {
        let text = format!("xx{OBJECT}yy");
        let plain = lay_out(400., &text, vec![atomic(30., 10.)]);
        let moved = lay_out(
            400.,
            &text,
            vec![atomic(30., 10.).offset(4., 0.).offset_of(0.1, 0.25, 80.)],
        );
        assert_close(moved.boxes[0].origin.x - plain.boxes[0].origin.x, 44., "x");
        assert_close(moved.boxes[0].origin.y - plain.boxes[0].origin.y, 20., "y");
    }

    /// Each box costs two layouts of its subtree a frame when it fits: its max-content
    /// layout to know the width it wants, and one at that width, which a descendant sized
    /// in percent needs (its percentage is of the width the box came to, not of the
    /// unresolved width of the max-content layout). Its min-content width is only measured
    /// when the line is narrower than that.
    #[test]
    fn a_box_that_fits_is_laid_out_twice_and_one_that_does_not_at_most_three_times() {
        let passes = |width: f32, atomics: usize| {
            lay_out(
                width,
                &format!("xx{}", OBJECT.to_string().repeat(atomics)),
                (0..atomics).map(|_| text_box("xxxx xxxx")).collect(),
            );
            FIRST_FRAME_PASSES.with(Cell::get)
        };
        assert_eq!(passes(400., 1), 2);
        assert_eq!(passes(400., 3), 6);
        // Narrower than the box wants: the max-content layout, then the min-content
        // width to know how narrow it can go, and the layout at the width it got.
        assert_eq!(passes(80., 1), 3);
    }

    /// A hard line break puts a box on the second line, with the bytes of the first
    /// counted, and the padding of a `<span>` around a box is before and after it.
    #[test]
    fn boxes_follow_hard_line_breaks_and_the_insets_around_them() {
        let o = OBJECT;
        // <div style="width:400px">xx<br><span style="padding:0 7px 0 5px"><box60x10></span>yy</div>
        let laid = lay_out_with(Setup {
            width: 400.,
            text: format!("xx\n{o}yy"),
            atomics: vec![atomic(60., 10.)],
            insets: vec![crate::TextInset {
                range: 3..6,
                leading: px(5.),
                trailing: px(7.),
            }],
            ..Default::default()
        });
        assert_close(laid.paragraph.size.height, 40., "two lines");
        assert_close(laid.boxes[0].origin.x, 5., "x after the leading padding");
        assert_close(laid.boxes[0].origin.y, 27., "y on the second line");
        // The "yy" after it starts after the trailing padding too.
        let after = laid.layout.position_for_index(6).unwrap();
        assert_close(after.x, 5. + 60. + 7., "the text after the box");
    }

    /// `<div style="width:400px"><span style="display:inline-block;width:300px;height:10px;max-width:50%">`
    /// and the same with `width:120px`, with `min-width:50%` instead, and with `width:fit-content`
    /// around a 300px or 120px child (fixtures/fit-content-oracle.html of the React tests, `ib` cases): a
    /// percentage limit is of the paragraph, not of the box.
    #[test]
    fn percentage_limits_are_of_the_width_of_the_paragraph() {
        let o = OBJECT;
        let box_width = |atomic: Atomic| {
            lay_out(400., &format!("{o}"), vec![atomic]).boxes[0].size.width
        };
        let limited = |width: f32| Atomic {
            max_width_fraction: Some(0.5),
            ..atomic(width, 10.)
        };
        assert_close(box_width(limited(300.)), 200., "max-width under the width");
        assert_close(box_width(limited(120.)), 120., "max-width over the width");
        let widened = Atomic {
            min_width_fraction: Some(0.5),
            ..atomic(120., 10.)
        };
        assert_close(box_width(widened), 200., "min-width");
        let fit = |content: f32| Atomic {
            width: None,
            fit_content: true,
            max_width_fraction: Some(0.5),
            flex_items: vec![(content, 10.)],
            ..atomic(0., 10.)
        };
        assert_close(box_width(fit(300.)), 200., "fit-content under max-width");
        assert_close(box_width(fit(120.)), 120., "fit-content over max-width");
    }

    /// `<span style="display:inline-block;width:50%;height:10px">` in a 150px paragraph:
    /// a percentage is of the width of the paragraph, whatever is on the line with it.
    #[test]
    fn a_percentage_width_is_of_the_width_of_the_paragraph() {
        let o = OBJECT;
        let laid = lay_out(
            150.,
            &format!("xx{o}{o}"),
            vec![
                Atomic {
                    width: None,
                    width_fraction: Some(0.5),
                    ..atomic(0., 10.)
                },
                Atomic {
                    width: None,
                    width_fraction: Some(1.),
                    ..atomic(0., 10.)
                },
            ],
        );
        assert_close(laid.boxes[0].size.width, 75., "50%");
        assert_close(laid.boxes[1].size.width, 150., "100%");
        // 24px of text and 75px of box fit on the line, and the 150px one is alone.
        assert_close(
            laid.boxes[0].origin.y,
            7.,
            "the first box is on the first line",
        );
        assert_close(laid.boxes[1].origin.x, 0., "the second box starts a line");
        assert_close(
            laid.boxes[1].origin.y,
            27.,
            "the second box is on the second line",
        );
    }

    /// `<span style="display:inline-flex">` of a 30x10 and a 20x15 box: it is as wide as its
    /// items, wherever the line is, and its baseline is the first item's.
    #[test]
    fn a_flex_box_is_as_wide_as_its_items() {
        let o = OBJECT;
        let flex = || Atomic {
            width: None,
            height: None,
            flex_items: vec![(30., 10.), (20., 15.)],
            ..atomic(0., 0.)
        };
        for (width, height, rects) in [
            (400., 22., [[24.09, 7., 50., 15.], [74.09, 17., 1., 0.]]),
            (40., 62., [[0., 27., 50., 15.], [0., 59., 1., 0.]]),
        ] {
            let laid = lay_out(width, &format!("xx{o}{o}"), vec![flex(), atomic(1., 0.)]);
            let name = format!("in {width}px");
            assert_close(laid.paragraph.size.height, height, &format!("{name}: height"));
            for (ix, [x, y, w, h]) in rects.into_iter().enumerate() {
                let rect = laid.boxes[ix];
                assert_close(rect.origin.x, x, &format!("{name}: x of {ix}"));
                assert_close(rect.origin.y, y, &format!("{name}: y of {ix}"));
                assert_close(rect.size.width, w, &format!("{name}: width of {ix}"));
                assert_close(rect.size.height, h, &format!("{name}: height of {ix}"));
            }
        }
    }

    /// What a frame of one window gave: the paragraph, its boxes relative to it, how the text
    /// wrapped, and what laying it out cost.
    #[derive(Debug, PartialEq)]
    struct Frame {
        paragraph: crate::Bounds<Pixels>,
        boxes: Vec<crate::Bounds<Pixels>>,
        wrapped: String,
    }

    /// The window engine's measures and the layouts of boxes that drawing a frame cost.
    struct Cost {
        measures: u64,
        passes: usize,
    }

    /// Draws `setups` one after another in one window, as frames of a page that is edited.
    fn frames(setups: &[Setup]) -> Vec<(Frame, Cost)> {
        let mut app = TestApp::with_text_system(Arc::new(FixedMetricsTextSystem::menlo()));
        let layout = Rc::new(RefCell::new(None));
        let mut window = app.open_window({
            let layout = layout.clone();
            let setup = setups[0].clone();
            move |_, _| Paragraph { setup, layout }
        });
        setups
            .iter()
            .map(|setup| {
                let before = window.update(|_, window, _| window.layout_counters().measures);
                LAYOUT_PASSES.with(|passes| passes.set(0));
                // Updating a test window draws it, once.
                window.update(|view, _, cx| {
                    view.setup = setup.clone();
                    cx.notify();
                });
                let (paragraph, boxes, measures) = window.update(|_, window, _| {
                    let bounds: HashMap<String, crate::Bounds<Pixels>> = window
                        .rendered_frame
                        .debug_bounds
                        .iter()
                        .map(|(key, value)| (key.to_string(), *value))
                        .collect();
                    let paragraph = bounds["paragraph"];
                    let boxes = (0..setup.atomics.len())
                        .map(|ix| {
                            let mut bounds = bounds[&format!("box{ix}")];
                            bounds.origin = bounds.origin - paragraph.origin;
                            bounds
                        })
                        .collect::<Vec<_>>();
                    (paragraph, boxes, window.layout_counters().measures)
                });
                let wrapped = layout.borrow().as_ref().expect("rendered").wrapped_text();
                (
                    Frame {
                        paragraph,
                        boxes,
                        wrapped,
                    },
                    Cost {
                        measures: measures - before,
                        passes: LAYOUT_PASSES.with(Cell::get),
                    },
                )
            })
            .collect()
    }

    fn with_boxes(width: f32, text: &str, atomics: Vec<Atomic>) -> Setup {
        Setup {
            width,
            text: text.to_string(),
            atomics,
            ..Default::default()
        }
    }

    /// A text whose boxes are as they were is not measured again, and lays each box out once, as
    /// it was last measured.
    #[test]
    fn a_text_whose_boxes_did_not_change_is_not_measured_again() {
        let o = OBJECT;
        let setup = || {
            with_boxes(
                150.,
                &format!("xx{o} yy {o}{o}"),
                vec![text_box("xxxx xxxx"), atomic(30., 10.), text_box("yy").padding(1., 2., 3., 4.)],
            )
        };
        let drawn = frames(&[setup(), setup(), setup(), setup()]);
        // The window drew its first frame when it was opened, which measured the text.
        let first = &drawn[0].0;
        for (frame, cost) in &drawn {
            assert_eq!(frame, first);
            assert_eq!(cost.measures, 0, "a frame measured the text again");
            assert_eq!(cost.passes, 3, "each box is laid out again once, where the text left it");
        }
    }

    /// Whatever changes between two frames, the second is the one a new window draws.
    #[test]
    fn a_text_whose_boxes_changed_is_measured_again_and_gives_what_a_new_window_gives() {
        let o = OBJECT;
        let text = format!("xx{o} yy {o}");
        let base = || vec![text_box("xxxx xxxx"), atomic(30., 10.)];
        let edits: Vec<(&str, Setup)> = vec![
            ("base", with_boxes(150., &text, base())),
            ("the text of a box", with_boxes(150., &text, vec![text_box("xxxx xxxx xxxx"), atomic(30., 10.)])),
            ("the size of a box", with_boxes(150., &text, vec![text_box("xxxx xxxx"), atomic(40., 10.)])),
            ("the width of the line", with_boxes(90., &text, base())),
            ("a margin", with_boxes(150., &text, vec![text_box("xxxx xxxx").margin(0., 5., 0., 7.), atomic(30., 10.)])),
            ("a padding", with_boxes(150., &text, vec![text_box("xxxx xxxx").padding(2., 3., 4., 5.), atomic(30., 10.)])),
            ("an alignment", with_boxes(150., &text, vec![text_box("xxxx xxxx").aligned(VerticalAlign::Middle), atomic(30., 10.)])),
            ("an offset", with_boxes(150., &text, vec![text_box("xxxx xxxx").offset(3., 4.), atomic(30., 10.)])),
            ("a fixed width", with_boxes(150., &text, vec![text_box("xxxx xxxx").width(70.), atomic(30., 10.)])),
            ("the text around", with_boxes(150., &format!("xxxx{o} yy yy {o}"), base())),
            ("one box fewer", with_boxes(150., &format!("xx{o} yy"), vec![text_box("xxxx xxxx")])),
            ("one box more", with_boxes(150., &format!("{o}xx{o} yy {o}"), vec![atomic(5., 5.), text_box("xxxx xxxx"), atomic(30., 10.)])),
            ("back to the base", with_boxes(150., &text, base())),
        ];
        let sequence: Vec<Setup> = edits.iter().map(|(_, setup)| setup.clone()).collect();
        // Each edit twice, so the frame after it is the one that keeps what the edit measured.
        let twice: Vec<Setup> = sequence.iter().flat_map(|setup| [setup.clone(), setup.clone()]).collect();
        let drawn = frames(&twice);
        for (ix, (name, setup)) in edits.iter().enumerate() {
            let fresh = &frames(std::slice::from_ref(setup))[0].0;
            let (edited, cost) = &drawn[ix * 2];
            let (kept, kept_cost) = &drawn[ix * 2 + 1];
            assert_eq!(edited, fresh, "{name}: the frame of the edit");
            assert_eq!(kept, fresh, "{name}: the frame after it");
            if ix > 0 {
                assert!(cost.measures > 0, "{name}: the edit was not measured");
            }
            assert_eq!(kept_cost.measures, 0, "{name}: the frame after it measured the text");
        }
    }
}
