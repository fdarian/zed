//! Patches that lay out again (see [`super::patch`]): a root that is `patchable_relayout` in a window
//! that allows it may be given other nodes, styles and children than its recording, and is
//! accepted when nothing outside it moved.

use crate::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, InteractiveElement, IntoElement, ParentElement, PatchCounters,
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
    /// How many times the unit was built.
    builds: Cell<usize>,
    /// The patcher finishes the patch that was prepared, and does not build the unit again.
    direct: Cell<bool>,
    /// The boxes of the unit's kids and of the header and footer report their bounds as they paint.
    track: Cell<bool>,
    kid_bounds: RefCell<Vec<Bounds<Pixels>>>,
    /// The user key of the unit does not move with the content, so that a frame replays the recording it left.
    key_stays: Cell<bool>,
    /// How many times the owner republished what the unit's listeners report, and what it republished.
    replays: Cell<usize>,
    republished: RefCell<Vec<Bounds<Pixels>>>,
    outside_bounds: RefCell<Vec<Bounds<Pixels>>>,
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
            builds: Cell::new(0),
            direct: Cell::new(true),
            track: Cell::new(false),
            kid_bounds: RefCell::new(Vec::new()),
            key_stays: Cell::new(false),
            replays: Cell::new(0),
            republished: RefCell::new(Vec::new()),
            outside_bounds: RefCell::new(vec![Bounds::default(); 2]),
        })
    }

    fn change(&self, content: Content) {
        self.content.set(content);
        self.version.set(self.version.get() + 1);
    }

    fn unit(self: &Rc<Self>) -> AnyElement {
        self.builds.set(self.builds.get() + 1);
        let mut unit = div().id(UNIT).h(px(self.unit_height.get())).bg(blue()).overflow_hidden();
        unit = match self.unit_width.get() {
            Some(width) => unit.w(px(width)).flex_none(),
            None => unit.flex_none(),
        };
        match self.content.get() {
            Content::Kids { count, height } => {
                self.kid_bounds.borrow_mut().resize(count, Bounds::default());
                for index in 0..count {
                    let mut kid = div().id(("kid", index)).h(px(height)).flex_none().bg(red()).hover(|style| style.bg(green()));
                    if self.track.get() {
                        let state = self.clone();
                        kid = kid.on_painted(move |bounds, _, _| state.kid_bounds.borrow_mut()[index] = bounds);
                    }
                    unit = unit.child(kid);
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
        let mut unit = retained(UNIT, 1_000 + if state.key_stays.get() { 0 } else { state.version.get() }, move |_, _| build.unit());
        unit = if state.relayout.get() { unit.patchable_relayout() } else { unit.patchable() };
        if state.track.get() {
            // Listeners that report to their owner keep a subtree from being recorded unless the owner reports for it.
            let owner = state.clone();
            unit = unit.on_replay(move |_, _, _| {
                owner.replays.set(owner.replays.get() + 1);
                *owner.republished.borrow_mut() = owner.kid_bounds.borrow().clone();
            });
        }
        let page = div().id("page").w(px(400.)).h(px(300.));
        let page = if state.block.get() {
            page
        } else if state.row.get() {
            page.flex().flex_row()
        } else {
            page.flex().flex_col()
        };
        let outside = |name: &'static str, at: usize| {
            let mut outside = div().id(name).w(px(60.)).h(px(20.)).flex_none().bg(green());
            if state.track.get() {
                let state = state.clone();
                outside = outside.on_painted(move |bounds, _, _| state.outside_bounds.borrow_mut()[at] = bounds);
            }
            outside
        };
        page.child(outside("header", 0)).child(unit).child(outside("footer", 1)).into_any_element()
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
                let outcome = if patched.direct.get() && window.prepared_patch_units().is_some() {
                    window.finish_prepared_patch(cx, |_, _| true)
                } else {
                    let element = patched.unit();
                    window.repaint_in_place(vec![(ElementId::from(UNIT), element)], cx, |_, _| true)
                };
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
fn the_recording_of_a_root_that_laid_out_again_is_patched_again_with_no_frame_built_between(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, State::new(kids(3, 20.)));
    for (height, what) in [(25., "first"), (30., "second"), (35., "third")] {
        let before = page.patches(cx);
        page.state.change(kids(3, height));
        page.tick(cx);
        let after = page.patches(cx);
        assert_eq!(after.relayout_patches, before.relayout_patches + 1, "{what}: {:?}", page.state.seen.borrow());
        assert_eq!(after.rewritten_recordings, before.rewritten_recordings + 1, "{what}");
        assert_eq!(after.declines(PatchDecline::StaleRecording), 0, "{what}: {:?}", page.state.seen.borrow());
    }
    page.assert_whole(cx, "a frame built whole after three patches in a row");
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

/// A patch that is prepared ahead of the draw (`Window::prepare_patch`): a read in between is answered by
/// it, and the draw finishes it without building or laying out again.
mod prepared {
    use super::*;

    impl Fixture {
        fn prepare(&self, cx: &mut TestAppContext) -> Result<(), PatchDecline> {
            let state = self.state.clone();
            cx.update_window(self.window, |_, window, cx| {
                window.prepare_patch(cx, move |_, _| vec![(ElementId::from(UNIT), state.unit())])
            })
            .unwrap()
        }

        fn prepared(&self, cx: &mut TestAppContext) -> Option<Vec<ElementId>> {
            cx.update_window(self.window, |_, window, _| window.prepared_patch_units()).unwrap()
        }

        /// Whether the window holds a prepared patch, current or not.
        fn holds_prepared(&self, cx: &mut TestAppContext) -> bool {
            cx.update_window(self.window, |_, window, _| window.patch.prepared.is_some()).unwrap()
        }

        fn layout_passes(&self, cx: &mut TestAppContext) -> u64 {
            cx.update_window(self.window, |_, window, _| window.layout_pass_count()).unwrap()
        }

        /// A read of the layout, as the embedder makes it after a commit.
        fn read(&self, cx: &mut TestAppContext) -> bool {
            cx.update_window(self.window, |_, window, cx| window.draw_if_dirty(cx)).unwrap()
        }

        fn kid_bounds(&self) -> Vec<Bounds<Pixels>> {
            self.state.kid_bounds.borrow().clone()
        }

        fn outside_bounds(&self) -> Vec<Bounds<Pixels>> {
            self.state.outside_bounds.borrow().clone()
        }

        /// A page, with the unit's bounds reported, that patches as the test says.
        fn tracked(cx: &mut TestAppContext, content: Content) -> Self {
            let state = State::new(content);
            state.track.set(true);
            Self::new(cx, state)
        }

        /// The commit whose frame is drawn by the next tick: nothing is requested, as a test window draws a
        /// dirty window when an update ends.
        fn commit(&self, _: &mut TestAppContext, content: Content) {
            self.state.change(content);
        }
    }

    #[gpui::test]
    fn a_prepared_patch_shows_the_frame_that_repaint_in_place_alone_does_and_builds_and_lays_out_once(cx: &mut TestAppContext) {
        let prepared = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.direct.set(false);
        let (builds, runs) = (prepared.state.builds.get(), prepared.layout_runs(cx));
        let (control_builds, control_runs) = (control.state.builds.get(), control.layout_runs(cx));

        prepared.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        prepared.prepare(cx).expect("the commit stays inside the root");
        assert_eq!(prepared.state.builds.get() - builds, 1, "the root is built once to be prepared");
        let after_prepare = prepared.layout_runs(cx);
        assert!(after_prepare > runs, "the preparation laid out");
        assert_eq!(prepared.prepared(cx), Some(vec![ElementId::from(UNIT)]));
        assert_eq!(prepared.patches(cx).patched_frames, 0, "nothing was drawn yet");

        prepared.tick(cx);
        control.tick(cx);
        assert_eq!(prepared.state.builds.get() - builds, 1, "the draw builds nothing: {:?}", prepared.state.seen.borrow());
        assert_eq!(prepared.layout_runs(cx), after_prepare, "the draw does not lay out again");
        // Laid out once by the preparation, as the control lays out once by its patch.
        assert_eq!(after_prepare - runs, control.layout_runs(cx) - control_runs);
        assert_eq!(control.state.builds.get() - control_builds, 1, "the control builds the root for its patch");
        let after = prepared.patches(cx);
        assert_eq!(after.patched_frames, 1, "{:?}", prepared.state.seen.borrow());
        assert_eq!(after.relayout_patches, 1);
        assert_eq!(after.total_declines(), 0);
        assert_eq!(after, control.patches(cx));
        assert!(!prepared.holds_prepared(cx), "the draw took it");
        assert_same(&prepared.snapshot(cx), &control.snapshot(cx), "the frame the draw shows");
        assert_eq!(prepared.unit_bounds(cx), control.unit_bounds(cx));
        prepared.assert_whole(cx, "the frame built whole after a prepared patch");
    }

    #[gpui::test]
    fn a_repaint_in_place_of_the_roots_that_were_prepared_finishes_the_patch(cx: &mut TestAppContext) {
        let prepared = Fixture::tracked(cx, kids(3, 20.));
        prepared.state.direct.set(false);
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.direct.set(false);
        let runs = prepared.layout_runs(cx);
        prepared.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        prepared.prepare(cx).unwrap();
        let after_prepare = prepared.layout_runs(cx);
        prepared.tick(cx);
        control.tick(cx);
        assert_eq!(prepared.layout_runs(cx), after_prepare, "laid out once: {:?}", prepared.state.seen.borrow());
        assert!(after_prepare > runs);
        assert_eq!(prepared.patches(cx).relayout_patches, 1);
        assert_same(&prepared.snapshot(cx), &control.snapshot(cx), "the frame the draw shows");
    }

    #[gpui::test]
    fn a_read_between_the_preparation_and_the_draw_is_answered_by_the_post_commit_layout(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let (before, outside) = (page.kid_bounds(), page.outside_bounds());
        assert_eq!(before.len(), 3);

        page.commit(cx, kids(3, 25.));
        let passes = page.layout_passes(cx);
        page.prepare(cx).unwrap();
        assert!(!page.read(cx), "a read runs no pass: the patch that was prepared answers");
        assert_eq!(page.layout_passes(cx), passes);
        let read = page.kid_bounds();
        assert_ne!(read, before, "the boxes inside the root report the layout after the commit");
        assert_eq!(read[1].size.height, px(25.));
        assert_eq!(read[2].origin.y, before[2].origin.y + px(10.), "the third box moved by what the two before it grew");
        assert_eq!(page.outside_bounds(), outside, "the header and footer answer as before");
        assert_eq!(page.layout_passes(cx), passes);

        page.tick(cx);
        assert_eq!(page.kid_bounds(), read, "what the draw reports is what the preparation did");
        assert_eq!(page.outside_bounds(), outside);
        assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
    }

    #[gpui::test]
    fn the_extent_of_a_scroller_inside_the_root_is_the_prepared_one_before_the_draw(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, Content::Scroller { count: 4, height: 50. });
        assert_eq!(page.state.scroll.max_offset().y, px(100.));
        page.commit(cx, Content::Scroller { count: 4, height: 60. });
        page.prepare(cx).unwrap();
        assert_eq!(page.state.scroll.max_offset().y, px(140.), "the scroller has its new extent when it is read");
        page.tick(cx);
        assert_eq!(page.state.scroll.max_offset().y, px(140.));
        assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
        page.assert_whole(cx, "a scroller that holds more");
    }

    #[gpui::test]
    fn another_invalidation_discards_the_prepared_patch_and_the_draw_is_whole_and_correct(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.patcher.set(false);
        page.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        page.prepare(cx).unwrap();
        assert!(page.holds_prepared(cx));

        // A test window draws what is dirty when the update ends: this draw is built whole.
        for fixture in [&page, &control] {
            cx.update_window(fixture.window, |_, window, _| window.refresh()).unwrap();
        }
        assert!(!page.holds_prepared(cx), "the draw discarded it");
        let after = page.patches(cx);
        assert_eq!(after.patched_frames, 0, "the frame was built whole");
        assert_eq!(after.relayout_patches, 0);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame built after the discard");
        assert_eq!(page.unit_bounds(cx), control.unit_bounds(cx));

        // And what follows is as it is for a window that never prepared.
        for fixture in [&page, &control] {
            fixture.state.change(kids(5, 15.));
            fixture.tick(cx);
        }
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the next tick");
        for fixture in [&page, &control] {
            fixture.draw_whole(cx);
        }
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the next whole frame");
    }

    #[gpui::test]
    fn a_view_notified_after_the_preparation_discards_it(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.patcher.set(false);
        page.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        page.prepare(cx).unwrap();
        assert!(page.holds_prepared(cx));
        for fixture in [&page, &control] {
            cx.update_window(fixture.window, |view, _, cx| cx.notify(view.entity_id())).unwrap();
        }
        assert!(!page.holds_prepared(cx), "the draw the notify made discarded it");
        assert_eq!(page.patches(cx).patched_frames, 0);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame built after the notify");
        assert_eq!(page.unit_bounds(cx), control.unit_bounds(cx));
    }

    #[gpui::test]
    fn a_draw_that_patches_other_roots_or_none_discards_the_prepared_patch(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.patcher.set(false);
        page.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        page.prepare(cx).unwrap();
        // The embedder's patcher does not patch this tick.
        page.state.patcher.set(false);
        page.tick(cx);
        control.tick(cx);
        assert!(!page.holds_prepared(cx));
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame built whole");

        // Discarding by hand leaves the same.
        page.state.patcher.set(true);
        control.state.patcher.set(true);
        for fixture in [&page, &control] {
            fixture.commit(cx, kids(3, 30.));
        }
        page.prepare(cx).unwrap();
        cx.update_window(page.window, |_, window, cx| window.discard_prepared_patch(cx)).unwrap();
        assert!(!page.holds_prepared(cx));
        page.tick(cx);
        control.tick(cx);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame after a discard by hand");
        page.draw_whole(cx);
        control.draw_whole(cx);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the next whole frame");
    }

    #[gpui::test]
    fn a_patch_made_after_a_prepared_one_was_discarded_is_made_from_the_recording_that_was_on_screen(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.direct.set(false);
        // The preparation laid the root out for 25, and the content that is drawn is another, and then the one on screen.
        for (discarded, drawn) in [(kids(3, 25.), kids(4, 22.)), (kids(2, 30.), kids(3, 20.))] {
            page.commit(cx, discarded);
            page.prepare(cx).unwrap();
            cx.update_window(page.window, |_, window, cx| window.discard_prepared_patch(cx)).unwrap();
            assert!(!page.holds_prepared(cx));
            page.commit(cx, drawn);
            control.commit(cx, drawn);
            let before = page.patches(cx);
            page.tick(cx);
            control.tick(cx);
            let after = page.patches(cx);
            assert_eq!(after.declines(PatchDecline::StaleRecording), 0, "{:?}", page.state.seen.borrow());
            assert_eq!(after.patched_frames - before.patched_frames, 1, "{:?}", page.state.seen.borrow());
            assert_eq!(after.relayout_patches - before.relayout_patches, 1);
            assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame patched after a discard");
            assert_eq!(page.unit_bounds(cx), control.unit_bounds(cx));
            assert_eq!(page.kid_bounds(), control.kid_bounds(), "the listeners reported the same");
        }
        page.assert_whole(cx, "a frame built whole after the patches");
    }

    #[gpui::test]
    fn a_second_preparation_after_a_discard_is_made_from_the_recording_that_was_on_screen(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.direct.set(false);
        page.commit(cx, kids(3, 25.));
        page.prepare(cx).unwrap();
        cx.update_window(page.window, |_, window, cx| window.discard_prepared_patch(cx)).unwrap();
        page.commit(cx, kids(5, 12.));
        control.commit(cx, kids(5, 12.));
        page.prepare(cx).expect("the recording of the frame on screen is still there to patch");
        assert_eq!(page.prepared(cx), Some(vec![ElementId::from(UNIT)]));
        page.tick(cx);
        control.tick(cx);
        assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
        assert_eq!(page.patches(cx).total_declines(), 0);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame the second preparation drew");
        page.assert_whole(cx, "a frame built whole after it");
    }

    #[gpui::test]
    fn a_whole_frame_after_a_discard_leaves_nothing_of_it_to_patch_from(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.patcher.set(false);
        page.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        page.prepare(cx).unwrap();
        cx.update_window(page.window, |_, window, cx| window.discard_prepared_patch(cx)).unwrap();
        page.draw_whole(cx);
        control.draw_whole(cx);
        // The frame on screen is another: what follows is as it is for a window that never prepared.
        for fixture in [&page, &control] {
            fixture.commit(cx, kids(4, 18.));
            fixture.tick(cx);
        }
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame after the whole frame");
        page.assert_whole(cx, "a frame built whole after it");
    }

    #[gpui::test]
    fn a_prepare_that_declines_not_contained_leaves_the_next_whole_frame_as_if_it_had_not_tried(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.patcher.set(false);
        for fixture in [&page, &control] {
            fixture.state.unit_height.set(140.);
            fixture.commit(cx, kids(4, 20.));
        }
        assert_eq!(page.prepare(cx), Err(PatchDecline::NotContained));
        assert_eq!(page.prepared(cx), None);
        assert!(!page.holds_prepared(cx), "a decline leaves nothing behind");
        assert_eq!(page.patches(cx).declines(PatchDecline::NotContained), 1);
        let passes = page.layout_passes(cx);
        // The embedder lays out as it would have.
        assert!(page.read(cx) || page.layout_passes(cx) == passes);

        page.tick(cx);
        control.tick(cx);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame drawn after the decline");
        page.draw_whole(cx);
        control.draw_whole(cx);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the next whole frame");
        assert_eq!(page.unit_bounds(cx), control.unit_bounds(cx));
    }

    #[gpui::test]
    fn a_prepared_patch_whose_root_gains_and_loses_boxes_is_spliced_when_it_is_finished(cx: &mut TestAppContext) {
        let page = Fixture::new(cx, State::new(kids(3, 20.)));
        let control = Fixture::new(cx, State::new(kids(3, 20.)));
        control.state.direct.set(false);
        for (count, height) in [(7, 10.), (1, 30.), (5, 12.)] {
            page.commit(cx, kids(count, height));
            control.commit(cx, kids(count, height));
            page.prepare(cx).unwrap();
            let runs = page.layout_runs(cx);
            page.tick(cx);
            control.tick(cx);
            assert_eq!(page.layout_runs(cx), runs, "{count} kids: the draw lays out nothing");
            assert_eq!(page.patches(cx).relayout_patches, control.patches(cx).relayout_patches, "{count} kids: {:?}", page.state.seen.borrow());
            assert_same(&page.snapshot(cx), &control.snapshot(cx), &format!("{count} kids"));
        }
        assert_eq!(page.patches(cx).relayout_patches, 3);
        page.assert_whole(cx, "a frame built whole after three prepared patches with other counts");
    }

    #[gpui::test]
    fn a_second_preparation_takes_over_from_the_layout_the_first_left(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.direct.set(false);
        for (count, height) in [(4, 25.), (4, 20.)] {
            page.commit(cx, kids(count, height));
            page.prepare(cx).unwrap();
        }
        // The second commit is the one that is drawn; it equals the recording's content for a root
        // whose boxes were laid out for the first.
        control.commit(cx, kids(4, 20.));
        page.tick(cx);
        control.tick(cx);
        assert_eq!(page.patches(cx).relayout_patches, 1, "{:?}", page.state.seen.borrow());
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame of the second commit");
        page.assert_whole(cx, "a frame built whole after it");
    }

    #[gpui::test]
    fn a_patch_for_other_roots_than_the_prepared_ones_is_made_from_what_was_prepared_for(cx: &mut TestAppContext) {
        let page = Fixture::tracked(cx, kids(3, 20.));
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.direct.set(false);
        page.commit(cx, kids(3, 25.));
        control.commit(cx, kids(3, 25.));
        page.prepare(cx).unwrap();
        // The patcher builds the unit and one that does not exist, which no patch can name.
        page.state.direct.set(false);
        cx.update_window(page.window, |_, window, cx| {
            let outcome = window.repaint_in_place(
                vec![(ElementId::from(UNIT), page.state.unit()), (ElementId::from("nothing"), div().into_any_element())],
                cx,
                |_, _| true,
            );
            assert_eq!(outcome.map(|_| ()), Err(PatchDecline::UnknownUnit));
        })
        .unwrap();
        assert!(!page.holds_prepared(cx), "asking for other roots discards it");
        page.state.patcher.set(false);
        control.state.patcher.set(false);
        page.tick(cx);
        control.tick(cx);
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "the frame built whole");
    }

    #[gpui::test]
    fn a_window_that_does_not_allow_it_declines_the_preparation(cx: &mut TestAppContext) {
        let page = Fixture::with(cx, State::new(kids(3, 20.)), false);
        page.state.change(kids(3, 25.));
        assert_eq!(page.prepare(cx), Err(PatchDecline::LayoutMismatch));
        assert!(!page.holds_prepared(cx));
        assert_eq!(page.patches(cx).declines(PatchDecline::LayoutMismatch), 1);
        page.tick(cx);
        page.assert_whole(cx, "a frame built whole after the decline");
    }

    #[gpui::test]
    fn a_root_that_publishes_and_has_a_replay_hook_is_rewritten_by_three_patches_in_a_row_and_republishes_what_they_published(
        cx: &mut TestAppContext,
    ) {
        let state = State::new(kids(3, 20.));
        state.track.set(true);
        state.key_stays.set(true);
        let page = Fixture::new(cx, state);
        let control = Fixture::tracked(cx, kids(3, 20.));
        control.state.patcher.set(false);
        let first = page.kid_bounds();

        for (index, height) in [25., 30., 35.].into_iter().enumerate() {
            page.commit(cx, kids(3, height));
            control.commit(cx, kids(3, height));
            page.tick(cx);
            control.tick(cx);
            let after = page.patches(cx);
            assert_eq!(after.relayout_patches, index as u64 + 1, "{height}: {:?}", page.state.seen.borrow());
            assert_eq!(after.rewritten_recordings, index as u64 + 1, "{height}: the recording is rewritten though the root published");
            assert_eq!(after.declines(PatchDecline::StaleRecording), 0, "{height}: {:?}", page.state.seen.borrow());
            assert_same(&page.snapshot(cx), &control.snapshot(cx), &format!("a patch of height {height}"));
            assert_eq!(page.kid_bounds(), control.kid_bounds(), "{height}: the listeners reported the same");
        }
        assert_eq!(page.patches(cx).rewritten_recordings, 3);

        // A frame that replays the root republishes what the patches published.
        let replays = page.state.replays.get();
        for fixture in [&page, &control] {
            cx.update_window(fixture.window, |view, _, cx| cx.notify(view.entity_id())).unwrap();
        }
        assert!(page.state.replays.get() > replays, "the root was replayed, from the recording the patch left");
        assert_ne!(*page.state.republished.borrow(), first, "what is republished is not what the first frame published");
        assert_eq!(*page.state.republished.borrow(), control.kid_bounds());
        assert_same(&page.snapshot(cx), &control.snapshot(cx), "a frame that replays the root after three patches");
        page.assert_whole(cx, "a frame built whole after them");
    }
}
