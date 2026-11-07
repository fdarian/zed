//! Patches that lay out again (see [`super::patch`]): a root that is `patchable_relayout` in a window
//! that allows it may be given other nodes, styles and children than its recording, and is
//! accepted when nothing outside it moved.

use crate::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, ElementId, InteractiveElement, IntoElement, ParentElement, PatchCounters,
    PatchDecline, Pixels, Render, ScrollHandle, StatefulInteractiveElement, Styled, TestAppContext, Window, blue, div, green, point, px, red,
    retained, size,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const UNIT: &str = "unit";

#[derive(Clone, Copy, PartialEq)]
enum Content {
    /// Boxes of a height each, which grow and shrink.
    Kids { count: usize, height: f32 },
    /// A text in a box of a width, which wraps.
    Text { text: &'static str, width: f32 },
    /// A scroller of boxes.
    Scroller { count: usize, height: f32 },
}

/// What the test changes between frames: the page and the patcher both build the unit from it.
struct State {
    content: Cell<Content>,
    unit_width: Cell<Option<f32>>,
    unit_height: Cell<f32>,
    /// What the key of the unit is, which moves when the state does: a frame built whole builds it.
    version: Cell<u64>,
    scroll: ScrollHandle,
    /// The page is a flex row, with the unit as a flex item of automatic width.
    row: Cell<bool>,
    /// The page is a block, with the unit a box of a fixed size that clips: a relayout boundary.
    block: Cell<bool>,
    relayout: Cell<bool>,
    patcher: Cell<bool>,
    seen: RefCell<Vec<PatchOutcomeLog>>,
}

type PatchOutcomeLog = Result<(), PatchDecline>;

impl State {
    fn new(content: Content) -> Rc<Self> {
        Rc::new(Self {
            content: Cell::new(content),
            unit_width: Cell::new(Some(120.)),
            unit_height: Cell::new(100.),
            version: Cell::new(0),
            scroll: ScrollHandle::new(),
            row: Cell::new(false),
            block: Cell::new(false),
            relayout: Cell::new(true),
            patcher: Cell::new(true),
            seen: RefCell::new(Vec::new()),
        })
    }

    fn change(&self, content: Content) {
        self.content.set(content);
        self.version.set(self.version.get() + 1);
    }

    fn unit(&self) -> AnyElement {
        let mut unit = div().id(UNIT).h(px(self.unit_height.get())).bg(blue()).overflow_hidden();
        unit = match self.unit_width.get() {
            Some(width) => unit.w(px(width)).flex_none(),
            None => unit.flex_none(),
        };
        match self.content.get() {
            Content::Kids { count, height } => {
                for index in 0..count {
                    unit = unit.child(
                        div().id(("kid", index)).h(px(height)).flex_none().bg(red()).hover(|style| style.bg(green())),
                    );
                }
            }
            Content::Text { text, width } => {
                unit = unit.child(div().id("text").w(px(width)).bg(red()).hover(|style| style.bg(green())).child(text));
            }
            Content::Scroller { count, height } => {
                unit = unit.overflow_y_scroll().track_scroll(&self.scroll);
                for index in 0..count {
                    unit = unit.child(div().id(("kid", index)).h(px(height)).flex_none().bg(red()));
                }
            }
        }
        unit.into_any_element()
    }
}

struct Page(Rc<State>);

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let state = self.0.clone();
        let build = state.clone();
        let mut unit = retained(UNIT, 1_000 + state.version.get(), move |_, _| build.unit());
        unit = if state.relayout.get() { unit.patchable_relayout() } else { unit.patchable() };
        let page = div().id("page").w(px(400.)).h(px(300.));
        let page = if state.block.get() {
            page
        } else if state.row.get() {
            page.flex().flex_row()
        } else {
            page.flex().flex_col()
        };
        page.child(div().id("header").w(px(60.)).h(px(20.)).flex_none().bg(green()))
            .child(unit)
            .child(div().id("footer").w(px(60.)).h(px(20.)).flex_none().bg(green()))
            .into_any_element()
    }
}

struct Fixture {
    window: AnyWindowHandle,
    state: Rc<State>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, state: Rc<State>) -> Self {
        Self::with(cx, state, true)
    }

    fn with(cx: &mut TestAppContext, state: Rc<State>, contained: bool) -> Self {
        let window = cx.open_window(size(px(400.), px(300.)), {
            let state = state.clone();
            move |_, _| Page(state)
        });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_contained_patches(contained);
            let patched = state.clone();
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                if !patched.patcher.get() {
                    return false;
                }
                let element = patched.unit();
                let outcome = window.repaint_in_place(vec![(ElementId::from(UNIT), element)], cx, |_, _| true);
                patched.seen.borrow_mut().push(outcome.as_ref().map(|_| ()).map_err(|decline| *decline));
                outcome.is_ok()
            })));
            // The second frame replays the unit, which registers it again.
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

    fn patches(&self, cx: &mut TestAppContext) -> PatchCounters {
        cx.update_window(self.window, |_, window, _| window.patch_counters()).unwrap()
    }

    fn layout_runs(&self, cx: &mut TestAppContext) -> u64 {
        cx.update_window(self.window, |_, window, _| window.layout_engine().runs()).unwrap()
    }

    /// What the rendered frame paints and answers.
    fn snapshot(&self, cx: &mut TestAppContext) -> String {
        let text = cx.update_window(self.window, |_, window, _| {
            let scene = &window.rendered_frame.scene;
            let hitboxes: Vec<String> = window.rendered_frame.hitboxes.iter().map(|hitbox| hitbox.describe()).collect();
            format!(
                "{} operations\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{} groups\n{:#?}\n{:?}",
                scene.len(),
                scene.shadows,
                scene.quads,
                scene.paths,
                scene.underlines,
                scene.monochrome_sprites,
                scene.subpixel_sprites,
                scene.polychrome_sprites,
                scene.groups.len(),
                hitboxes,
                window
                    .rendered_frame
                    .cursor_styles
                    .iter()
                    .map(|request| {
                        let at = request.hitbox_id.and_then(|id| window.rendered_frame.hitboxes.iter().position(|hitbox| hitbox.id == id));
                        (at, request.style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap();
        without_view_ids(&text)
    }

    /// The unit's boxes as the layout of the frame that is rendered has them, by element id.
    fn unit_bounds(&self, cx: &mut TestAppContext) -> Vec<(String, crate::Bounds<Pixels>)> {
        cx.update_window(self.window, |_, window, _| {
            let mut bounds: Vec<_> = window
                .rendered_frame
                .debug_bounds
                .iter()
                .map(|(name, bounds)| (name.clone(), *bounds))
                .collect();
            bounds.sort_by(|left, right| left.0.cmp(&right.0));
            bounds
        })
        .unwrap()
    }

    /// Asserts that the patch the last tick made, if it did, left the frame as one built whole does.
    #[track_caller]
    fn assert_whole(&self, cx: &mut TestAppContext, what: &str) {
        let patched = self.snapshot(cx);
        self.draw_whole(cx);
        assert_same(&patched, &self.snapshot(cx), what);
    }
}

/// `text` without what two windows do not share: the ids of the entities, and the hashes of element
/// ids, which hold them.
fn without_view_ids(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let entity = rest.find("EntityId(");
        let hash = rest.find("hash: ");
        let (at, skip_to) = match (entity, hash) {
            (Some(entity), Some(hash)) if entity < hash => (entity, ')'),
            (_, Some(hash)) => (hash, ' '),
            (Some(entity), None) => (entity, ')'),
            (None, None) => break,
        };
        let is_hash = skip_to == ' ';
        out.push_str(&rest[..at]);
        let from = if is_hash { at + "hash: ".len() } else { at };
        let end = if is_hash {
            rest[from..].find(|c: char| !c.is_ascii_digit()).expect("a hash ends")
        } else {
            rest[from..].find(')').expect("an entity id is closed") + 1
        };
        rest = &rest[from + end..];
    }
    out.push_str(rest);
    out
}

#[track_caller]
fn assert_same(patched: &str, whole: &str, what: &str) {
    if patched == whole {
        return;
    }
    let line = patched.lines().zip(whole.lines()).position(|(patched, whole)| patched != whole).unwrap_or(0);
    let around = |text: &str| text.lines().skip(line.saturating_sub(12)).take(24).collect::<Vec<_>>().join("\n");
    panic!("{what}: differ at line {line}\npatched:\n{}\nwhole:\n{}", around(patched), around(whole));
}

fn kids(count: usize, height: f32) -> Content {
    Content::Kids { count, height }
}

#[gpui::test]
fn a_box_that_grows_inside_a_root_of_a_fixed_size_is_patched(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    let before = page.patches(cx);
    let runs = page.layout_runs(cx);

    page.state.change(kids(3, 25.));
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.patched_frames - before.patched_frames, 1, "the tick is patched: {:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches - before.relayout_patches, 1);
    assert_eq!(after.total_declines(), 0);
    assert!(page.layout_runs(cx) > runs, "the patch laid out");
    page.assert_whole(cx, "a box that grew");

    // And back.
    page.state.change(kids(3, 20.));
    page.draw_whole(cx);
    page.draw_whole(cx);
    page.state.change(kids(3, 15.));
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 2, "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "a box that shrank");
}

#[gpui::test]
fn the_hitboxes_of_the_shown_frame_have_the_new_bounds_after_a_patch(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    page.state.change(kids(3, 25.));
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
    let patched = cx
        .update_window(page.window, |_, window, _| {
            window.rendered_frame.hitboxes.iter().map(|hitbox| (hitbox.id, hitbox.bounds)).collect::<Vec<_>>()
        })
        .unwrap();
    page.draw_whole(cx);
    let whole = cx
        .update_window(page.window, |_, window, _| {
            window.rendered_frame.hitboxes.iter().map(|hitbox| (hitbox.id, hitbox.bounds)).collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(
        patched.iter().map(|(_, bounds)| *bounds).collect::<Vec<_>>(),
        whole.iter().map(|(_, bounds)| *bounds).collect::<Vec<_>>()
    );
    assert!(!patched.is_empty());
}

#[gpui::test]
fn a_text_that_rewraps_inside_a_root_of_a_fixed_size_is_patched(cx: &mut TestAppContext) {
    let wide = Content::Text { text: "aaaa bbbb cccc dddd", width: 100. };
    let narrow = Content::Text { text: "aaaa bbbb cccc dddd", width: 60. };
    let page = Fixture::new(cx, State::new(wide));
    let before = page.patches(cx);

    page.state.change(narrow);
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.relayout_patches - before.relayout_patches, 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.total_declines(), 0);
    page.assert_whole(cx, "a text that wrapped");
}

#[gpui::test]
fn a_root_that_is_a_boundary_of_the_layout_is_patched(cx: &mut TestAppContext) {
    let state = State::new(kids(3, 20.));
    state.block.set(true);
    let page = Fixture::new(cx, state);
    page.state.change(kids(3, 30.));
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "a boundary that grew inside");
}

#[gpui::test]
fn a_root_whose_box_would_change_is_declined_and_the_frame_is_built_whole(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    page.state.unit_height.set(120.);
    page.state.change(kids(3, 20.));
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::NotContained), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches, 0);
    assert_eq!(after.patched_frames, 0);

    let control = Fixture::new(cx, State::new(kids(3, 20.)));
    control.state.unit_height.set(120.);
    control.state.change(kids(3, 20.));
    control.state.patcher.set(false);
    control.tick(cx);
    assert_same(&page.snapshot(cx), &control.snapshot(cx), "a declined patch leaves the frame a frame built whole would");
    assert_eq!(page.unit_bounds(cx), control.unit_bounds(cx));
}

#[gpui::test]
fn a_decline_after_the_layout_ran_leaves_the_next_frames_as_if_it_had_not_tried(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    let control = Fixture::new(cx, State::new(kids(3, 20.)));
    control.state.patcher.set(false);
    for fixture in [&page, &control] {
        fixture.state.unit_height.set(140.);
        fixture.state.change(kids(4, 20.));
        fixture.tick(cx);
    }
    assert_eq!(page.patches(cx).declines(PatchDecline::NotContained), 1, "{:?}", page.state.seen.borrow());
    assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame drawn after the decline");

    // The frames after it are the same too, whole or ticked.
    for fixture in [&page, &control] {
        fixture.state.unit_height.set(100.);
        fixture.state.change(kids(2, 30.));
        fixture.tick(cx);
    }
    assert_same(&page.snapshot(cx), &control.snapshot(cx), "the next tick");
    for fixture in [&page, &control] {
        fixture.draw_whole(cx);
    }
    assert_same(&page.snapshot(cx), &control.snapshot(cx), "the next whole frame");
}

#[gpui::test]
fn a_flex_item_of_automatic_basis_whose_content_changes_moves_its_sibling_and_is_declined(cx: &mut TestAppContext) {
    let state = State::new(kids(1, 20.));
    state.row.set(true);
    state.unit_width.set(None);
    // The unit is as wide as its content: a box of a width in it.
    let page = Fixture::new(cx, state);
    let before = page.patches(cx);
    page.state.change(Content::Text { text: "aaaa", width: 80. });
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::NotContained) - before.declines(PatchDecline::NotContained), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches, 0);
    page.assert_whole(cx, "a frame built whole after the decline");
}

#[gpui::test]
fn a_scroller_root_measures_its_extent_again_when_its_content_changes(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(Content::Scroller { count: 4, height: 50. }));
    let range = page.state.scroll.max_offset().y;
    assert_eq!(range, px(100.));

    page.state.change(Content::Scroller { count: 4, height: 60. });
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.relayout_patches, 1, "{:?}", page.state.seen.borrow());
    assert_eq!(page.state.scroll.max_offset().y, px(140.), "the range follows the content");
    page.assert_whole(cx, "a scroller that holds more");
}

#[gpui::test]
fn a_scroller_root_whose_offset_clamps_is_patched_as_a_frame_built_whole_is_drawn(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(Content::Scroller { count: 6, height: 50. }));
    cx.update_window(page.window, |_, window, _| {
        page.state.scroll.set_offset(point(px(0.), px(-200.)));
        window.refresh();
    })
    .unwrap();
    page.draw_whole(cx);
    page.draw_whole(cx);
    assert_eq!(page.state.scroll.offset().y, px(-200.));

    page.state.change(Content::Scroller { count: 6, height: 30. });
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.relayout_patches, 1, "{:?}", page.state.seen.borrow());
    assert_eq!(page.state.scroll.offset().y, px(-80.), "the offset follows the range it can have");
    page.assert_whole(cx, "a scroller whose offset clamped");
    assert_eq!(page.state.scroll.offset().y, px(-80.));
}

#[gpui::test]
fn a_window_that_does_not_allow_it_declines_a_relayout_as_before(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, State::new(kids(3, 20.)), false);
    page.state.change(kids(3, 25.));
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::LayoutMismatch), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches, 0);
    assert_eq!(after.patched_frames, 0);
    assert_eq!(after.declines(PatchDecline::NotContained), 0);
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "a frame built whole after the decline");
}

#[gpui::test]
fn a_root_that_does_not_say_it_may_lay_out_again_declines_a_relayout_as_before(cx: &mut TestAppContext) {
    let state = State::new(kids(3, 20.));
    state.relayout.set(false);
    let page = Fixture::new(cx, state);
    page.state.change(kids(3, 25.));
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::LayoutMismatch), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches, 0);
}

#[gpui::test]
fn the_recording_of_a_root_that_laid_out_again_is_not_patched_until_a_frame_records_it(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    page.state.change(kids(3, 25.));
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());

    page.state.change(kids(3, 30.));
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::StaleRecording), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches, 1);
    page.assert_whole(cx, "a frame built whole after the stale recording");

    // The frame that was built whole recorded it again; the next frame replays it and registers it.
    page.draw_whole(cx);
    page.state.change(kids(3, 35.));
    page.tick(cx);
    assert_eq!(page.patches(cx).relayout_patches, 2, "{:?}", page.state.seen.borrow());
    page.assert_whole(cx, "patched again once a whole frame recorded it");
}

#[gpui::test]
fn a_window_with_an_accessibility_tree_declines_a_relayout(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    cx.update_window(page.window, |_, window, _| window.set_a11y_active_for_tests(true)).unwrap();
    page.draw_whole(cx);
    page.draw_whole(cx);
    page.state.change(kids(3, 25.));
    page.tick(cx);
    let after = page.patches(cx);
    assert_eq!(after.declines(PatchDecline::LayoutMismatch), 1, "{:?}", page.state.seen.borrow());
    assert_eq!(after.relayout_patches, 0);
}
