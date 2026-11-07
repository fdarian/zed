//! A list scroll that changes which rows are shown is still a translate (see [`super::scroll_patch`]):
//! the rows that stay are moved, the rows that leave are taken out of the frame, and the rows that come
//! in are built as the list builds them, in a frame of scratch, and put into the frame at their place.
//! The frame it leaves is the frame built whole: what it paints, what the pointer hits, which listener a
//! click reaches, the tab order and the dispatch tree. A row that cannot be built so declines the
//! translate, and the frame is built whole.

use crate::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, ElementId, EntityId, FocusHandle, InteractiveElement,
    IntoElement, ListAlignment, ListState, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, PatchDecline, PlatformInput, Point, Render, ScrollApplyDecline, ScrollDelta, ScrollPatchCause,
    ScrollPatchReason, ScrollWheelEvent, Styled, TestAppContext, Window, blue, deferred,
    div, green, list, point, px, red, retained, size,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

const ROWS: usize = 200;
const UNIT: &str = "unit";
/// Where the list is, under a unit of its own that patches.
const LIST_TOP: f32 = 100.;
const LIST_H: f32 = 500.;
const LIST_W: f32 = 120.;
/// Where the pointer rests: over the middle of the list, which no step of a few rows reaches.
const POINTER: (f32, f32) = (60., LIST_TOP + 250.);
/// The rows with a focus handle.
const FOCUSABLE: usize = 10;

/// The rows have different heights, 20 to 44.
fn height(ix: usize) -> f32 {
    20. + (ix * 5 % 7) as f32 * 4.
}

/// How far the top of row `ix` is below the top of the first.
fn top(ix: usize) -> f32 {
    (0..ix).map(height).sum()
}

struct Shared {
    state: ListState,
    key: Cell<u64>,
    log: RefCell<Vec<String>>,
    /// The rows the list asked its builder for, in order.
    asked: RefCell<Vec<usize>>,
    focus: Vec<FocusHandle>,
    green: RefCell<BTreeSet<usize>>,
    /// A row the builder makes 10 taller than the list measured it.
    taller: Cell<Option<usize>>,
    /// A row that defers the draw of a child.
    deferred: Cell<Option<usize>>,
    kids: Cell<usize>,
    /// The key of the unit, which moves when what it holds does.
    version: Cell<u64>,
    patch_unit: Cell<bool>,
    patch_row: Cell<Option<usize>>,
    seen: RefCell<Vec<Result<(), PatchDecline>>>,
}

impl Shared {
    fn new(cx: &mut TestAppContext, measure_all: bool) -> Rc<Self> {
        let focus = cx.update(|cx| (0..ROWS / FOCUSABLE).map(|_| cx.focus_handle().tab_stop(true)).collect());
        let state = ListState::new(ROWS, ListAlignment::Top, px(0.));
        Rc::new(Self {
            // A row that is not measured has the height of a hint, which is what the list scrolls by.
            state: if measure_all { state.measure_all() } else { state.with_uniform_item_height(px(32.)) },
            key: Cell::new(1),
            log: RefCell::new(Vec::new()),
            asked: RefCell::new(Vec::new()),
            focus,
            green: RefCell::new(BTreeSet::new()),
            taller: Cell::new(None),
            deferred: Cell::new(None),
            kids: Cell::new(3),
            version: Cell::new(0),
            patch_unit: Cell::new(false),
            patch_row: Cell::new(None),
            seen: RefCell::new(Vec::new()),
        })
    }

    fn handle(&self, ix: usize) -> Option<&FocusHandle> {
        (ix % FOCUSABLE == 3).then(|| &self.focus[ix / FOCUSABLE])
    }

    /// What a row is, as the subtree of its retained root.
    fn row_inner(self: &Rc<Self>, ix: usize) -> AnyElement {
        let log = self.clone();
        let extra = if self.taller.get() == Some(ix) { 10. } else { 0. };
        let mut row = div()
            .id(("row", ix))
            .w_full()
            .h(px(height(ix) + extra))
            .bg(if self.green.borrow().contains(&ix) { green() } else { blue() })
            .always_hitbox()
            .on_mouse_down(MouseButton::Left, move |_, _, _| log.log.borrow_mut().push(format!("down {ix}")));
        if let Some(handle) = self.handle(ix) {
            let keys = self.clone();
            row = row
                .track_focus(handle)
                .tab_stop(true)
                .on_key_down(move |event, _, _| keys.log.borrow_mut().push(format!("row {ix} key {}", event.keystroke.key)));
        }
        if self.deferred.get() == Some(ix) {
            row = row.child(deferred(div().id(("tip", ix)).w(px(10.)).h(px(10.)).bg(red())));
        }
        row.into_any_element()
    }

    fn row(self: &Rc<Self>, ix: usize) -> AnyElement {
        let build = self.clone();
        retained(("row", ix), self.key.get(), move |_, _| build.row_inner(ix))
            .relocatable(px(5000.))
            .patchable()
            .into_any_element()
    }

    fn unit(self: &Rc<Self>) -> AnyElement {
        let mut unit = div().id(UNIT).w(px(LIST_W)).h(px(LIST_TOP)).flex_none().overflow_hidden().bg(blue());
        for kid in 0..self.kids.get() {
            let log = self.clone();
            unit = unit.child(
                div()
                    .id(("kid", kid))
                    .h(px(10.))
                    .flex_none()
                    .bg(red())
                    .always_hitbox()
                    .on_mouse_down(MouseButton::Left, move |_, _, _| log.log.borrow_mut().push(format!("kid {kid}"))),
            );
        }
        unit.into_any_element()
    }
}

struct Page(Rc<Shared>);

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let shared = self.0.clone();
        let build = shared.clone();
        let unit = retained(UNIT, 1_000 + shared.version.get(), move |_, _| build.unit()).patchable_relayout();
        let builder = shared.clone();
        let rows = list(shared.state.clone(), move |ix, _, _| {
            builder.asked.borrow_mut().push(ix);
            builder.row(ix)
        })
        .w(px(LIST_W))
        .h(px(LIST_H));
        div()
            .id("page")
            .w(px(400.))
            .h(px(LIST_TOP + LIST_H + 20.))
            .flex()
            .flex_col()
            .child(unit)
            .child(rows)
            .child(div().id("footer").w(px(60.)).h(px(20.)).flex_none().bg(green()))
    }
}

struct Fixture {
    window: AnyWindowHandle,
    view: EntityId,
    shared: Rc<Shared>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, patch: bool) -> Self {
        let shared = Shared::new(cx, true);
        Self::open(cx, patch, shared)
    }

    fn open(cx: &mut TestAppContext, patch: bool, shared: Rc<Shared>) -> Self {
        let window = cx.open_window(size(px(400.), px(LIST_TOP + LIST_H + 20.)), {
            let shared = shared.clone();
            move |_, _| Page(shared)
        });
        let window = AnyWindowHandle::from(window);
        let view = cx
            .update_window(window, |view, window, cx| {
                window.set_dom_hit_testing(true);
                window.set_contained_patches(true);
                window.set_scroll_patch(patch);
                window.set_scroll_patch_gate(Some(Rc::new(|_, _| None)));
                // The rows have no hover style and no pointer event: where the pointer is among them does not matter.
                window.set_scroll_patch_hover(Some(Rc::new(|_, _, _| false)));
                let patched = shared.clone();
                window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                    let mut units = Vec::new();
                    if patched.patch_unit.get() {
                        units.push((ElementId::from(UNIT), patched.unit()));
                    }
                    if let Some(row) = patched.patch_row.get() {
                        units.push((ElementId::from(("row", row)), patched.row_inner(row)));
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
                view.entity_id()
            })
            .unwrap();
        let fixture = Self { window, view, shared };
        fixture.point_at(cx, POINTER);
        fixture.draw(cx);
        fixture
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    }

    fn point_at(&self, cx: &mut TestAppContext, at: (f32, f32)) {
        let at = point(px(at.0), px(at.1));
        cx.update_window(self.window, |_, window, cx| {
            let moved = MouseMoveEvent { position: at, pressed_button: None, modifiers: Default::default() };
            window.dispatch_event(PlatformInput::MouseMove(moved), cx);
        })
        .unwrap();
        self.draw(cx);
    }

    /// A wheel event at the pointer that moves the rows up by `by`: the frame it draws is the frame shown.
    fn wheel(&self, cx: &mut TestAppContext, by: f32) {
        cx.update_window(self.window, |_, window, cx| {
            let event = ScrollWheelEvent {
                position: point(px(POINTER.0), px(POINTER.1)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
    }

    /// Writes the offset of the list as an embedder does, without a wheel, and says it scrolled: the frame
    /// it draws is the frame shown.
    fn write_offset(&self, cx: &mut TestAppContext, to: f32) {
        let state = self.shared.state.clone();
        let view = self.view;
        cx.update_window(self.window, move |_, window, cx| {
            state.set_offset_from_scrollbar(point(px(0.), px(-to)));
            window.note_list_scrolled(view);
            cx.notify(view);
        })
        .unwrap();
    }

    /// Writes the offset as an embedder that scrolls with no notification does: the list is said to have
    /// scrolled, and nothing else is invalidated.
    fn write_offset_quietly(&self, cx: &mut TestAppContext, to: f32) {
        let state = self.shared.state.clone();
        let view = self.view;
        cx.update_window(self.window, move |_, window, _| {
            state.set_offset_from_scrollbar(point(px(0.), px(-to)));
            window.note_list_scrolled(view);
        })
        .unwrap();
    }

    /// How many frames this window has swapped in: a patch and a translate swap none.
    fn built_whole(&self, cx: &mut TestAppContext) -> u64 {
        self.with_window(cx, |window, _| window.rendered_serial())
    }

    fn scrolled(&self) -> f32 {
        f32::from(self.shared.state.scrolled())
    }

    fn rows(&self, cx: &mut TestAppContext) -> std::ops::Range<usize> {
        cx.update_window(self.window, |_, window, _| window.scroll_patch_rows()[0].clone()).unwrap()
    }

    /// The scroll position that has `count` rows more at the bottom than the rows shown, one pixel of the last.
    fn bringing_in_below(&self, cx: &mut TestAppContext, count: usize) -> f32 {
        let rows = self.rows(cx);
        top(rows.end + count - 1) - LIST_H + 1.
    }

    /// The scroll position that has `count` rows more at the top, one pixel of the first.
    fn bringing_in_above(&self, cx: &mut TestAppContext, count: usize) -> f32 {
        let rows = self.rows(cx);
        top(rows.start - count + 1) - 1.
    }

    fn scroll_to(&self, cx: &mut TestAppContext, to: f32) {
        self.wheel(cx, to - self.scrolled());
    }

    fn counters(&self, cx: &mut TestAppContext) -> crate::ScrollPatchCounters {
        cx.update_window(self.window, |_, window, _| window.scroll_patch_counters()).unwrap()
    }

    fn with_window<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.update_window(self.window, |_, window, cx| f(window, cx)).unwrap()
    }

    fn click(window: &mut Window, at: Point<crate::Pixels>, cx: &mut App) {
        let down = MouseDownEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1, first_mouse: false };
        window.dispatch_event(PlatformInput::MouseDown(down), cx);
        let up = MouseUpEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1 };
        window.dispatch_event(PlatformInput::MouseUp(up), cx);
    }

    /// What the rendered frame is and answers with no event sent to the window: the frame, the places the
    /// pointer would hit, the tab order and the dispatch tree. Not equal across windows until
    /// [`same_window`] has the identities out of it.
    fn observe(&self, cx: &mut TestAppContext) -> String {
        let shared = self.shared.clone();
        cx.update_window(self.window, |_, window, _| {
            let mut text = frame_text(window);
            if let Err(wrong) = window.check_scroll_runs() {
                text.push_str(&format!("\nthe runs are wrong: {wrong}"));
            }
            for y in [5., 60., 105., 150., 250., 330., 400., 495., 530.] {
                let at = point(px(60.), px(y + if y < 520. { LIST_TOP } else { 0. }));
                let hits: Vec<String> = window.rendered_frame.hit_test(at, true).ids.iter().map(|id| name_of(window, *id)).collect();
                text.push_str(&format!("\n{at:?}: hits {hits:?}"));
            }
            let tabs = &window.rendered_frame.tab_stops;
            let label = |handle: &FocusHandle| shared.focus.iter().position(|focusable| focusable.id == handle.id);
            let mut forward = Vec::new();
            let mut at = tabs.next(None);
            for _ in 0..24 {
                let Some(handle) = at else { break };
                forward.push(label(&handle));
                at = tabs.next(Some(&handle.id));
            }
            let mut backward = Vec::new();
            let mut at = tabs.prev(None);
            for _ in 0..24 {
                let Some(handle) = at else { break };
                backward.push(label(&handle));
                at = tabs.prev(Some(&handle.id));
            }
            text.push_str(&format!("\ntab order {forward:?} back {backward:?}"));
            same_window(&text)
        })
        .unwrap()
    }

    /// Which listener a click at some points of the list reaches, and the pointer back where it rests.
    fn clicks(&self, cx: &mut TestAppContext) -> String {
        let shared = self.shared.clone();
        let out = cx.update_window(self.window, |_, window, cx| {
            let mut out = Vec::new();
            for y in [10., 90., 160., 260., 330., 410., 490.] {
                let at = point(px(60.), px(LIST_TOP + y));
                shared.log.borrow_mut().clear();
                window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: at, pressed_button: None, modifiers: Default::default() }), cx);
                Self::click(window, at, cx);
                out.push(format!("{at:?} click {:?}", shared.log.borrow()));
            }
            // And in the unit above the list.
            let at = point(px(60.), px(5.));
            shared.log.borrow_mut().clear();
            Self::click(window, at, cx);
            out.push(format!("{at:?} click {:?}", shared.log.borrow()));
            out.join("\n")
        })
        .unwrap();
        self.point_at(cx, POINTER);
        out
    }

    /// Settles the frame shown as a frame built whole: the rows are recorded again from where they are.
    fn draw_whole(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    /// A tick, which patches if it can.
    fn tick(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.request_tick();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }
}

/// What the rendered frame is, as a frame being built is described, and what holds the rows apart.
fn frame_text(window: &mut Window) -> String {
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
    let cursors: Vec<_> = frame
        .cursor_styles
        .iter()
        .map(|request| (request.hitbox_id.map(name), request.style))
        .collect();
    format!(
        "{described}\n{primitives}\ncursors {cursors:?}\ntooltips {}\nhitbox count {}\ndeferred {}",
        frame.tooltip_requests.len(),
        frame.hitboxes.len(),
        frame.deferred_draws.len()
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

/// `text` without what two windows of one test do not share: the entity of the view, the focus handles and
/// the hashes of element ids hold them.
fn same_window(text: &str) -> String {
    let mut out = text.to_owned();
    for marker in ["EntityId(", "FocusId(", "hash: "] {
        let mut stripped = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(at) = rest.find(marker) {
            stripped.push_str(&rest[..at + marker.len()]);
            rest = &rest[at + marker.len()..];
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
            rest = &rest[end..];
        }
        stripped.push_str(rest);
        out = stripped;
    }
    out
}

#[track_caller]
fn assert_same(translated: &str, built: &str, what: &str) {
    assert!(translated == built, "{what}\n{}", super::retained::first_difference(translated, built));
}

/// A step of two windows that do the same, the one translating and the other building every frame whole.
struct Pair {
    translated: Fixture,
    built: Fixture,
}

impl Pair {
    fn new(cx: &mut TestAppContext) -> Self {
        Self { translated: Fixture::new(cx, true), built: Fixture::new(cx, false) }
    }

    #[track_caller]
    fn same(&self, cx: &mut TestAppContext, what: &str) {
        let (translated, built) = (self.translated.observe(cx), self.built.observe(cx));
        assert_same(&translated, &built, what);
    }

    /// Both windows scroll to `to`.
    fn scroll_to(&self, cx: &mut TestAppContext, to: f32) {
        self.translated.scroll_to(cx, to);
        self.built.scroll_to(cx, to);
    }
}

fn no_declines(counters: &crate::ScrollPatchCounters) {
    for decline in ScrollApplyDecline::ALL {
        assert_eq!(counters.apply_declines(decline), 0, "{decline:?}: {counters:?}");
    }
}

#[gpui::test]
fn rows_that_come_in_at_the_bottom_and_then_at_the_top_are_translates(cx: &mut TestAppContext) {
    for count in [1, 2, 5] {
        let pair = Pair::new(cx);
        pair.same(cx, "before any scroll");
        let rows = pair.translated.rows(cx);
        assert_eq!(rows.start, 0, "{rows:?}");

        // Down: `count` rows come in at the bottom, and rows leave at the top, until there are rows to come in above.
        let mut frames = 0;
        let mut step = 0;
        while pair.translated.rows(cx).start < 4 * count + 1 {
            step += 1;
            let to = pair.translated.bringing_in_below(cx, count);
            let before = pair.translated.rows(cx);
            pair.scroll_to(cx, to);
            frames += 1;
            let after = pair.translated.rows(cx);
            assert_eq!(after.end - before.end, count, "step {step} down by {count}: {before:?} to {after:?}");
            let counters = pair.translated.counters(cx);
            assert_eq!((counters.frames, counters.patched, counters.membership_frames), (frames, frames, frames), "{counters:?}");
            assert_eq!(counters.rows_built, (frames as usize * count) as u64, "{counters:?}");
            assert_eq!(counters.mispredicted, 0, "{counters:?}");
            no_declines(&counters);
            pair.same(cx, &format!("down by {count} rows, step {step}"));
        }
        assert!(pair.translated.rows(cx).start > 0);

        // Back up: `count` rows come in at the top.
        for step in 1..=3 {
            let to = pair.translated.bringing_in_above(cx, count);
            let before = pair.translated.rows(cx);
            pair.scroll_to(cx, to);
            frames += 1;
            let after = pair.translated.rows(cx);
            assert_eq!(before.start - after.start, count, "step {step} up by {count}: {before:?} to {after:?}");
            let counters = pair.translated.counters(cx);
            assert_eq!((counters.frames, counters.patched, counters.membership_frames), (frames, frames, frames), "{counters:?}");
            no_declines(&counters);
            pair.same(cx, &format!("up by {count} rows, step {step}"));
        }
        let counters = pair.translated.counters(cx);
        assert!(counters.rows_removed >= count as u64, "{counters:?}");
        assert_eq!(pair.built.counters(cx).frames, 0);
    }
}

#[gpui::test]
fn a_click_and_the_hit_test_reach_the_rows_a_translate_put_in(cx: &mut TestAppContext) {
    let translated = Fixture::new(cx, true);
    let built = Fixture::new(cx, false);
    for fixture in [&translated, &built] {
        let to = top(7) + 5.;
        fixture.scroll_to(cx, to);
    }
    let counters = translated.counters(cx);
    assert_eq!((counters.frames, counters.patched), (1, 1), "{counters:?}");
    assert!(counters.rows_built >= 1, "{counters:?}");
    // The rows 3 and 13 have focus handles: row 3 has left, row 13 is below.
    assert_same(&translated.observe(cx), &built.observe(cx), "after the scroll");
    let (a, b) = (translated.clicks(cx), built.clicks(cx));
    assert_eq!(a, b, "which listener a click reaches");
    assert!(a.contains("down"), "a click reached a row: {a}");
}

#[gpui::test]
fn the_tab_order_goes_through_the_rows_that_came_in(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    // Rows with a focus handle are every ten from 3: scroll until some come in and others have left.
    for _ in 0..8 {
        let to = pair.translated.bringing_in_below(cx, 5);
        pair.scroll_to(cx, to);
    }
    let counters = pair.translated.counters(cx);
    assert!(counters.membership_frames >= 1, "{counters:?}");
    pair.same(cx, "tab order and dispatch tree after rows came in and left");
    let observed = pair.translated.observe(cx);
    assert!(observed.contains("tab order [Some("), "a row that came in has a tab stop: {}", observed.lines().last().unwrap());
}

#[gpui::test]
fn a_programmatic_offset_write_through_the_entry_point_is_the_same_translate(cx: &mut TestAppContext) {
    let translated = Fixture::new(cx, true);
    let built = Fixture::new(cx, false);
    // As a wheel without one: only the offset is written and the list is said to have scrolled.
    for count in [1, 2, 5] {
        let to = translated.bringing_in_below(cx, count);
        translated.write_offset(cx, to);
        built.write_offset(cx, to);
        let counters = translated.counters(cx);
        assert_eq!(counters.patched, counters.frames, "{counters:?}");
        assert!(counters.membership_frames >= 1, "{counters:?}");
        no_declines(&counters);
        assert_same(&translated.observe(cx), &built.observe(cx), &format!("a write that brings in {count} rows"));
    }
    let to = translated.bringing_in_above(cx, 2);
    translated.write_offset(cx, to);
    built.write_offset(cx, to);
    assert_same(&translated.observe(cx), &built.observe(cx), "a write that brings in rows at the top");

    // Without the entry point the write is no scroll the window knows of: a frame is built whole.
    let before = translated.counters(cx).frames;
    let state = translated.shared.state.clone();
    let view = translated.view;
    let to = translated.bringing_in_below(cx, 1);
    translated.with_window(cx, move |_, cx| {
        state.set_offset_from_scrollbar(point(px(0.), px(-to)));
        cx.notify(view);
    });
    assert_eq!(translated.counters(cx).frames, before);
}

#[gpui::test]
fn an_entering_row_with_no_measured_size_is_a_membership_change_and_is_drawn_whole(cx: &mut TestAppContext) {
    let open = |cx: &mut TestAppContext, patch: bool| {
        let shared = Shared::new(cx, false);
        Fixture::open(cx, patch, shared)
    };
    let (translated, built) = (open(cx, true), open(cx, false));
    assert_same(&translated.observe(cx), &built.observe(cx), "before any scroll");
    let to = translated.scrolled() + 60.;
    translated.scroll_to(cx, to);
    built.scroll_to(cx, to);
    let counters = translated.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched), (1, 0, 0), "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Membership), 1, "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::Unmeasured), 1, "{counters:?}");
    assert_same(&translated.observe(cx), &built.observe(cx), "the frame built whole");
}

#[gpui::test]
fn an_entering_row_that_is_not_the_height_it_was_measured_at_declines_and_is_drawn_whole(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let rows = pair.translated.rows(cx);
    let entering = rows.end;
    for fixture in [&pair.translated, &pair.built] {
        fixture.shared.taller.set(Some(entering));
    }
    let to = pair.translated.bringing_in_below(cx, 1);
    pair.scroll_to(cx, to);
    let counters = pair.translated.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 0), "{counters:?}");
    assert_eq!(counters.apply_declines(ScrollApplyDecline::Height), 1, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    pair.same(cx, "a row that has another height than it was measured at");
    // The state of the window is as a frame built whole leaves it: a later translate works.
    let to = pair.translated.bringing_in_below(cx, 1);
    pair.scroll_to(cx, to);
    pair.same(cx, "the frame after");
}

#[gpui::test]
fn a_row_that_defers_a_draw_declines_and_is_drawn_whole(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let entering = pair.translated.rows(cx).end;
    for fixture in [&pair.translated, &pair.built] {
        fixture.shared.deferred.set(Some(entering));
        fixture.shared.key.set(2);
        fixture.draw_whole(cx);
        fixture.draw(cx);
    }
    let before = pair.translated.counters(cx);
    let to = pair.translated.bringing_in_below(cx, 1);
    pair.scroll_to(cx, to);
    let counters = pair.translated.counters(cx);
    assert_eq!(counters.frames - before.frames, 1, "{counters:?}");
    assert_eq!(counters.apply_declines(ScrollApplyDecline::Effects), 1, "{counters:?}");
    pair.same(cx, "a row that defers a draw");
}

#[gpui::test]
fn a_row_that_comes_in_under_the_pointer_is_drawn_whole_and_says_why(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    // The rows come in at the bottom, where the pointer is.
    for fixture in [&pair.translated, &pair.built] {
        fixture.point_at(cx, (60., LIST_TOP + LIST_H - 4.));
    }
    let before = pair.translated.counters(cx);
    let to = pair.translated.bringing_in_below(cx, 2);
    for fixture in [&pair.translated, &pair.built] {
        let by = to - fixture.scrolled();
        cx.update_window(fixture.window, |_, window, cx| {
            let event = ScrollWheelEvent {
                position: point(px(60.), px(LIST_TOP + LIST_H - 4.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
    }
    let counters = pair.translated.counters(cx);
    assert_eq!(counters.frames - before.frames, 1, "{counters:?}");
    assert_eq!(counters.apply_declines(ScrollApplyDecline::Pointer), 1, "{counters:?}");
    pair.same(cx, "a row under the pointer");
}

#[gpui::test]
fn a_translate_with_rows_is_followed_by_a_relayout_patch_of_the_root_before_the_list(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let to = pair.translated.bringing_in_below(cx, 2);
    pair.scroll_to(cx, to);
    let to = pair.translated.bringing_in_below(cx, 3);
    pair.scroll_to(cx, to);
    pair.same(cx, "after the translates");
    let counters = pair.translated.counters(cx);
    assert_eq!((counters.patched, counters.membership_frames), (2, 2), "{counters:?}");

    // The unit before the list holds more boxes: its part of the frame grows, and the rows after it move in
    // every vector with the recordings of the rows that came in.
    for kids in [7, 1, 5] {
        for fixture in [&pair.translated, &pair.built] {
            fixture.shared.kids.set(kids);
            fixture.shared.version.set(fixture.shared.version.get() + 1);
        }
        pair.translated.shared.patch_unit.set(true);
        pair.translated.tick(cx);
        pair.translated.shared.patch_unit.set(false);
        pair.built.draw_whole(cx);
        let patches = pair.translated.with_window(cx, |window, _| window.patch_counters());
        assert!(patches.relayout_patches >= 1, "{patches:?}");
        assert_eq!(pair.translated.shared.seen.borrow().last(), Some(&Ok(())), "{:?}", pair.translated.shared.seen);
        pair.same(cx, &format!("a unit with {kids} boxes"));
        // And the rows are translated from the frame the patch left.
        let to = pair.translated.bringing_in_below(cx, 1);
        pair.scroll_to(cx, to);
        pair.same(cx, &format!("a translate after the patch of {kids} boxes"));
    }
    let counters = pair.translated.counters(cx);
    assert!(counters.patched >= 3, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
}

#[gpui::test]
fn a_row_that_came_in_is_patched_for_a_restyle(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let to = pair.translated.bringing_in_below(cx, 2);
    pair.scroll_to(cx, to);
    let counters = pair.translated.counters(cx);
    assert_eq!((counters.patched, counters.membership_frames), (1, 1), "{counters:?}");
    let entered = pair.translated.rows(cx).end - 1;
    for fixture in [&pair.translated, &pair.built] {
        fixture.shared.green.borrow_mut().insert(entered);
    }
    pair.translated.shared.patch_row.set(Some(entered));
    pair.translated.tick(cx);
    pair.translated.shared.patch_row.set(None);
    pair.built.draw_whole(cx);
    let patches = pair.translated.with_window(cx, |window, _| window.patch_counters());
    assert_eq!(pair.translated.shared.seen.borrow().last(), Some(&Ok(())), "{:?} {patches:?}", pair.translated.shared.seen);
    assert_eq!(patches.patched_units, 1, "{patches:?}");
    pair.same(cx, "a row that came in, restyled in place");
}

#[gpui::test]
fn fifty_steps_up_and_down_are_the_frames_built_whole_under_the_verifier(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    pair.translated.with_window(cx, |window, _| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
    });
    let counts = [1, 2, 5, 3, 1, 4];
    let mut down = true;
    let mut changed = 0;
    for step in 0..50 {
        let count = counts[step % counts.len()];
        if step % 9 == 0 {
            down = !down;
        }
        let rows = pair.translated.rows(cx);
        let to = if down || rows.start < count {
            pair.translated.bringing_in_below(cx, count)
        } else {
            pair.translated.bringing_in_above(cx, count)
        };
        pair.scroll_to(cx, to);
        pair.same(cx, &format!("step {step}"));
        changed += 1;
    }
    assert_eq!(changed, 50);
    let counters = pair.translated.counters(cx);
    assert!(counters.patched >= 15, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    pair.translated.with_window(cx, |window, _| {
        assert_eq!(window.retention_mismatches(), &[] as &[String]);
        assert!(window.retained_counters().verified >= 15);
    });
}

#[gpui::test]
fn fifty_steps_up_and_down_in_a_row_of_translates_are_the_frames_built_whole(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let counts = [1, 2, 5, 3, 1, 4];
    let mut down = true;
    for step in 0..50 {
        let count = counts[step % counts.len()];
        if step % 9 == 0 {
            down = !down;
        }
        let rows = pair.translated.rows(cx);
        let to = if down || rows.start < count {
            pair.translated.bringing_in_below(cx, count)
        } else {
            pair.translated.bringing_in_above(cx, count)
        };
        pair.scroll_to(cx, to);
        pair.same(cx, &format!("step {step}"));
    }
    let counters = pair.translated.counters(cx);
    assert_eq!(counters.patched, counters.frames, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    no_declines(&counters);
    // After all that the rows are put from their recordings by a frame built whole, and it is the same.
    pair.translated.draw_whole(cx);
    pair.built.draw_whole(cx);
    pair.same(cx, "the frame built whole after the translates");
    assert_eq!(pair.translated.shared.asked.borrow().len() > 0, true);
}

#[gpui::test]
fn a_jump_that_keeps_none_of_the_rows_is_a_membership_change_and_is_drawn_whole(cx: &mut TestAppContext) {
    let translated = Fixture::new(cx, true);
    let built = Fixture::new(cx, false);
    let to = top(100);
    translated.write_offset(cx, to);
    built.write_offset(cx, to);
    let counters = translated.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched), (1, 0, 0), "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Membership), 1, "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::Disjoint), 1, "{counters:?}");
    assert_same(&translated.observe(cx), &built.observe(cx), "the frame built whole after a jump");
    // And the next scroll is a translate from that frame.
    let to = translated.bringing_in_below(cx, 2);
    translated.scroll_to(cx, to);
    built.scroll_to(cx, to);
    let counters = translated.counters(cx);
    assert_eq!((counters.frames, counters.patched), (2, 1), "{counters:?}");
    assert_same(&translated.observe(cx), &built.observe(cx), "the translate after the jump");
}

/// The two windows of a step that restyles rows and scrolls the list: the one patches the rows and translates
/// the list in one draw, the other builds the frame whole.
impl Pair {
    /// Restyles the rows `rows` (green or back) and writes the offset `to` in one draw of the window that patches.
    /// `to` of `None` restyles without scrolling.
    fn restyle_and_scroll(&self, cx: &mut TestAppContext, rows: &[usize], green: bool, to: Option<f32>) {
        for fixture in [&self.translated, &self.built] {
            for row in rows {
                let mut set = fixture.shared.green.borrow_mut();
                if green { set.insert(*row) } else { set.remove(row) };
            }
        }
        self.translated.shared.patch_row.set(None);
        let patched = rows.first().copied();
        self.translated.shared.patch_row.set(patched);
        assert!(rows.len() <= 1, "the patcher of this fixture repaints one row");
        // One update, so that nothing draws between the tick and the write.
        let (state, view) = (self.translated.shared.state.clone(), self.translated.view);
        cx.update_window(self.translated.window, move |_, window, cx| {
            window.request_tick();
            if let Some(to) = to {
                state.set_offset_from_scrollbar(point(px(0.), px(-to)));
                window.note_list_scrolled(view);
            }
            window.draw(cx).clear(cx);
        })
        .unwrap();
        self.translated.shared.patch_row.set(None);

        cx.update_window(self.built.window, |_, window, _| window.refresh()).unwrap();
        match to {
            Some(to) => self.built.write_offset(cx, to),
            None => {}
        }
        self.built.draw(cx);
    }
}

fn visible_row(fixture: &Fixture, cx: &mut TestAppContext, nth: usize) -> usize {
    fixture.rows(cx).start + nth
}

#[gpui::test]
fn a_restyle_and_a_scroll_in_one_draw_are_a_patch_then_a_translate(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    // The rows are moved once first, so that the row restyled is one that is put elsewhere than it was recorded.
    let first = pair.translated.bringing_in_below(cx, 1);
    pair.scroll_to(cx, first);
    pair.same(cx, "after a first scroll");
    let row = visible_row(&pair.translated, cx, 3);
    let before = pair.translated.counters(cx);
    let serial = pair.translated.built_whole(cx);
    let rows = pair.translated.rows(cx);

    let to = pair.translated.scrolled() + 2.;
    pair.restyle_and_scroll(cx, &[row], true, Some(to));

    let counters = pair.translated.counters(cx);
    assert_eq!(pair.translated.rows(cx), rows, "no row comes in or leaves");
    assert_eq!(counters.patched - before.patched, 1, "{counters:?}");
    assert_eq!(counters.composed, 1, "{counters:?}");
    assert_eq!(counters.membership_frames, before.membership_frames, "{counters:?}");
    assert_eq!(pair.translated.built_whole(cx), serial, "no frame was built whole");
    let patches = pair.translated.with_window(cx, |window, _| window.patch_counters());
    assert_eq!((patches.patched_frames, patches.total_declines()), (1, 0), "{patches:?}");
    assert_eq!(pair.translated.shared.seen.borrow().last(), Some(&Ok(())));
    no_declines(&counters);
    pair.same(cx, "a restyle and a scroll of 2 pixels");
}

#[gpui::test]
fn a_restyle_of_a_row_that_stays_and_rows_that_come_in_are_one_patch_and_one_translate(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let row = visible_row(&pair.translated, cx, 2);
    for count in [1, 2] {
        let before = pair.translated.counters(cx);
        let serial = pair.translated.built_whole(cx);
        let to = pair.translated.bringing_in_below(cx, count);
        pair.restyle_and_scroll(cx, &[row], count == 1, Some(to));
        let counters = pair.translated.counters(cx);
        assert_eq!(counters.composed - before.composed, 1, "{counters:?} {:?}", pair.translated.shared.seen);
        assert_eq!(counters.membership_frames - before.membership_frames, 1, "{counters:?}");
        assert_eq!(pair.translated.built_whole(cx), serial, "no frame was built whole");
        no_declines(&counters);
        assert_eq!(pair.translated.shared.seen.borrow().last(), Some(&Ok(())));
        pair.same(cx, &format!("a restyle and {count} rows in"));
    }
    assert!(pair.translated.rows(cx).start == 0 || pair.translated.rows(cx).contains(&row), "the restyled row stays");
}

#[gpui::test]
fn a_relayout_patch_prepared_before_a_scroll_write_is_finished_and_the_rows_are_translated(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    for kids in [7, 2] {
        for fixture in [&pair.translated, &pair.built] {
            fixture.shared.kids.set(kids);
            fixture.shared.version.set(fixture.shared.version.get() + 1);
        }
        let before = pair.translated.counters(cx);
        let serial = pair.translated.built_whole(cx);
        let to = pair.translated.bringing_in_below(cx, 1);
        let (shared, state, view) = (pair.translated.shared.clone(), pair.translated.shared.state.clone(), pair.translated.view);
        // One update, so that nothing draws between the tick and the draw, as the work of a commit and its frame.
        let units = pair.translated.with_window(cx, move |window, cx| {
            window.request_tick();
            let patch = shared.clone();
            let prepared = window.prepare_patch(cx, move |_, _| vec![(ElementId::from(UNIT), patch.unit())]);
            assert_eq!(prepared, Ok(()));
            state.set_offset_from_scrollbar(point(px(0.), px(-to)));
            window.note_list_scrolled(view);
            let units = window.prepared_patch_units();
            shared.patch_unit.set(true);
            window.draw(cx).clear(cx);
            shared.patch_unit.set(false);
            units
        });
        assert_eq!(units, Some(vec![ElementId::from(UNIT)]), "a scroll write does not discard what was prepared");
        let counters = pair.translated.counters(cx);
        assert_eq!(counters.composed - before.composed, 1, "{counters:?}");
        assert_eq!(pair.translated.built_whole(cx), serial, "no frame was built whole");
        let patches = pair.translated.with_window(cx, |window, _| window.patch_counters());
        assert!(patches.relayout_patches >= 1 && patches.total_declines() == 0, "{patches:?}");
        no_declines(&counters);

        pair.built.write_offset(cx, to);
        pair.built.draw_whole(cx);
        pair.same(cx, &format!("a unit with {kids} boxes prepared before the scroll"));
    }
}

#[gpui::test]
fn a_restyle_then_a_translate_then_a_restyle_of_the_same_row_are_all_patches(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let row = visible_row(&pair.translated, cx, 3);
    let serial = pair.translated.built_whole(cx);
    pair.restyle_and_scroll(cx, &[row], true, None);
    pair.same(cx, "restyled in place");
    // A translate puts the row elsewhere than it was recorded, with the new paint.
    let to = pair.translated.scrolled() + 9.;
    pair.scroll_to(cx, to);
    pair.same(cx, "translated after the restyle");
    // And a restyle of the row where it is now patches it there.
    pair.restyle_and_scroll(cx, &[row], false, None);
    pair.same(cx, "restyled where it was put");
    let to = pair.translated.scrolled() + 9.;
    pair.restyle_and_scroll(cx, &[row], true, Some(to));
    pair.same(cx, "restyled and translated again");

    assert_eq!(pair.translated.built_whole(cx), serial, "no frame was built whole");
    let counters = pair.translated.counters(cx);
    assert_eq!(counters.composed, 1, "{counters:?}");
    assert_eq!(counters.patched, 2, "{counters:?}");
    no_declines(&counters);
    let patches = pair.translated.with_window(cx, |window, _| window.patch_counters());
    assert_eq!((patches.patched_frames, patches.total_declines()), (3, 0), "{patches:?}");
    assert!(patches.rewritten_recordings >= 3, "{patches:?}");
    // The frame built whole from the recordings the patches left is the same as well.
    pair.translated.draw_whole(cx);
    pair.built.draw_whole(cx);
    pair.same(cx, "the frame built whole after them");
}

#[gpui::test]
fn fifty_steps_of_restyles_and_scrolls_are_the_frames_built_whole_under_the_verifier(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    pair.translated.with_window(cx, |window, _| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
    });
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |bound: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % bound as u64) as usize
    };
    let mut composed_expected = 0;
    for step in 0..50 {
        let rows = pair.translated.rows(cx);
        let row = rows.start + next(4);
        let restyle = next(3) != 0;
        let scroll = next(3) != 0;
        let to = scroll.then(|| match next(3) {
            0 if rows.start > 2 => pair.translated.bringing_in_above(cx, 1 + next(2)),
            1 => pair.translated.bringing_in_below(cx, 1 + next(2)),
            _ => pair.translated.scrolled() + 3. + next(12) as f32,
        });
        let green = !pair.translated.shared.green.borrow().contains(&row);
        match (restyle, to) {
            (true, to) => {
                composed_expected += u64::from(to.is_some());
                pair.restyle_and_scroll(cx, &[row], green, to);
            }
            (false, Some(to)) => pair.scroll_to(cx, to),
            (false, None) => {}
        }
        pair.same(cx, &format!("step {step}"));
    }
    let counters = pair.translated.counters(cx);
    assert!(counters.composed >= 1, "{counters:?} of {composed_expected} steps that restyled and scrolled");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    pair.translated.with_window(cx, |window, _| {
        assert_eq!(window.retention_mismatches(), &[] as &[String]);
    });
}

#[gpui::test]
fn a_restyle_with_a_scroll_that_cannot_be_a_translate_is_the_patch_and_a_frame_built_whole(cx: &mut TestAppContext) {
    let pair = Pair::new(cx);
    let row = visible_row(&pair.translated, cx, 3);
    // A jump that keeps none of the rows: the translate declines, the patch has been made, and the frame built whole
    // after it draws the list from the offset written, with no view notified.
    pair.restyle_and_scroll(cx, &[row], true, Some(top(100)));
    let counters = pair.translated.counters(cx);
    assert_eq!((counters.frames, counters.patched, counters.composed), (1, 0, 0), "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::Disjoint), 1, "{counters:?}");
    pair.same(cx, "the frame built whole after a patch");
    // And the translate that follows is made from it.
    let to = pair.translated.bringing_in_below(cx, 1);
    pair.restyle_and_scroll(cx, &[row + 50], true, Some(to));
    pair.same(cx, "the translate after it");
}
