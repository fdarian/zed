//! CSS scroll anchoring (`overflow-anchor`), as Blink does it.
//!
//! A scroller that is scrolled away from the start picks an anchor, the first box of its content
//! that is in view. When the layout moves the anchor along the block axis (content inserted or
//! resized above it), the scroller is scrolled by the same distance, so what the user looks at
//! stays where it is.
//!
//! Blink picks the anchor before a layout and compares after it. A pass of gpui is already
//! laid out when the scroller prepaints, so the anchor is picked at the end of the prepaint
//! of the pass before, from the geometry that pass ended with, which is the geometry Blink's
//! pick would read, and compared with the layout of the next one.
//!
//! The scroller keeps the state in its `ScrollHandleState`, so only scrollers that track a
//! handle anchor. Blink: `third_party/blink/renderer/core/layout/scroll_anchor.cc`.

use crate::{
    Bounds, LayoutId, Pixels, Point, Position, Size, Window, point,
    taffy::{AnchorFacts, TaffyLayoutEngine},
};

/// The anchor of a scroller and what is needed to tell whether the layout moved it.
#[derive(Default, Debug)]
pub(crate) struct ScrollAnchoring {
    anchor: Option<Anchor>,
    /// The offset the last step left in the scroller. Another offset at the next prepaint means
    /// that something scrolled it: the wheel, a scrollbar, a call of the handle.
    left_offset: Point<Pixels>,
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    engine: u64,
    node: LayoutId,
    /// Where the block start of the anchor was from the top of the scrollport.
    block_start: Pixels,
}

/// What the step needs of the scroller it runs for.
pub(crate) struct Scroller {
    pub layout_id: LayoutId,
    /// The scrollport: the padding box less the scrollbars.
    pub client: Size<Pixels>,
    /// How far the offset can go, positive.
    pub max_offset_y: Pixels,
    /// `overflow-anchor` is not `none` on the scroller.
    pub enabled: bool,
}

impl ScrollAnchoring {
    /// To be called before the offset is clamped. An offset that is not the one the last step
    /// left was written by someone else, which drops the anchor: Blink does the same for every
    /// scroll that is not its own or the layout's clamp.
    pub(crate) fn note_offset(&mut self, offset: Point<Pixels>, jumped: bool) {
        if self.anchor.is_some() && (jumped || offset != self.left_offset) {
            self.anchor = None;
        }
    }

    /// Moves the offset by what the layout moved the anchor, and picks an anchor if the scroller
    /// has none. To be called when the offset is clamped to the new layout, since Blink clamps
    /// first and compensates after.
    pub(crate) fn step(
        &mut self,
        scroller: &Scroller,
        offset: &mut Point<Pixels>,
        window: &mut Window,
    ) {
        if !scroller.enabled {
            self.anchor = None;
            return;
        }
        let scale_factor = window.scale_factor();
        let engine = window.layout_engine_mut();
        let view = Scrollport::of(engine, scroller, scale_factor);

        if let Some(anchor) = self.anchor {
            match self.compensation(engine, &view, scroller, anchor, *offset) {
                Compensation::Drop => self.anchor = None,
                Compensation::None => {}
                Compensation::Scroll(delta) => {
                    offset.y = (offset.y - delta).clamp(-scroller.max_offset_y, Pixels::ZERO);
                }
            }
        }
        // Blink does not anchor a scroller that is at its start.
        if offset.y == Pixels::ZERO {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Walk::select(engine, &view, scroller, *offset).map(|(node, block_start)| {
                Anchor {
                    engine: engine.id(),
                    node,
                    block_start,
                }
            });
        }
        self.left_offset = *offset;
    }

    fn compensation(
        &self,
        engine: &mut TaffyLayoutEngine,
        view: &Scrollport,
        scroller: &Scroller,
        anchor: Anchor,
        offset: Point<Pixels>,
    ) -> Compensation {
        if anchor.engine != engine.id() || !engine.contains(anchor.node) {
            return Compensation::Drop;
        }
        let Some(path) = Path::to_scroller(engine, anchor.node, scroller.layout_id) else {
            return Compensation::Drop;
        };
        if path.excluded {
            return Compensation::Drop;
        }
        let block_start = view.bounds_of(engine, anchor.node, offset).origin.y;
        // The anchor is DIP-snapped when painted, so Blink compares the rounded positions: a
        // growth from 50.4 to 50.6 is a pixel.
        let delta = round_to_pixel(block_start) - round_to_pixel(anchor.block_start);
        if delta == Pixels::ZERO {
            return Compensation::None;
        }
        // A restyle that moves the anchor itself (its margin, its top) is the author's move, not
        // content shifting under the anchor.
        if path.restyled {
            return Compensation::Drop;
        }
        Compensation::Scroll(delta)
    }
}

enum Compensation {
    Drop,
    None,
    Scroll(Pixels),
}

/// `LayoutUnit::Round`: halves go up.
fn round_to_pixel(value: Pixels) -> Pixels {
    Pixels((value.0 + 0.5).floor())
}

/// The chain of nodes from the anchor up to the scroller, inclusive.
struct Path {
    restyled: bool,
    excluded: bool,
}

impl Path {
    /// `None` when the node is not below the scroller any more.
    fn to_scroller(engine: &TaffyLayoutEngine, node: LayoutId, scroller: LayoutId) -> Option<Self> {
        let mut path = Path {
            restyled: false,
            excluded: false,
        };
        let mut current = node;
        loop {
            let facts = engine.anchor_facts(current);
            path.restyled |= facts.restyled;
            if current == scroller {
                return Some(path);
            }
            path.excluded |= facts.excluded;
            current = engine.layout_parent(current)?;
        }
    }
}

/// Where the scrollport is, to put the boxes of the scroller's content in its coordinates.
struct Scrollport {
    /// The top left of the scrollport in the layout root's coordinates, which the scroll offset
    /// does not move.
    content_origin: Point<Pixels>,
    client: Size<Pixels>,
    scale_factor: f32,
}

impl Scrollport {
    fn of(engine: &mut TaffyLayoutEngine, scroller: &Scroller, scale_factor: f32) -> Self {
        let bounds = engine.layout_unsnapped_bounds(scroller.layout_id, scale_factor);
        let border = engine.layout_insets(scroller.layout_id, scale_factor).border;
        Scrollport {
            content_origin: point(bounds.origin.x + border.left, bounds.origin.y + border.top),
            client: scroller.client,
            scale_factor,
        }
    }

    /// The border box of the node from the top left of the scrollport, with the scroller at
    /// `offset`. The bottom reaches what overflows a box that does not clip it, as Blink's
    /// `RelativeBounds` does, so that a box with its overflow in view is partly visible.
    fn bounds_of(
        &self,
        engine: &mut TaffyLayoutEngine,
        node: LayoutId,
        offset: Point<Pixels>,
    ) -> Bounds<Pixels> {
        let facts = engine.anchor_facts(node);
        self.bounds_with(engine, node, &facts, offset)
    }

    fn bounds_with(
        &self,
        engine: &mut TaffyLayoutEngine,
        node: LayoutId,
        facts: &AnchorFacts,
        offset: Point<Pixels>,
    ) -> Bounds<Pixels> {
        let border_box = engine.layout_unsnapped_bounds(node, self.scale_factor);
        let mut bottom = border_box.bottom();
        if !facts.clips
            && let Some(end) = engine.layout_unsnapped_scrollable_overflow_end(node, self.scale_factor)
        {
            bottom = bottom.max(end.y);
        }
        let shift = self.content_origin - offset;
        Bounds::from_corners(
            border_box.origin - shift,
            point(border_box.right(), bottom) - shift,
        )
    }
}

/// Blink's `FindAnchorRecursive`.
struct Walk<'a> {
    engine: &'a mut TaffyLayoutEngine,
    view: &'a Scrollport,
    scroller: LayoutId,
    offset: Point<Pixels>,
    found: Option<(LayoutId, Pixels)>,
}

enum Examined {
    /// Not a candidate, its children may be.
    Continue,
    /// Neither it nor its subtree is.
    Skip,
    /// In view whole: the anchor, with nothing better below.
    Whole(Pixels),
    /// Partly in view: the anchor unless a box below is better.
    Partial(Pixels),
}

impl<'a> Walk<'a> {
    /// The node and the block start of the box that anchors the scroller.
    fn select(
        engine: &'a mut TaffyLayoutEngine,
        view: &'a Scrollport,
        scroller: &Scroller,
        offset: Point<Pixels>,
    ) -> Option<(LayoutId, Pixels)> {
        let mut walk = Walk {
            engine,
            view,
            scroller: scroller.layout_id,
            offset,
            found: None,
        };
        let cb_inside = !walk.engine.anchor_facts(scroller.layout_id).is_static;
        walk.visit(scroller.layout_id, cb_inside);
        walk.found
    }

    /// Whether the walk is over.
    fn visit(&mut self, node: LayoutId, containing_block_inside: bool) -> bool {
        let facts = self.engine.anchor_facts(node);
        let examined = if node == self.scroller {
            Examined::Continue
        } else {
            self.examine(node, &facts, containing_block_inside)
        };
        match examined {
            Examined::Skip => return false,
            Examined::Whole(block_start) => {
                self.found = Some((node, block_start));
                return true;
            }
            Examined::Partial(block_start) => {
                self.found = Some((node, block_start));
                // Blink walks into a nested scroller, which is a deviation: a box that scrolls
                // is anchored as a whole.
                if facts.scrolls {
                    return true;
                }
            }
            Examined::Continue => {}
        }
        let containing_block_inside = containing_block_inside || !facts.is_static;
        for index in 0..self.engine.layout_child_count(node) {
            let child = self.engine.layout_child_at(node, index);
            if self.visit(child, containing_block_inside) {
                return true;
            }
        }
        matches!(examined, Examined::Partial(_))
    }

    fn examine(
        &mut self,
        node: LayoutId,
        facts: &AnchorFacts,
        containing_block_inside: bool,
    ) -> Examined {
        if facts.excluded {
            return Examined::Skip;
        }
        // Blink's `CandidateMayMoveWithScroller`.
        match facts.position {
            Position::Fixed => return Examined::Skip,
            Position::Sticky if facts.has_inset => return Examined::Skip,
            Position::AbsoluteInAncestor if !containing_block_inside => return Examined::Skip,
            _ => {}
        }
        let bounds = self.view.bounds_with(self.engine, node, facts, self.offset);
        if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
            return Examined::Skip;
        }
        let client = self.view.client;
        let intersects = bounds.right() > Pixels::ZERO
            && bounds.left() < client.width
            && bounds.bottom() > Pixels::ZERO
            && bounds.top() < client.height;
        if !intersects {
            return Examined::Skip;
        }
        let contained = bounds.left() >= Pixels::ZERO
            && bounds.right() <= client.width
            && bounds.top() >= Pixels::ZERO
            && bounds.bottom() <= client.height;
        if contained {
            Examined::Whole(bounds.top())
        } else {
            Examined::Partial(bounds.top())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AnyElement, AnyWindowHandle, AppContext, Context, InteractiveElement, IntoElement,
        OverflowAnchor, ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled,
        TestAppContext, Window, div, point, px, size,
    };
    use std::{cell::RefCell, rc::Rc};

    #[derive(Clone)]
    struct Row {
        key: usize,
        height: f32,
        margin_top: f32,
        anchor_none: bool,
    }

    struct Model {
        rows: Vec<Row>,
        scroller_anchor_none: bool,
    }

    struct Scene {
        model: Rc<RefCell<Model>>,
        handle: ScrollHandle,
    }

    impl Render for Scene {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let model = self.model.borrow();
            let rows = model
                .rows
                .iter()
                .map(|row| {
                    let element = div()
                        .id(("row", row.key))
                        .w(px(100.))
                        .h(px(row.height))
                        .mt(px(row.margin_top));
                    if row.anchor_none {
                        element.overflow_anchor(OverflowAnchor::None)
                    } else {
                        element
                    }
                    .into_any_element()
                })
                .collect::<Vec<AnyElement>>();
            div().size_full().child(
                div()
                    .id("scroller")
                    .block()
                    .w(px(200.))
                    .h(px(100.))
                    .overflow_y_scroll()
                    .overflow_anchor(if model.scroller_anchor_none {
                        OverflowAnchor::None
                    } else {
                        OverflowAnchor::Auto
                    })
                    .track_scroll(&self.handle)
                    .children(rows),
            )
        }
    }

    struct Harness {
        window: AnyWindowHandle,
        model: Rc<RefCell<Model>>,
        handle: ScrollHandle,
    }

    /// Twenty rows of 30px under a 100px scroller, keys 0 to 19.
    fn harness(cx: &mut TestAppContext) -> Harness {
        let model = Rc::new(RefCell::new(Model {
            rows: (0..20)
                .map(|key| Row {
                    key,
                    height: 30.,
                    margin_top: 0.,
                    anchor_none: false,
                })
                .collect(),
            scroller_anchor_none: false,
        }));
        let handle = ScrollHandle::new();
        let window = cx.open_window(size(px(400.), px(300.)), {
            let (model, handle) = (model.clone(), handle.clone());
            move |_, _| Scene { model, handle }
        });
        let window = AnyWindowHandle::from(window);
        let harness = Harness {
            window,
            model,
            handle,
        };
        harness.draw(cx);
        harness
    }

    impl Harness {
        fn draw(&self, cx: &mut TestAppContext) {
            cx.update_window(self.window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }

        /// A pass that lays out and prepaints but presents nothing.
        fn layout_only(&self, cx: &mut TestAppContext) {
            let ran = cx
                .update_window(self.window, |_, window, cx| {
                    window.refresh();
                    window.draw_if_dirty(cx)
                })
                .unwrap();
            assert!(ran, "the pass ran");
        }

        /// Scrolls to `top` and draws, which picks the anchor.
        fn scroll_to(&self, top: f32, cx: &mut TestAppContext) {
            self.handle.set_offset(point(px(0.), px(-top)));
            self.draw(cx);
            assert_eq!(self.top(), top);
        }

        fn top(&self) -> f32 {
            -self.handle.offset().y.0
        }

        fn row(&self, key: usize) -> std::cell::RefMut<'_, Row> {
            std::cell::RefMut::map(self.model.borrow_mut(), |model| {
                model.rows.iter_mut().find(|row| row.key == key).unwrap()
            })
        }

        fn insert_above(&self, key: usize, height: f32) {
            self.model.borrow_mut().rows.insert(
                0,
                Row {
                    key,
                    height,
                    margin_top: 0.,
                    anchor_none: false,
                },
            );
        }
    }

    #[gpui::test]
    fn content_inserted_above_the_view_moves_the_offset_by_its_height(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.insert_above(100, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 350.);
    }

    #[gpui::test]
    fn an_element_that_grows_above_the_view_moves_the_offset(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(2).height = 70.;
        scene.draw(cx);
        assert_eq!(scene.top(), 340.);
    }

    #[gpui::test]
    fn an_element_that_shrinks_above_the_view_moves_the_offset_back(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(2).height = 10.;
        scene.draw(cx);
        assert_eq!(scene.top(), 280.);
    }

    #[gpui::test]
    fn a_change_below_the_anchor_moves_nothing(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(15).height = 90.;
        scene.draw(cx);
        assert_eq!(scene.top(), 300.);
    }

    #[gpui::test]
    fn a_scroller_at_its_start_is_not_anchored(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.insert_above(100, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 0.);
        // And it has no anchor to compensate once it is scrolled by a hair either.
        scene.scroll_to(0., cx);
        scene.insert_above(101, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 0.);
    }

    #[gpui::test]
    fn a_scroller_with_overflow_anchor_none_does_not_anchor(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.model.borrow_mut().scroller_anchor_none = true;
        scene.scroll_to(300., cx);
        scene.insert_above(100, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 300.);
    }

    #[gpui::test]
    fn overflow_anchor_none_on_the_first_visible_child_passes_the_anchor_to_the_next(
        cx: &mut TestAppContext,
    ) {
        // Row 10 is the first row in view. Growing it moves row 11 and not itself.
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(10).height = 80.;
        scene.draw(cx);
        assert_eq!(scene.top(), 300., "row 10 is the anchor and did not move");

        let scene = harness(cx);
        scene.row(10).anchor_none = true;
        scene.scroll_to(300., cx);
        scene.row(10).height = 80.;
        scene.draw(cx);
        assert_eq!(scene.top(), 350., "row 11 is the anchor and moved by 50");
    }

    #[gpui::test]
    fn a_box_partly_in_view_is_the_anchor(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(310., cx);
        scene.row(10).height = 80.;
        scene.draw(cx);
        assert_eq!(scene.top(), 310., "row 10 is in view by 20px and is the anchor");
    }

    #[gpui::test]
    fn a_restyle_that_moves_the_anchor_is_not_compensated(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(10).margin_top = 20.;
        scene.draw(cx);
        assert_eq!(scene.top(), 300., "the margin of the anchor is the author's move");

        scene.insert_above(100, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 350., "a new anchor was picked and follows");
    }

    #[gpui::test]
    fn a_restyle_that_moves_nothing_keeps_the_anchor(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(10).margin_top = 0.;
        scene.row(3).height = 31.;
        scene.draw(cx);
        assert_eq!(scene.top(), 301.);
    }

    #[gpui::test]
    fn an_explicit_scroll_between_frames_drops_the_anchor(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.handle.set_offset(point(px(0.), px(-200.)));
        scene.insert_above(100, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 200.);

        scene.insert_above(101, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 250., "the scroll picked a new anchor");
    }

    #[gpui::test]
    fn the_compensation_stops_at_the_end_of_the_scroll_range(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(400., cx);
        scene.insert_above(100, 200.);
        scene.model.borrow_mut().rows.truncate(17);
        scene.draw(cx);
        // The content is 200 + 16 * 30 = 680 high: the range ends at 580, short of the 600 the
        // anchor moved by.
        assert_eq!(scene.top(), 580.);
    }

    #[gpui::test]
    fn a_removed_anchor_compensates_nothing(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.model.borrow_mut().rows.retain(|row| row.key != 10);
        scene.insert_above(100, 50.);
        scene.draw(cx);
        assert_eq!(scene.top(), 300.);
    }

    #[gpui::test]
    fn a_layout_only_pass_compensates_and_the_draw_after_it_does_not_again(
        cx: &mut TestAppContext,
    ) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.insert_above(100, 50.);
        scene.layout_only(cx);
        assert_eq!(scene.top(), 350., "a forced layout reads the compensated offset");
        scene.draw(cx);
        assert_eq!(scene.top(), 350.);
    }

    #[gpui::test]
    fn the_compensation_is_whole_pixels(cx: &mut TestAppContext) {
        let scene = harness(cx);
        scene.scroll_to(300., cx);
        scene.row(2).height = 30.4;
        scene.draw(cx);
        assert_eq!(scene.top(), 300., "0.4px rounds away");
        scene.row(2).height = 30.6;
        scene.draw(cx);
        assert_eq!(scene.top(), 301., "0.6px is a pixel");
    }
}
