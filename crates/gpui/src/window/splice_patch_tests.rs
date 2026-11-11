//! Patches that splice (see [`super::patch_splice`]): a root that lays out again and registers other
//! things than it did (more or fewer hitboxes, listeners, dispatch nodes, tab stops, operations)
//! replaces its part of the rendered frame, and the frame is the one built whole: what it paints, what
//! the pointer hits, which listener a click reaches, the tab order, where a key goes and the cursor. The
//! roots recorded after it, the nested ones it held and a scroll translate are right after it.

use crate::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, ElementId, FocusHandle, InteractiveElement, IntoElement,
    ListAlignment, ListState, Modifiers, OverflowAnchor, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement, PatchCounters, PatchDecline,
    PlatformInput, Point, Render, ScrollDelta, ScrollHandle, ScrollPatchCause, ScrollWheelEvent, StatefulInteractiveElement, Styled, TestAppContext, Window, blue, div,
    green, list, point, px, red, retained, size,
};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

const UNIT: &str = "unit";
const SIBLING: &str = "sibling";
const NESTED: &str = "nested";

/// Where the pointer rests, over the second box of the unit.
const POINTER: (f32, f32) = (10., 38.);

struct Tip;

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(20.)).h(px(10.)).child("tip")
    }
}

struct State {
    kids: Cell<usize>,
    /// The key of the unit, which moves when what it holds does.
    version: Cell<u64>,
    /// The key of the unit stays what it was, so that a frame replays the recording it left.
    key_stays: Cell<bool>,
    kid_height: Cell<f32>,
    /// What the first box is taller than the others, which moves the boxes after it and none of their own sizes.
    first_extra: Cell<f32>,
    kid_green: Cell<bool>,
    sibling_green: Cell<bool>,
    /// The unit has a key context and a key listener of its own, so that it always has a dispatch node.
    key_context: Cell<bool>,
    /// The first two boxes are a retained root of their own in a frame built whole.
    nested: Cell<bool>,
    /// The patcher builds the unit with no retained root inside.
    flat: Cell<bool>,
    patch_unit: Cell<bool>,
    patch_sibling: Cell<bool>,
    patch_nested: Cell<bool>,
    focusable: [FocusHandle; 2],
    log: RefCell<Vec<String>>,
    seen: RefCell<Vec<Result<(), PatchDecline>>>,
    /// A list of rows after the unit, for the test of the translate.
    list: Option<ListState>,
    /// The list is before the header instead of after the footer.
    list_first: Cell<bool>,
    /// The unit paints nothing and registers no hitbox, listener or dispatch node while it holds no box.
    bare: Cell<bool>,
    /// The unit is recorded to be put elsewhere, as the row of a scroller is.
    relocatable: Cell<bool>,
    /// The unit is inside a scroller, which `scroll` moves: a frame after it replays the unit `delta` away from where it was recorded.
    scrolled: Cell<bool>,
    scroll: ScrollHandle,
    /// The scroller keeps the CSS default `overflow-anchor: auto`, where the other tests of it turn anchoring off.
    anchoring: Cell<bool>,
    /// The third box has a tooltip, which a recording cannot be put elsewhere with.
    tooltip: Cell<bool>,
}

impl State {
    fn new(cx: &mut TestAppContext, kids: usize, list: bool) -> Rc<Self> {
        let focusable = cx.update(|cx| [cx.focus_handle(), cx.focus_handle()]);
        Rc::new(Self {
            kids: Cell::new(kids),
            version: Cell::new(0),
            key_stays: Cell::new(false),
            kid_height: Cell::new(12.),
            first_extra: Cell::new(0.),
            kid_green: Cell::new(false),
            sibling_green: Cell::new(false),
            key_context: Cell::new(true),
            nested: Cell::new(false),
            flat: Cell::new(false),
            patch_unit: Cell::new(true),
            patch_sibling: Cell::new(false),
            patch_nested: Cell::new(false),
            focusable,
            log: RefCell::new(Vec::new()),
            seen: RefCell::new(Vec::new()),
            list: list.then(|| ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(30.))),
            list_first: Cell::new(false),
            bare: Cell::new(false),
            relocatable: Cell::new(false),
            scrolled: Cell::new(false),
            scroll: ScrollHandle::new(),
            anchoring: Cell::new(false),
            tooltip: Cell::new(true),
        })
    }

    fn change(&self, kids: usize) {
        self.kids.set(kids);
        self.version.set(self.version.get() + 1);
    }

    fn kid(self: &Rc<Self>, index: usize) -> AnyElement {
        let down = self.clone();
        let mut kid = div()
            .id(("kid", index))
            .h(px(self.kid_height.get() + if index == 0 { self.first_extra.get() } else { 0. }))
            .flex_none()
            .bg(if self.kid_green.get() { green() } else { red() })
            .hover(|style| style.bg(green()))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, _, _| down.log.borrow_mut().push(format!("down {index}")));
        if index == 2 && self.tooltip.get() {
            kid = kid.tooltip(|_, cx| cx.new(|_| Tip).into());
        }
        if let Some(handle) = [1, 4].iter().position(|at| *at == index).map(|slot| &self.focusable[slot]) {
            let keys = self.clone();
            kid = kid
                .track_focus(handle)
                .tab_stop(true)
                .on_key_down(move |event, _, _| keys.log.borrow_mut().push(format!("kid {index} key {}", event.keystroke.key)));
        }
        kid.into_any_element()
    }

    fn group(self: &Rc<Self>, kids: Range<usize>) -> AnyElement {
        let mut group = div().id("group").flex_none();
        for index in kids {
            group = group.child(self.kid(index));
        }
        group.into_any_element()
    }

    fn unit(self: &Rc<Self>) -> AnyElement {
        let mut unit = div().id(UNIT).w(px(120.)).h(px(100.)).flex_none().overflow_hidden();
        if !self.bare.get() {
            unit = unit.bg(blue());
        }
        if self.key_context.get() && !self.bare.get() {
            let keys = self.clone();
            unit = unit
                .key_context("Unit")
                .on_key_down(move |event, _, _| keys.log.borrow_mut().push(format!("unit key {}", event.keystroke.key)));
        }
        let count = self.kids.get();
        let grouped = if self.nested.get() && count >= 2 { 2 } else { 0 };
        if grouped > 0 {
            let state = self.clone();
            unit = if self.flat.get() {
                unit.child(self.group(0..grouped))
            } else {
                let key = 500 + self.version.get();
                unit.child(retained(NESTED, key, move |_, _| state.group(0..grouped)).patchable())
            };
        }
        for index in grouped..count {
            unit = unit.child(self.kid(index));
        }
        unit.into_any_element()
    }

    fn sibling(&self) -> AnyElement {
        let color = if self.sibling_green.get() { green() } else { red() };
        div().id("sib").w(px(60.)).h(px(20.)).flex_none().bg(color).into_any_element()
    }
}

struct Page(Rc<State>);

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let state = self.0.clone();
        let build = state.clone();
        let key = if state.key_stays.get() { 1_000 } else { 1_000 + state.version.get() };
        let mut unit = retained(UNIT, key, move |_, _| build.unit()).patchable_relayout();
        if state.relocatable.get() {
            unit = unit.relocatable(px(100.));
        }
        let unit = if state.scrolled.get() {
            let mut scroller = div()
                .id("scroller")
                .w(px(200.))
                .h(px(80.))
                .flex_none()
                .overflow_y_scroll()
                .track_scroll(&state.scroll);
            if !state.anchoring.get() {
                scroller = scroller.overflow_anchor(OverflowAnchor::None);
            }
            scroller
                .child(unit)
                .child(div().h(px(if state.anchoring.get() { 200. } else { 60. })).flex_none())
                .into_any_element()
        } else {
            unit.into_any_element()
        };
        let sibling_state = state.clone();
        let sibling = retained(SIBLING, 2_000, move |_, _| sibling_state.sibling()).patchable();
        let rows = state.list.as_ref().map(|rows| {
            list(rows.clone(), move |index, _, _| {
                retained(("row", index), 3_000, move |_, _| {
                    div().id(("row", index)).w_full().h(px(30.)).bg(blue()).always_hitbox().into_any_element()
                })
                .relocatable(px(100.))
                .into_any_element()
            })
            .w(px(100.))
            .h(px(100.))
        });
        let (first, last) = if state.list_first.get() { (rows, None) } else { (None, rows) };
        let mut page = div().id("page").key_context("Page").w(px(400.)).h(px(400.)).flex().flex_col();
        if let Some(rows) = first {
            page = page.child(rows);
        }
        let mut page = page
            .child(div().id("header").w(px(60.)).h(px(20.)).flex_none().bg(green()))
            .child(unit)
            .child(sibling)
            .child(div().id("footer").w(px(60.)).h(px(20.)).flex_none().bg(green()));
        if let Some(rows) = last {
            page = page.child(rows);
        }
        page.into_any_element()
    }
}

struct Fixture {
    window: AnyWindowHandle,
    state: Rc<State>,
}

/// What the rendered frame is and answers.
#[derive(PartialEq, Debug)]
struct Observed {
    frame: String,
    probes: Vec<String>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, state: Rc<State>) -> Self {
        let window = cx.open_window(size(px(400.), px(400.)), {
            let state = state.clone();
            move |_, _| Page(state)
        });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_contained_patches(true);
            window.set_scroll_patch(true);
            window.set_scroll_patch_gate(Some(Rc::new(|_, _| None)));
            let patched = state.clone();
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                let mut units = Vec::new();
                if patched.patch_unit.get() {
                    patched.flat.set(true);
                    units.push((ElementId::from(UNIT), patched.unit()));
                    patched.flat.set(false);
                }
                if patched.patch_sibling.get() {
                    units.push((ElementId::from(SIBLING), patched.sibling()));
                }
                if patched.patch_nested.get() {
                    units.push((ElementId::from(NESTED), patched.group(0..2)));
                }
                if units.is_empty() {
                    return false;
                }
                let outcome = window.repaint_in_place(units, cx, |_, _| true);
                patched.seen.borrow_mut().push(outcome.as_ref().map(|_| ()).map_err(|decline| *decline));
                outcome.is_ok()
            })));
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(point(px(POINTER.0), px(POINTER.1)), cx);
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            window.present_cached_frame();
        })
        .unwrap();
        Self { window, state }
    }

    /// Draws the frame for the state the test left, which patches if it can.
    fn tick(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.request_tick();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    fn draw_whole(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    /// A frame that is not asked to be built from nothing: the roots that did not change are put
    /// from their recordings.
    fn draw_plain(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    }

    fn patches(&self, cx: &mut TestAppContext) -> PatchCounters {
        cx.update_window(self.window, |_, window, _| window.patch_counters()).unwrap()
    }

    fn replays(&self, cx: &mut TestAppContext) -> u64 {
        cx.update_window(self.window, |_, window, _| window.retained_counters().replays).unwrap()
    }

    fn click(window: &mut Window, at: Point<crate::Pixels>, cx: &mut App) {
        let down = MouseDownEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1, first_mouse: false };
        window.dispatch_event(PlatformInput::MouseDown(down), cx);
        let up = MouseUpEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1 };
        window.dispatch_event(PlatformInput::MouseUp(up), cx);
    }

    /// What the frame rendered paints, holds and answers, all in one update: nothing is drawn between.
    fn observe(&self, cx: &mut TestAppContext) -> Observed {
        let state = self.state.clone();
        cx.update_window(self.window, |_, window, cx| {
            let text = frame_text(window);

            let mut probes = Vec::new();
            // What is at the points, which listener a click there reaches, and the cursor.
            let mut points: Vec<Point<crate::Pixels>> = (0..8).map(|kid| point(px(10.), px(26. + 12. * kid as f32))).collect();
            points.extend([point(px(10.), px(5.)), point(px(10.), px(130.)), point(px(10.), px(150.)), point(px(200.), px(60.))]);
            for at in points {
                let hits: Vec<String> = window.rendered_frame.hit_test(at, true).ids.iter().map(|id| name_of(window, *id)).collect();
                window.simulate_mouse_move(at, cx);
                let cursor = format!("{:?}", window.pointer_cursor_style());
                state.log.borrow_mut().clear();
                Self::click(window, at, cx);
                window.blur();
                probes.push(format!("{at:?}: hits {hits:?} cursor {cursor} click {:?}", state.log.borrow()));
            }
            // The tab order, walked from the start and from the end.
            let tabs = &window.rendered_frame.tab_stops;
            let label = |handle: &FocusHandle| state.focusable.iter().position(|focusable| focusable.id == handle.id);
            let mut forward = Vec::new();
            let mut at = tabs.next(None);
            for _ in 0..4 {
                let Some(handle) = at else { break };
                forward.push(label(&handle));
                at = tabs.next(Some(&handle.id));
            }
            let mut backward = Vec::new();
            let mut at = tabs.prev(None);
            for _ in 0..4 {
                let Some(handle) = at else { break };
                backward.push(label(&handle));
                at = tabs.prev(Some(&handle.id));
            }
            probes.push(format!("tab order {forward:?} back {backward:?}"));
            // A key goes to the focused box, and up through the unit and the page.
            for (slot, handle) in state.focusable.iter().enumerate() {
                state.log.borrow_mut().clear();
                window.focus(handle, cx);
                let reached = window.dispatch_keystroke(crate::Keystroke::parse("a").unwrap(), cx);
                probes.push(format!("key to {slot}: {reached} {:?}", state.log.borrow()));
                window.blur();
            }
            window.simulate_mouse_move(point(px(POINTER.0), px(POINTER.1)), cx);
            Observed { frame: text, probes }
        })
        .unwrap()
    }

    /// What the rendered frame is and what its hitboxes and tab stops answer, with no event sent to the
    /// window: nothing is drawn, so the next tick patches from the recording the last one left.
    fn passive(&self, cx: &mut TestAppContext) -> Observed {
        let state = self.state.clone();
        cx.update_window(self.window, |_, window, _| {
            let text = frame_text(window);
            let mut probes = Vec::new();
            let mut points: Vec<Point<crate::Pixels>> = (0..8).map(|kid| point(px(10.), px(26. + 12. * kid as f32))).collect();
            points.extend([point(px(10.), px(5.)), point(px(10.), px(130.)), point(px(10.), px(150.)), point(px(200.), px(60.))]);
            for at in points {
                let hits: Vec<String> = window.rendered_frame.hit_test(at, true).ids.iter().map(|id| name_of(window, *id)).collect();
                probes.push(format!("{at:?}: hits {hits:?}"));
            }
            let tabs = &window.rendered_frame.tab_stops;
            let label = |handle: &FocusHandle| state.focusable.iter().position(|focusable| focusable.id == handle.id);
            let mut forward = Vec::new();
            let mut at = tabs.next(None);
            for _ in 0..4 {
                let Some(handle) = at else { break };
                forward.push(label(&handle));
                at = tabs.next(Some(&handle.id));
            }
            probes.push(format!("tab order {forward:?}"));
            Observed { frame: text, probes }
        })
        .unwrap()
    }

    /// Asserts that what the last tick patched is what a frame built whole shows, in the frame and in what
    /// it answers.
    #[track_caller]
    fn assert_whole(&self, cx: &mut TestAppContext, what: &str) {
        let patched = self.observe(cx);
        self.draw_whole(cx);
        let whole = self.observe(cx);
        assert_same(&patched.frame, &whole.frame, what);
        assert_eq!(patched.probes, whole.probes, "{what}: what the frame answers");
    }

    /// Settles the page with frames built whole, so that a later tick patches from recordings.
    fn settle(&self, cx: &mut TestAppContext) {
        self.draw_whole(cx);
        self.draw_plain(cx);
        self.draw_plain(cx);
    }
}

/// What the rendered frame is, as a frame being built is described.
fn frame_text(window: &mut Window) -> String {
    // The frame, as a frame being built is described.
    std::mem::swap(&mut window.rendered_frame, &mut window.next_frame);
    let described = window.describe_frame(false);
    let primitives = window.next_frame.scene.describe_primitives();
    std::mem::swap(&mut window.rendered_frame, &mut window.next_frame);
    let frame = &window.rendered_frame;
    let name = |id: crate::HitboxId| {
        frame
            .hitboxes
            .iter()
            .find(|hitbox| hitbox.id == id)
            .map_or_else(|| "(none)".to_owned(), |hitbox| format!("{:?}", hitbox.identity))
    };
    let mut bounds: Vec<_> = frame.debug_bounds.iter().map(|(name, bounds)| format!("{name} {bounds:?}")).collect();
    bounds.sort();
    let cursors: Vec<_> = frame
        .cursor_styles
        .iter()
        .map(|request| (request.hitbox_id.map(name), request.style))
        .collect();
    format!(
        "{described}\n{primitives}\nbounds {bounds:?}\ncursors {cursors:?}\ntooltips {}\nhitbox count {}",
        frame.tooltip_requests.len(),
        frame.hitboxes.len()
    )
}

fn name_of(window: &Window, id: crate::HitboxId) -> String {
    window
        .rendered_frame
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.id == id)
        .map_or_else(|| "(none)".to_owned(), |hitbox| format!("{:?}", hitbox.identity.as_ref().and_then(|identity| identity.ids.last())))
}

/// `frame` without what two windows do not share: what names a hitbox or a tab stop carries the
/// window's view and its handles, and the hashes of element ids hold them.
fn same_window(frame: &str) -> String {
    let mut text = frame.to_owned();
    for prefix in ["EntityId(", "FocusId("] {
        let mut stripped = String::new();
        let mut rest = text.as_str();
        while let Some(at) = rest.find(prefix) {
            let (before, after) = rest.split_at(at);
            stripped.push_str(before);
            stripped.push_str(prefix);
            stripped.push('_');
            rest = &after[after.find(')').unwrap()..];
        }
        stripped.push_str(rest);
        text = stripped;
    }
    let mut hashed = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("hash: ") {
        let (before, after) = rest.split_at(at + "hash: ".len());
        hashed.push_str(before);
        hashed.push('_');
        rest = after.trim_start_matches(|digit: char| digit.is_ascii_digit());
    }
    hashed.push_str(rest);
    hashed
}

#[track_caller]
fn assert_same(patched: &str, whole: &str, what: &str) {
    if patched == whole {
        return;
    }
    let line = patched.lines().zip(whole.lines()).position(|(patched, whole)| patched != whole).unwrap_or(0);
    let around = |text: &str| text.lines().skip(line.saturating_sub(10)).take(22).collect::<Vec<_>>().join("\n");
    panic!("{what}: differ at line {line}\npatched:\n{}\nwhole:\n{}", around(patched), around(whole));
}

#[gpui::test]
fn the_interior_of_a_root_goes_from_3_boxes_to_7_and_to_1_and_the_frame_is_the_one_built_whole(cx: &mut TestAppContext) {
    let state = State::new(cx, 3, false);
    let page = Fixture::new(cx, state);
    let mut relayouts = page.patches(cx).relayout_patches;
    for (kids, what) in [(7, "3 to 7"), (1, "7 to 1"), (5, "1 to 5")] {
        page.state.change(kids);
        page.tick(cx);
        let after = page.patches(cx);
        assert_eq!(after.relayout_patches, relayouts + 1, "{what}: {:?}", page.state.seen.borrow());
        assert_eq!(after.total_declines(), 0, "{what}: {:?}", page.state.seen.borrow());
        relayouts = after.relayout_patches;
        page.assert_whole(cx, what);
        page.settle(cx);
    }
}

#[gpui::test]
fn a_root_with_no_dispatch_node_that_gets_some_hangs_them_off_the_node_it_was_in(cx: &mut TestAppContext) {
    let state = State::new(cx, 1, false);
    state.key_context.set(false);
    let page = Fixture::new(cx, state);
    // One box has no focus, key listener or context: the unit has no dispatch node.
    page.state.change(7);
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "dispatch nodes where there were none");
    page.settle(cx);
    page.state.change(1);
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 2, "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "no dispatch nodes where there were some");
}

#[gpui::test]
fn roots_recorded_after_the_spliced_one_are_patched_and_replayed_from_where_they_are_now(cx: &mut TestAppContext) {
    let state = State::new(cx, 3, false);
    let page = Fixture::new(cx, state);
    page.state.change(7);
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());

    // The sibling is after the unit in every vector the splice moved: a tick that patches only it.
    let before = page.patches(cx);
    page.state.patch_unit.set(false);
    page.state.patch_sibling.set(true);
    page.state.sibling_green.set(true);
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.patched_frames - before.patched_frames, 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.total_declines(), before.total_declines(), "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "a sibling patched after a splice");

    // A frame built whole replays the sibling from its recording, and builds the unit.
    page.state.patch_sibling.set(false);
    page.state.patch_unit.set(true);
    let replays = page.replays(cx);
    page.state.change(2);
    page.draw_plain(cx);
    page.draw_plain(cx);
    assert!(page.replays(cx) > replays, "the sibling is replayed");
    let replayed = page.observe(cx);
    page.draw_whole(cx);
    let whole = page.observe(cx);
    assert_same(&replayed.frame, &whole.frame, "a frame that replays the roots after a spliced one");
    assert_eq!(replayed.probes, whole.probes);

    // And the sibling is patched again from that frame, after the unit is spliced once more.
    page.state.change(6);
    page.tick(cx);
    assert!(page.patches(cx).relayout_patches >= 2, "{:?}", page.state.seen.borrow());
    page.state.patch_unit.set(false);
    page.state.patch_sibling.set(true);
    page.state.sibling_green.set(false);
    page.tick(cx);
    page.assert_whole(cx, "a sibling patched after the second splice");
}

#[gpui::test]
fn the_verifier_finds_no_difference_in_a_run_of_splices(cx: &mut TestAppContext) {
    let state = State::new(cx, 3, false);
    let page = Fixture::new(cx, state);
    cx.update_window(page.window, |_, window, _| window.set_retention_verifier(Some(Rc::new(|_| String::new())))).unwrap();
    page.settle(cx);
    for kids in [7, 1, 5, 3] {
        page.state.change(kids);
        page.tick(cx);
        page.settle(cx);
    }
    let (verified, mismatches) = cx
        .update_window(page.window, |_, window, _| (window.retained_counters().verified, window.retention_mismatches().to_vec()))
        .unwrap();
    assert!(verified > 0);
    assert!(mismatches.is_empty(), "{mismatches:?}");
    assert!(page.patches(cx).relayout_patches >= 4, "{:?}", page.state.seen.borrow());
}

#[gpui::test]
fn a_root_spliced_whole_of_nested_roots_leaves_nothing_that_replays_or_patches_them(cx: &mut TestAppContext) {
    let state = State::new(cx, 3, false);
    state.nested.set(true);
    let page = Fixture::new(cx, state);
    let nested = ElementId::from(NESTED);
    // A frame that replays the unit does not register the roots inside it, so it is a frame built whole.
    page.draw_whole(cx);
    assert_eq!(
        cx.update_window(page.window, |_, window, _| window.patch_blocker(&nested)).unwrap(),
        None,
        "the nested root is patchable in a frame built whole"
    );

    // The patch builds the unit flat: the nested root is not in the frame it leaves.
    page.state.change(7);
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
    assert_eq!(
        cx.update_window(page.window, |_, window, _| window.patch_blocker(&nested)).unwrap(),
        Some(PatchDecline::UnknownUnit),
        "the nested root is not patchable until a frame records it"
    );

    // A tick that would patch the nested root is declined, and the frame built whole is right.
    page.state.patch_unit.set(false);
    page.state.patch_nested.set(true);
    let before = page.patches(cx);
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::UnknownUnit) - before.declines(PatchDecline::UnknownUnit), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.patched_frames, before.patched_frames);
    let drawn = page.observe(cx);
    page.draw_whole(cx);
    let whole = page.observe(cx);
    assert_same(&drawn.frame, &whole.frame, "the frame drawn after the decline");

    // A frame built whole records the nested root again, and the tick patches it.
    page.state.patch_nested.set(false);
    page.state.patch_unit.set(true);
    page.state.change(7);
    page.draw_whole(cx);
    assert_eq!(
        cx.update_window(page.window, |_, window, _| window.patch_blocker(&nested)).unwrap(),
        None,
        "the nested root is patchable again"
    );
    page.state.patch_unit.set(false);
    page.state.patch_nested.set(true);
    let before = page.patches(cx);
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.patched_frames - before.patched_frames, 1, "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "a nested root patched after a frame recorded it again");
}

#[gpui::test]
fn the_rows_of_a_list_after_a_spliced_root_are_translated_as_the_frame_built_whole_is(cx: &mut TestAppContext) {
    let wheel = |page: &Fixture, cx: &mut TestAppContext, by: f32| {
        cx.update_window(page.window, |_, window, cx| {
            let event = ScrollWheelEvent {
                position: point(px(10.), px(170.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
    };
    let run = |cx: &mut TestAppContext, patch: bool| {
        let state = State::new(cx, 3, true);
        state.patch_unit.set(patch);
        let page = Fixture::new(cx, state);
        cx.update_window(page.window, |_, window, _| window.set_scroll_patch(patch)).unwrap();
        page.state.change(7);
        page.tick(cx);
        if !patch {
            page.draw_whole(cx);
        }
        // The rows, put where a fraction of a row moves them, from the frame the splice left.
        cx.update_window(page.window, |_, window, cx| window.simulate_mouse_move(point(px(10.), px(170.)), cx)).unwrap();
        page.draw_plain(cx);
        for by in [5., 5., 7.] {
            wheel(&page, cx, by);
        }
        let counters = cx.update_window(page.window, |_, window, _| window.scroll_patch_counters()).unwrap();
        if !patch {
            // A plain draw replays the unit with the hover it was recorded with; the reference is built from nothing.
            page.draw_whole(cx);
        }
        let shown = page.observe(cx);
        (page, shown, counters)
    };
    let (patched, shown, counters) = run(cx, true);
    let (_, built, _) = run(cx, false);
    // The two runs are two windows.
    assert_eq!(patched.patches(cx).relayout_patches, 1, "{:?}", patched.state.seen.borrow());
    assert!(counters.patched >= 1, "the rows are translated after the splice: {counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    assert_same(&same_window(&shown.frame), &same_window(&built.frame), "the rows translated after a splice");
}

#[gpui::test]
fn a_root_that_held_nothing_and_gains_boxes_leaves_the_runs_of_the_lists_around_it(cx: &mut TestAppContext) {
    for list_first in [false, true] {
        let at = if list_first { (10., 50.) } else { (10., 170.) };
        let run = |cx: &mut TestAppContext, patch: bool| {
            let state = State::new(cx, 0, true);
            state.bare.set(true);
            state.list_first.set(list_first);
            state.patch_unit.set(patch);
            let page = Fixture::new(cx, state);
            cx.update_window(page.window, |_, window, _| window.set_scroll_patch(patch)).unwrap();
            // The runs are recorded with the pointer over the rows, and the wheel is the first frame after the splice.
            cx.update_window(page.window, |_, window, cx| window.simulate_mouse_move(point(px(at.0), px(at.1)), cx)).unwrap();
            page.draw_plain(cx);
            page.draw_plain(cx);
            page.state.bare.set(false);
            page.state.change(3);
            page.tick(cx);
            if !patch {
                page.draw_whole(cx);
            }
            for by in [5., 5., 7.] {
                cx.update_window(page.window, |_, window, cx| {
                    let event = ScrollWheelEvent {
                        position: point(px(at.0), px(at.1)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                        ..Default::default()
                    };
                    window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
                })
                .unwrap();
            }
            let counters = cx.update_window(page.window, |_, window, _| window.scroll_patch_counters()).unwrap();
            if !patch {
                page.draw_whole(cx);
            }
            let shown = page.observe(cx);
            (page, shown, counters)
        };
        let (patched, shown, counters) = run(cx, true);
        let (_, built, _) = run(cx, false);
        assert_eq!(patched.patches(cx).relayout_patches, 1, "list_first {list_first}: {:?}", patched.state.seen.borrow());
        assert!(counters.patched >= 1, "list_first {list_first}: the rows are translated after the splice: {counters:?}");
        assert_eq!(counters.held_cause(ScrollPatchCause::NoRun), 0, "list_first {list_first}: {counters:?}");
        assert_eq!(counters.mispredicted, 0, "list_first {list_first}: {counters:?}");
        assert_same(&same_window(&shown.frame), &same_window(&built.frame), "the rows translated after a splice of a root that was empty");
    }
}

/// A page patched by `ticks` and one that is built whole for the same state, to compare what they show.
struct Pair {
    patched: Fixture,
    whole: Fixture,
}

impl Pair {
    fn new(cx: &mut TestAppContext, kids: usize, key_stays: bool) -> Self {
        Self::with_unit(cx, kids, key_stays, false)
    }

    fn with_unit(cx: &mut TestAppContext, kids: usize, key_stays: bool, relocatable: bool) -> Self {
        Self::with_page(cx, kids, key_stays, relocatable, false, false)
    }

    /// The unit is a relocatable root inside a scroller, and a frame that replays it can put it elsewhere than it was recorded.
    fn scrolled(cx: &mut TestAppContext, kids: usize, list: bool) -> Self {
        Self::with_page(cx, kids, true, true, true, list)
    }

    fn with_page(cx: &mut TestAppContext, kids: usize, key_stays: bool, relocatable: bool, scrolled: bool, list: bool) -> Self {
        Self::with_anchoring(cx, kids, key_stays, relocatable, scrolled, list, false)
    }

    fn with_anchoring(cx: &mut TestAppContext, kids: usize, key_stays: bool, relocatable: bool, scrolled: bool, list: bool, anchoring: bool) -> Self {
        let state = State::new(cx, kids, list);
        state.anchoring.set(anchoring);
        state.relocatable.set(relocatable);
        state.scrolled.set(scrolled);
        state.tooltip.set(!scrolled);
        let patched = Fixture::new(cx, state);
        let reference = State::new(cx, kids, list);
        reference.anchoring.set(anchoring);
        reference.relocatable.set(relocatable);
        reference.scrolled.set(scrolled);
        reference.tooltip.set(!scrolled);
        reference.patch_unit.set(false);
        let whole = Fixture::new(cx, reference);
        for state in [&patched.state, &whole.state] {
            state.key_stays.set(key_stays);
        }
        if scrolled {
            for fixture in [&patched, &whole] {
                cx.update_window(fixture.window, |_, window, cx| window.simulate_mouse_move(point(px(150.), px(60.)), cx)).unwrap();
                fixture.draw_whole(cx);
            }
        }
        Self { patched, whole }
    }

    /// Moves the scroller of both pages to `top` with a wheel and draws the patched one, which puts the unit where it was
    /// not recorded.
    fn scroll_to(&self, cx: &mut TestAppContext, top: f32) {
        let now = -self.patched.state.scroll.offset().y.0;
        self.wheel(cx, top - now);
        self.patched.draw_plain(cx);
    }

    /// A wheel over the unit's scroller beside the unit (the recording holds the hover it was made with, so the pointer stays off
    /// it), in the patched page and the other.
    fn wheel(&self, cx: &mut TestAppContext, by: f32) {
        for fixture in [&self.patched, &self.whole] {
            cx.update_window(fixture.window, |_, window, cx| {
                let event = ScrollWheelEvent {
                    position: point(px(150.), px(60.)),
                    delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                    ..Default::default()
                };
                window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
            })
            .unwrap();
        }
    }

    fn relocations(&self, cx: &mut TestAppContext) -> u64 {
        cx.update_window(self.patched.window, |_, window, _| window.retained_counters().relocations).unwrap()
    }

    /// A change that the patch lays out again, which the unit patches and whose recording it rewrites.
    #[track_caller]
    fn patch_step(&self, cx: &mut TestAppContext, what: &str, change: impl Fn(&State)) {
        let before = self.patched.patches(cx);
        self.set(change);
        self.patched.tick(cx);
        let after = self.patched.patches(cx);
        let seen = self.patched.state.seen.borrow();
        assert_eq!(after.relayout_patches, before.relayout_patches + 1, "{what}: {seen:?}");
        assert_eq!(after.rewritten_recordings, before.rewritten_recordings + 1, "{what}: {seen:?}");
        assert_eq!(after.total_declines(), 0, "{what}: {seen:?}");
        drop(seen);
        self.assert_same(cx, what);
    }

    fn set(&self, change: impl Fn(&State)) {
        for state in [&self.patched.state, &self.whole.state] {
            change(state);
            state.version.set(state.version.get() + 1);
        }
    }

    /// The patched page shows what the other, built whole from the same state, does, without a frame being
    /// drawn in it (a window that is sent events draws).
    #[track_caller]
    fn assert_same(&self, cx: &mut TestAppContext, what: &str) {
        self.whole.draw_whole(cx);
        let patched = self.patched.passive(cx);
        let whole = self.whole.passive(cx);
        assert_same(&same_window(&patched.frame), &same_window(&whole.frame), what);
        assert_eq!(
            patched.probes.iter().map(|probe| same_window(probe)).collect::<Vec<_>>(),
            whole.probes.iter().map(|probe| same_window(probe)).collect::<Vec<_>>(),
            "{what}: what the frame answers"
        );
    }
}

#[gpui::test]
fn three_relayout_patches_of_a_root_in_a_row_are_all_patched_and_each_frame_is_the_one_built_whole(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, 3, false);
    let steps: [(&str, Box<dyn Fn(&State)>); 3] = [
        ("same shape", Box::new(|state: &State| state.kid_height.set(14.))),
        ("more children", Box::new(|state: &State| state.kids.set(7))),
        ("fewer children", Box::new(|state: &State| state.kids.set(2))),
    ];
    for (what, change) in steps {
        let before = pair.patched.patches(cx);
        pair.set(change);
        pair.patched.tick(cx);
        let after = pair.patched.patches(cx);
        let seen = pair.patched.state.seen.borrow();
        assert_eq!(after.relayout_patches, before.relayout_patches + 1, "{what}: {seen:?}");
        assert_eq!(after.rewritten_recordings, before.rewritten_recordings + 1, "{what}: {seen:?}");
        assert_eq!(after.total_declines(), 0, "{what}: {seen:?}");
        drop(seen);
        pair.assert_same(cx, what);
    }
}

#[gpui::test]
fn three_relayout_patches_of_a_root_recorded_to_be_put_elsewhere_are_all_patched_and_a_frame_after_them_replays_it(cx: &mut TestAppContext) {
    let pair = Pair::with_unit(cx, 3, true, true);
    let steps: [(&str, Box<dyn Fn(&State)>); 3] = [
        ("same shape", Box::new(|state: &State| state.kid_height.set(14.))),
        ("more children", Box::new(|state: &State| state.kids.set(7))),
        ("fewer children", Box::new(|state: &State| state.kids.set(4))),
    ];
    for (what, change) in steps {
        let before = pair.patched.patches(cx);
        pair.set(change);
        pair.patched.tick(cx);
        let after = pair.patched.patches(cx);
        let seen = pair.patched.state.seen.borrow();
        assert_eq!(after.relayout_patches, before.relayout_patches + 1, "{what}: {seen:?}");
        assert_eq!(after.rewritten_recordings, before.rewritten_recordings + 1, "{what}: {seen:?}");
        assert_eq!(after.total_declines(), 0, "{what}: {seen:?}");
        drop(seen);
        pair.assert_same(cx, what);
    }
    let replays = pair.patched.replays(cx);
    pair.patched.draw_plain(cx);
    assert!(pair.patched.replays(cx) > replays, "the unit is replayed from the recording the patches left");
    pair.assert_same(cx, "replayed after three patches");
    // And patched again from the recording the replay left.
    let before = pair.patched.patches(cx);
    pair.set(|state| state.kids.set(5));
    pair.patched.tick(cx);
    let after = pair.patched.patches(cx);
    assert_eq!(after.relayout_patches, before.relayout_patches + 1, "{:?}", pair.patched.state.seen.borrow());
    pair.assert_same(cx, "patched after a replay");
}

/// The tooltip of the third box is one of the reasons the recording of the root cannot be put elsewhere, which a patch
/// that takes the box away changes: the recording is not rewritten (it would have to say it can be moved), and the root
/// is built whole by the next frame.
#[gpui::test]
fn a_relayout_patch_that_changes_why_a_root_cannot_be_put_elsewhere_does_not_rewrite_its_recording(cx: &mut TestAppContext) {
    let pair = Pair::with_unit(cx, 3, true, true);
    let before = pair.patched.patches(cx);
    pair.set(|state| state.kids.set(2));
    pair.patched.tick(cx);
    let after = pair.patched.patches(cx);
    let seen = pair.patched.state.seen.borrow();
    assert_eq!(after.relayout_patches, before.relayout_patches + 1, "{seen:?}");
    assert_eq!(after.rewritten_recordings, before.rewritten_recordings, "{seen:?}");
    assert_eq!(after.total_declines(), 0, "{seen:?}");
    drop(seen);
    pair.assert_same(cx, "without the box with the tooltip");
}

#[gpui::test]
fn a_relayout_patch_of_a_relocatable_root_that_a_scroll_moved_patches_and_rewrites_it(cx: &mut TestAppContext) {
    let pair = Pair::scrolled(cx, 3, false);
    let moved = pair.relocations(cx);
    pair.scroll_to(cx, 24.);
    assert!(pair.relocations(cx) > moved, "the frame after the scroll replays the unit away from where it was recorded");
    pair.assert_same(cx, "replayed away from where it was recorded");
    let steps: [(&str, Box<dyn Fn(&State)>); 3] = [
        ("same shape", Box::new(|state: &State| state.kid_height.set(14.))),
        ("more children", Box::new(|state: &State| state.kids.set(7))),
        ("fewer children", Box::new(|state: &State| state.kids.set(4))),
    ];
    for (what, change) in steps {
        pair.patch_step(cx, what, change);
    }
    // A frame after the patches replays the rewritten recording the same distance away, and the patch goes on from it.
    let replays = pair.patched.replays(cx);
    pair.patched.draw_plain(cx);
    assert!(pair.patched.replays(cx) > replays, "the unit is replayed from the recording the patches left");
    pair.assert_same(cx, "replayed after three patches of a moved root");
    pair.patch_step(cx, "patched after a replay", |state| state.kids.set(5));
}

/// A root a scroll moved whose patch would not rewrite its recording (the box with the tooltip is a reason it cannot be put
/// elsewhere) leaves the handles it holds outside the frame where it was recorded, `delta` from where it is: the patch is
/// declined, and the frame built whole is what is shown.
#[gpui::test]
fn a_relayout_patch_of_a_root_a_scroll_moved_that_cannot_rewrite_its_recording_is_declined(cx: &mut TestAppContext) {
    let pair = Pair::scrolled(cx, 3, false);
    pair.scroll_to(cx, 24.);
    let before = pair.patched.patches(cx);
    pair.set(|state| {
        state.tooltip.set(true);
        state.kids.set(7);
    });
    pair.patched.tick(cx);
    let after = pair.patched.patches(cx);
    let seen = pair.patched.state.seen.borrow();
    assert_eq!(after.relayout_patches, before.relayout_patches, "{seen:?}");
    assert_eq!(after.declines(PatchDecline::Relocated), before.declines(PatchDecline::Relocated) + 1, "{seen:?}");
    drop(seen);
    pair.assert_same(cx, "a moved root that cannot be rewritten, built whole");
}

#[gpui::test]
fn scrolls_and_relayout_patches_of_a_relocatable_root_alternate_and_each_frame_is_the_one_built_whole(cx: &mut TestAppContext) {
    let pair = Pair::scrolled(cx, 3, false);
    let tops = [24., 10., 0., 31., 31., 7.];
    for (step, top) in tops.into_iter().enumerate() {
        pair.scroll_to(cx, top);
        pair.assert_same(cx, &format!("scrolled to {top} (step {step})"));
        let kids = 3 + (step * 2 + 1) % 5;
        pair.patch_step(cx, &format!("patched at {top} (step {step})"), move |state| {
            state.kids.set(kids);
            state.kid_height.set(12. + (step % 3) as f32);
        });
        // A scroll that follows the patch moves the rewritten recording.
        pair.scroll_to(cx, top + 5.);
        pair.assert_same(cx, &format!("scrolled again after a patch at {top} (step {step})"));
    }
}

#[gpui::test]
fn a_wheel_on_the_scroller_of_a_patched_root_and_the_rows_of_a_list_around_it_translate_as_the_frame_built_whole_is(cx: &mut TestAppContext) {
    let pair = Pair::scrolled(cx, 3, true);
    for (step, by) in [(0, 9.), (1, 5.), (2, 7.)] {
        pair.wheel(cx, by);
        pair.patched.draw_plain(cx);
        pair.patch_step(cx, &format!("patched after a wheel (step {step})"), move |state| state.kids.set(4 + step));
        // The rows of the list are put where the scroll moves them from their recordings, which the patches of the unit
        // left in the frame as a frame built whole does.
        for fixture in [&pair.patched, &pair.whole] {
            cx.update_window(fixture.window, |_, window, cx| {
                let event = ScrollWheelEvent {
                    position: point(px(10.), px(170.)),
                    delta: ScrollDelta::Pixels(point(px(0.), px(-6.))),
                    ..Default::default()
                };
                window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
            })
            .unwrap();
        }
        let counters = cx.update_window(pair.patched.window, |_, window, _| window.scroll_patch_counters()).unwrap();
        assert_eq!(counters.mispredicted, 0, "{counters:?}");
        pair.assert_same(cx, &format!("rows moved after a patch of the moved unit (step {step})"));
    }
}

#[gpui::test]
fn a_frame_that_replays_a_root_after_relayout_patches_shows_what_a_frame_built_whole_does(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, 3, true);
    for kids in [7, 1, 5] {
        pair.set(|state| state.kids.set(kids));
        pair.patched.tick(cx);
        assert_eq!(pair.patched.patches(cx).total_declines(), 0, "{:?}", pair.patched.state.seen.borrow());
        let replays = pair.patched.replays(cx);
        pair.patched.draw_plain(cx);
        assert!(pair.patched.replays(cx) > replays, "{kids} kids: the unit is replayed from the recording the patch left");
        pair.assert_same(cx, &format!("replayed after a patch to {kids} kids"));
    }
}

#[gpui::test]
fn a_patch_that_does_not_lay_out_again_patches_from_the_recording_a_relayout_patch_left(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, 3, false);
    // A change of how many boxes there are lays the root out again; one of their colour is repainted over the hitboxes and
    // tab stops that the rewritten recording holds.
    for (kids, green, relaid) in [(7, false, true), (7, true, false), (4, true, true), (4, false, false)] {
        pair.set(|state| {
            state.kids.set(kids);
            state.kid_green.set(green);
        });
        let before = pair.patched.patches(cx);
        pair.patched.tick(cx);
        let after = pair.patched.patches(cx);
        let seen = pair.patched.state.seen.borrow();
        assert_eq!(after.patched_frames, before.patched_frames + 1, "{kids} {green}: {seen:?}");
        assert_eq!(after.relayout_patches - before.relayout_patches, u64::from(relaid), "{kids} {green}: {seen:?}");
        assert_eq!(after.total_declines(), 0, "{kids} {green}: {seen:?}");
        drop(seen);
        pair.assert_same(cx, &format!("{kids} boxes, green {green}"));
    }
}

#[gpui::test]
fn the_verifier_finds_no_difference_in_what_a_frame_replays_of_a_recording_a_patch_rewrote(cx: &mut TestAppContext) {
    let state = State::new(cx, 3, false);
    state.key_stays.set(true);
    let page = Fixture::new(cx, state);
    cx.update_window(page.window, |_, window, _| window.set_retention_verifier(Some(Rc::new(|_| String::new())))).unwrap();
    page.settle(cx);
    for kids in [7, 1, 5, 3, 6] {
        page.state.change(kids);
        page.tick(cx);
    }
    let (verified, mismatches) = cx
        .update_window(page.window, |_, window, _| (window.retained_counters().verified, window.retention_mismatches().to_vec()))
        .unwrap();
    assert!(verified >= 5, "{verified}");
    assert!(mismatches.is_empty(), "{mismatches:?}");
    let patches = page.patches(cx);
    assert_eq!(patches.relayout_patches, 5, "{:?}", page.state.seen.borrow());
    assert_eq!(patches.rewritten_recordings, 5);
    assert_eq!(patches.total_declines(), 0, "{:?}", page.state.seen.borrow());
}

/// Several roots that lay out again in one patch, each spliced: the splices are made last first, and the
/// element states the paint accessed are counted after all the ones the prepaint did, so a root that
/// had more or fewer of them moves where the paint states of the roots before it are.
mod several_roots {
    use super::*;
    use crate::elements::RetainedState;
    use crate::{Div, GlobalElementId, Stateful};
    use std::any::TypeId;

    const ROOTS: [&str; 3] = ["first", "second", "third"];

    /// A div keeps an element state only when something reads it, and these tests are about the states a frame
    /// accesses, so every element here has a hover style, which does.
    fn stateful(id: impl Into<ElementId>) -> Stateful<Div> {
        div().id(id).hover(|style| style)
    }

    struct Roots {
        kids: [Cell<usize>; 3],
        version: Cell<u64>,
        seen: RefCell<Vec<Result<(), PatchDecline>>>,
    }

    impl Roots {
        fn unit(&self, root: usize) -> AnyElement {
            let name = ROOTS[root];
            let mut unit = stateful(name).w(px(120.)).h(px(60.)).flex_none().overflow_hidden().bg(blue());
            for index in 0..self.kids[root].get() {
                unit = unit.child(stateful((name, index)).h(px(8.)).flex_none().bg(if index % 2 == 0 { red() } else { green() }));
            }
            unit.into_any_element()
        }
    }

    struct Rooted(Rc<Roots>);

    impl Render for Rooted {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let mut page = stateful("page")
                .w(px(400.))
                .h(px(400.))
                .flex()
                .flex_col()
                .child(stateful("header").w(px(60.)).h(px(20.)).flex_none().bg(green()));
            for root in 0..ROOTS.len() {
                let state = self.0.clone();
                page = page.child(retained(ROOTS[root], 1_000 + self.0.version.get(), move |_, _| state.unit(root)).patchable_relayout());
            }
            page.child(stateful("footer").w(px(60.)).h(px(20.)).flex_none().bg(green())).into_any_element()
        }
    }

    struct Page {
        window: AnyWindowHandle,
        state: Rc<Roots>,
    }

    impl Page {
        fn new(cx: &mut TestAppContext, kids: [usize; 3]) -> Self {
            let state = Rc::new(Roots { kids: kids.map(Cell::new), version: Cell::new(0), seen: RefCell::new(Vec::new()) });
            let window = AnyWindowHandle::from(cx.open_window(size(px(400.), px(400.)), {
                let state = state.clone();
                move |_, _| Rooted(state)
            }));
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.set_contained_patches(true);
                let patched = state.clone();
                window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                    let units = (0..ROOTS.len()).map(|root| (ElementId::from(ROOTS[root]), patched.unit(root))).collect();
                    let outcome = window.repaint_in_place(units, cx, |_, _| true);
                    patched.seen.borrow_mut().push(outcome.as_ref().map(|_| ()).map_err(|decline| *decline));
                    outcome.is_ok()
                })));
                window.draw(cx).clear(cx);
                window.draw(cx).clear(cx);
                window.present_cached_frame();
            })
            .unwrap();
            Self { window, state }
        }

        fn change(&self, kids: [usize; 3]) {
            for (cell, count) in self.state.kids.iter().zip(kids) {
                cell.set(count);
            }
            self.state.version.set(self.state.version.get() + 1);
        }

        fn draw(&self, cx: &mut TestAppContext, tick: bool) {
            cx.update_window(self.window, |_, window, cx| {
                match tick {
                    true => window.request_tick(),
                    false => window.refresh(),
                }
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }

        /// Settles the page with frames built whole, so that a later tick patches from recordings.
        fn settle(&self, cx: &mut TestAppContext) {
            self.draw(cx, false);
            self.draw(cx, false);
        }

        fn patches(&self, cx: &mut TestAppContext) -> PatchCounters {
            cx.update_window(self.window, |_, window, _| window.patch_counters()).unwrap()
        }

        /// What the rendered frame paints, which hitboxes it holds, and the element states it accessed, in order.
        fn shown(&self, cx: &mut TestAppContext) -> (String, Vec<(GlobalElementId, TypeId)>) {
            cx.update_window(self.window, |_, window, _| {
                let frame = &window.rendered_frame;
                let hitboxes: Vec<String> = frame.hitboxes.iter().map(|hitbox| hitbox.describe()).collect();
                (format!("{:#?}\n{hitboxes:#?}", frame.scene.quads), frame.accessed_element_states.clone())
            })
            .unwrap()
        }

        /// Draws a frame that patches and then one built whole, and asserts that they leave the same.
        #[track_caller]
        fn assert_whole(&self, cx: &mut TestAppContext, what: &str) {
            let (patched, patched_states) = self.shown(cx);
            self.draw(cx, false);
            let (whole, whole_states) = self.shown(cx);
            assert_same(&same_window(&patched), &same_window(&whole), what);
            // The states the layout accessed come first, and a patch makes no layout of the page: the ones the prepaint
            // and the paint accessed are what it splices. The root's own state is its recording's, which a patch keeps.
            let names = |states: &[(GlobalElementId, TypeId)]| -> Vec<String> {
                let own = TypeId::of::<RetainedState>();
                let names: Vec<String> = states.iter().filter(|(_, ty)| *ty != own).map(|(id, _)| format!("{:?}", &id.ids[1..])).collect();
                let page = names.iter().skip(1).position(|name| *name == names[0]).expect("the prepaint starts at the page") + 1;
                names[page..].to_vec()
            };
            let (patched_names, whole_names) = (names(&patched_states), names(&whole_states));
            if patched_names != whole_names {
                let short = |names: &[String]| {
                    names.iter().map(|name| name.replace("Name(", "").replace("NamedInteger(", "").replace([')', '"', '[', ']'], "").replace("page, ", "")).collect::<Vec<_>>().join(" | ")
                };
                panic!(
                    "{what}: the element states the frame accessed differ ({} against {}):\npatched: {}\nwhole:   {}",
                    patched_names.len(),
                    whole_names.len(),
                    short(&patched_names),
                    short(&whole_names)
                );
            }
        }
    }

    #[gpui::test]
    fn roots_that_each_gain_or_lose_element_states_in_one_patch_leave_the_accessed_states_a_frame_built_whole_does(cx: &mut TestAppContext) {
        let page = Page::new(cx, [3, 3, 3]);
        page.settle(cx);
        for (what, kids) in [("more, fewer, more", [6, 1, 5]), ("fewer, more, fewer", [2, 7, 3]), ("more, more, more", [4, 9, 12])] {
            let before = page.patches(cx);
            page.change(kids);
            page.draw(cx, true);
            let after = page.patches(cx);
            let seen = page.state.seen.borrow();
            assert_eq!(after.relayout_patches - before.relayout_patches, 3, "{what}: {seen:?}");
            assert_eq!(after.total_declines(), 0, "{what}: {seen:?}");
            drop(seen);
            page.assert_whole(cx, what);
        }
    }

    #[gpui::test]
    fn a_frame_that_replays_roots_that_were_spliced_together_shows_what_a_frame_built_whole_does(cx: &mut TestAppContext) {
        let page = Page::new(cx, [3, 3, 3]);
        page.settle(cx);
        for kids in [[6, 1, 5], [2, 7, 5]] {
            page.change(kids);
            page.draw(cx, true);
            assert_eq!(page.patches(cx).total_declines(), 0, "{:?}", page.state.seen.borrow());
            // The recordings of the roots hold the places the splices left them: another patch and a replay use them.
            page.change([kids[0] + 1, kids[1] + 2, kids[2]]);
            page.draw(cx, true);
            assert_eq!(page.patches(cx).total_declines(), 0, "{:?}", page.state.seen.borrow());
            page.assert_whole(cx, &format!("patched again after {kids:?}"));
        }
    }
}

#[gpui::test]
fn the_verifier_finds_no_difference_in_a_patch_of_a_root_in_place_and_a_root_spliced(cx: &mut TestAppContext) {
    let state = State::new(cx, 3, false);
    let page = Fixture::new(cx, state);
    cx.update_window(page.window, |_, window, _| window.set_retention_verifier(Some(Rc::new(|_| String::new())))).unwrap();
    page.settle(cx);
    for (kids, green) in [(7, true), (1, false), (5, true)] {
        page.state.change(kids);
        page.state.patch_sibling.set(true);
        page.state.sibling_green.set(green);
        page.tick(cx);
        page.settle(cx);
    }
    let (verified, mismatches) = cx
        .update_window(page.window, |_, window, _| (window.retained_counters().verified, window.retention_mismatches().to_vec()))
        .unwrap();
    assert!(verified > 0);
    assert_eq!(page.patches(cx).relayout_patches, 3, "{:?}", page.state.seen.borrow());
    assert_eq!(page.patches(cx).patched_units, 6, "{:?}", page.state.seen.borrow());
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

const LOWER: &str = "lower";
const UPPER: &str = "upper";

/// Two roots that overlap at the pointer, the upper one painted over the lower: in the patch the
/// lower root keeps its box under the pointer and the upper root moves a box that occludes into it.
struct Overlap {
    covering: Cell<bool>,
    height: Cell<f32>,
    log: RefCell<Vec<String>>,
}

impl Overlap {
    fn lower(self: &Rc<Self>) -> AnyElement {
        let down = self.clone();
        div()
            .id(LOWER)
            .w(px(60.))
            .h(px(100.))
            .flex_none()
            .child(
                div()
                    .id("lower-box")
                    .w(px(60.))
                    .h(px(self.height.get()))
                    .bg(red())
                    .hover(|style| style.bg(green()))
                    .on_mouse_down(MouseButton::Left, move |_, _, _| down.log.borrow_mut().push("lower".to_owned())),
            )
            .into_any_element()
    }

    fn upper(self: &Rc<Self>) -> AnyElement {
        let down = self.clone();
        let margin = if self.covering.get() { 0. } else { 60. };
        div()
            .id(UPPER)
            .mt(px(-100.))
            .w(px(60.))
            .h(px(100.))
            .flex_none()
            .child(
                div()
                    .id("upper-box")
                    .mt(px(margin))
                    .w(px(60.))
                    .h(px(30.))
                    .occlude()
                    .bg(blue())
                    .hover(|style| style.bg(green()))
                    .on_mouse_down(MouseButton::Left, move |_, _, _| down.log.borrow_mut().push("upper".to_owned())),
            )
            .into_any_element()
    }
}

struct OverlapPage(Rc<Overlap>);

impl Render for OverlapPage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (lower, upper) = (self.0.clone(), self.0.clone());
        div()
            .id("page")
            .flex()
            .flex_col()
            .w(px(400.))
            .h(px(400.))
            .child(retained(LOWER, 1, move |_, _| lower.lower()).patchable_relayout())
            .child(retained(UPPER, 2, move |_, _| upper.upper()).patchable_relayout())
            .into_any_element()
    }
}

struct OverlapFixture {
    window: AnyWindowHandle,
    state: Rc<Overlap>,
}

impl OverlapFixture {
    fn new(cx: &mut TestAppContext, upper_first: bool) -> Self {
        let state = Rc::new(Overlap { covering: Cell::new(false), height: Cell::new(40.), log: RefCell::new(Vec::new()) });
        let window = AnyWindowHandle::from(cx.open_window(size(px(400.), px(400.)), {
            let state = state.clone();
            move |_, _| OverlapPage(state)
        }));
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_contained_patches(true);
            let patched = state.clone();
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                let mut units = vec![(ElementId::from(LOWER), patched.lower()), (ElementId::from(UPPER), patched.upper())];
                if upper_first {
                    units.reverse();
                }
                window.repaint_in_place(units, cx, |_, _| true).is_ok()
            })));
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(point(px(10.), px(20.)), cx);
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            window.present_cached_frame();
        })
        .unwrap();
        Self { window, state }
    }

    /// What the pointer hits, how the frame paints it and which box a click reaches.
    fn observe(&self, cx: &mut TestAppContext) -> (Vec<String>, String, Vec<String>) {
        let state = self.state.clone();
        cx.update_window(self.window, |_, window, cx| {
            let hits: Vec<String> = window.mouse_hit_test.ids.iter().map(|id| name_of(window, *id)).collect();
            let frame = frame_text(window);
            state.log.borrow_mut().clear();
            Fixture::click(window, point(px(10.), px(20.)), cx);
            let clicked = state.log.borrow().clone();
            (hits, frame, clicked)
        })
        .unwrap()
    }
}

#[gpui::test]
fn two_roots_that_lay_out_again_in_one_patch_hit_test_the_pointer_as_a_frame_built_whole_does(cx: &mut TestAppContext) {
    for upper_first in [false, true] {
        let page = OverlapFixture::new(cx, upper_first);
        page.state.covering.set(true);
        page.state.height.set(41.);
        cx.update_window(page.window, |_, window, cx| {
            window.request_tick();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let patches = cx.update_window(page.window, |_, window, _| window.patch_counters()).unwrap();
        assert_eq!((patches.patched_frames, patches.patched_units, patches.relayout_patches), (1, 2, 2), "{upper_first}");
        assert_eq!(patches.total_declines(), 0, "{upper_first}");
        let patched = page.observe(cx);
        cx.update_window(page.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let whole = page.observe(cx);
        assert_eq!(patched.0, whole.0, "upper first {upper_first}: what the pointer hits");
        assert_same(&patched.1, &whole.1, "the frame");
        assert_eq!(patched.2, whole.2, "upper first {upper_first}: the box a click reaches");
    }
}

/// The scroller keeps `overflow-anchor: auto`: what a patch of a row shows, and where the scroller is left, is what a frame
/// built whole shows. A box in view that the root holds is the anchor at 24 (the third box), and an edit that moves it or
/// restyles it is declined (a frame built whole scrolls the scroller, or drops the anchor); where the anchor is the root
/// itself (40, whose boxes are above the view), outside it (110, the root is above the view), or the edit leaves it be, the
/// edit is a patch.
#[gpui::test]
fn a_relayout_patch_in_a_scroller_that_anchors_leaves_the_offset_and_the_frame_a_frame_built_whole_does(cx: &mut TestAppContext) {
    let edits: [(&str, Box<dyn Fn(&State)>); 5] = [
        ("taller boxes", Box::new(|state: &State| state.kid_height.set(14.))),
        ("more boxes", Box::new(|state: &State| state.kids.set(7))),
        ("fewer boxes", Box::new(|state: &State| state.kids.set(2))),
        ("taller first box", Box::new(|state: &State| state.first_extra.set(8.))),
        ("shorter first box", Box::new(|state: &State| state.first_extra.set(-6.))),
    ];
    for top in [0., 24., 40., 70., 110.] {
        for (what, change) in &edits {
            let pair = Pair::with_anchoring(cx, 3, true, true, true, false, true);
            pair.scroll_to(cx, top);
            pair.assert_same(cx, "scrolled");
            pair.set(change);
            pair.patched.tick(cx);
            let seen = pair.patched.state.seen.borrow().clone();
            let moves_anchor = top == 24. && matches!(*what, "taller boxes" | "taller first box" | "shorter first box");
            let expected = if moves_anchor { vec![Err(PatchDecline::NotContained)] } else { vec![Ok(())] };
            assert_eq!(seen, expected, "{what} at {top}");
            pair.assert_same(cx, &format!("{what} at {top}"));
            assert_eq!(
                pair.patched.state.scroll.offset().y,
                pair.whole.state.scroll.offset().y,
                "{what} at {top}: the offset"
            );
        }
    }
}

/// An edit that is declined for the anchor is followed by the frame built whole, which finds the styles the patch set and
/// no restyle: the anchor is dropped as in a frame built whole from the page before, not compensated for.
#[gpui::test]
fn patches_that_move_the_anchor_and_frames_after_them_leave_the_offset_a_frame_built_whole_does(cx: &mut TestAppContext) {
    let pair = Pair::with_anchoring(cx, 3, true, true, true, false, true);
    pair.scroll_to(cx, 24.);
    for (what, change) in [
        ("taller first box", Box::new(|state: &State| state.first_extra.set(8.)) as Box<dyn Fn(&State)>),
        ("taller boxes", Box::new(|state: &State| state.kid_height.set(14.))),
        ("shorter first box", Box::new(|state: &State| state.first_extra.set(-6.))),
        ("fewer boxes", Box::new(|state: &State| state.kids.set(2))),
    ] {
        pair.set(change);
        pair.patched.tick(cx);
        pair.assert_same(cx, what);
        pair.wheel(cx, 3.);
        pair.patched.draw_plain(cx);
        pair.assert_same(cx, &format!("{what}, wheeled"));
        assert_eq!(pair.patched.state.scroll.offset().y, pair.whole.state.scroll.offset().y, "{what}: the offset");
    }
}
