//! Patches that splice (see [`super::patch_splice`]): a root that lays out again and registers other
//! things than it did (more or fewer hitboxes, listeners, dispatch nodes, tab stops, operations)
//! replaces its part of the rendered frame, and the frame is the one built whole: what it paints, what
//! the pointer hits, which listener a click reaches, the tab order, where a key goes and the cursor. The
//! roots recorded after it, the nested ones it held and a scroll translate are right after it.

use crate::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, ElementId, FocusHandle, InteractiveElement, IntoElement,
    ListAlignment, ListState, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement, PatchCounters, PatchDecline,
    PlatformInput, Point, Render, ScrollDelta, ScrollWheelEvent, StatefulInteractiveElement, Styled, TestAppContext, Window, blue, div,
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
}

impl State {
    fn new(cx: &mut TestAppContext, kids: usize, list: bool) -> Rc<Self> {
        let focusable = cx.update(|cx| [cx.focus_handle(), cx.focus_handle()]);
        Rc::new(Self {
            kids: Cell::new(kids),
            version: Cell::new(0),
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
            .h(px(12.))
            .flex_none()
            .bg(red())
            .hover(|style| style.bg(green()))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, _, _| down.log.borrow_mut().push(format!("down {index}")));
        if index == 2 {
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
        let mut unit = div().id(UNIT).w(px(120.)).h(px(100.)).flex_none().overflow_hidden().bg(blue());
        if self.key_context.get() {
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
        let unit = retained(UNIT, 1_000 + state.version.get(), move |_, _| build.unit()).patchable_relayout();
        let sibling_state = state.clone();
        let sibling = retained(SIBLING, 2_000, move |_, _| sibling_state.sibling()).patchable();
        let mut page = div()
            .id("page")
            .key_context("Page")
            .w(px(400.))
            .h(px(400.))
            .flex()
            .flex_col()
            .child(div().id("header").w(px(60.)).h(px(20.)).flex_none().bg(green()))
            .child(unit)
            .child(sibling)
            .child(div().id("footer").w(px(60.)).h(px(20.)).flex_none().bg(green()));
        if let Some(rows) = &state.list {
            let rows = list(rows.clone(), move |index, _, _| {
                retained(("row", index), 3_000, move |_, _| {
                    div().id(("row", index)).w_full().h(px(30.)).bg(blue()).always_hitbox().into_any_element()
                })
                .relocatable(px(100.))
                .into_any_element()
            })
            .w(px(100.))
            .h(px(100.));
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
            let text = format!(
                "{described}\n{primitives}\nbounds {bounds:?}\ncursors {cursors:?}\ntooltips {}\nhitbox count {}",
                frame.tooltip_requests.len(),
                frame.hitboxes.len()
            );

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

fn name_of(window: &Window, id: crate::HitboxId) -> String {
    window
        .rendered_frame
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.id == id)
        .map_or_else(|| "(none)".to_owned(), |hitbox| format!("{:?}", hitbox.identity.as_ref().and_then(|identity| identity.ids.last())))
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
    // The two runs are two windows: what names a hitbox or a tab stop carries the window's view and its handles.
    let same_window = |frame: &str| {
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
    };

    assert_eq!(patched.patches(cx).relayout_patches, 1, "{:?}", patched.state.seen.borrow());
    assert!(counters.patched >= 1, "the rows are translated after the splice: {counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    assert_same(&same_window(&shown.frame), &same_window(&built.frame), "the rows translated after a splice");
}
