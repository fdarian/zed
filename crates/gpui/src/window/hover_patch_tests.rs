//! A patch of the rows of a list: a row is recorded to be put elsewhere when the list scrolls
//! (`retained(..).relocatable(..)`), and is patched while the frame has it where it was recorded. The
//! rows are a list of boxes whose colour a hover changes, built in the frame as the list asks for them
//! and built again by the patcher with the colour the pointer is giving them.

use crate::{
    AppContext as _, AnyElement, AnyWindowHandle, App, Context, ElementId, InteractiveElement, IntoElement, ListAlignment, ListState,
    ParentElement, PatchDecline, Render, ScrollDelta, ScrollWheelEvent, Styled, TestAppContext, Window, div, list,
    point, px, retained, size,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ROW: f32 = 30.;
const LIST: f32 = 100.;

/// What the rows are, as the frame builds them and the patcher builds them again.
#[derive(Default)]
struct Knobs {
    /// The row that has the hover, if any.
    hot: Cell<Option<usize>>,
    /// The key of the rows: another one builds them again.
    key: Cell<u64>,
    /// The rows that patchers are asked to repaint, by the next tick.
    patch: RefCell<Vec<usize>>,
    /// Whether the patcher builds the row with the colour of the hover, or as the frame did.
    painted: Cell<bool>,
}

fn row_element(ix: usize, hot: bool) -> AnyElement {
    let color = if hot { crate::red() } else { crate::blue() };
    div().id(("row", ix)).w_full().h(px(ROW)).bg(color).always_hitbox().into_any_element()
}

#[derive(Clone)]
struct Page {
    knobs: Rc<Knobs>,
    state: ListState,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let knobs = self.knobs.clone();
        div().size_full().child(
            list(self.state.clone(), move |ix, _, _| {
                let hot = knobs.hot.get() == Some(ix);
                retained(("row", ix), knobs.key.get() * 1_000 + u64::from(hot), move |_, _| row_element(ix, hot))
                    .relocatable(px(100.))
                    .patchable()
                    .into_any_element()
            })
            .w(px(100.))
            .h(px(LIST)),
        )
    }
}

struct Fixture {
    window: AnyWindowHandle,
    knobs: Rc<Knobs>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        let knobs = Rc::new(Knobs::default());
        knobs.key.set(1);
        let state = ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(ROW));
        let page = Page { knobs: knobs.clone(), state };
        let window = AnyWindowHandle::from(cx.open_window(size(px(400.), px(300.)), move |_, _| page));
        cx.update_window(window, |_, window, _| {
            window.set_dom_hit_testing(true);
            window.set_scroll_patch(true);
            window.set_scroll_patch_gate(Some(Rc::new(|_| None)));
            let knobs = knobs.clone();
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                let units = knobs
                    .patch
                    .borrow()
                    .iter()
                    .map(|&ix| (ElementId::from(("row", ix)), row_element(ix, knobs.painted.get())))
                    .collect();
                window.repaint_in_place(units, cx, |_, _| true).is_ok()
            })));
        })
        .unwrap();
        let fixture = Self { window, knobs };
        // The first frame builds and records the rows, the second puts them back.
        fixture.draw(cx);
        fixture.draw(cx);
        fixture
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    }

    /// Draws the tick that patches `rows` to the colour of the hover (or back).
    fn patch(&self, cx: &mut TestAppContext, rows: &[usize], painted: bool) {
        *self.knobs.patch.borrow_mut() = rows.to_vec();
        self.knobs.painted.set(painted);
        cx.update_window(self.window, |_, window, cx| {
            window.request_tick();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    /// Builds the rows with `hot` as the one under the pointer, in a frame built whole.
    fn whole(&self, cx: &mut TestAppContext, hot: Option<usize>) {
        self.knobs.hot.set(hot);
        cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    fn wheel(&self, cx: &mut TestAppContext, by: f32) {
        cx.update_window(self.window, |_, window, cx| {
            let event = ScrollWheelEvent {
                position: point(px(10.), px(10.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(crate::PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
        self.draw(cx);
    }

    fn patched(&self, cx: &mut TestAppContext) -> crate::PatchCounters {
        cx.update_window(self.window, |_, window, _| window.patch_counters()).unwrap()
    }

    fn scene(&self, cx: &mut TestAppContext) -> String {
        cx.update_window(self.window, |_, window, _| {
            let scene = &window.rendered_frame.scene;
            format!("{} operations\n{:#?}", scene.len(), scene.quads)
        })
        .unwrap()
    }

    fn can_patch(&self, cx: &mut TestAppContext, row: usize) -> bool {
        cx.update_window(self.window, |_, window, _| window.patch_blocker(&ElementId::from(("row", row))).is_none()).unwrap()
    }
}

#[gpui::test]
fn a_row_at_its_recorded_place_is_patched_and_paints_what_a_frame_built_whole_paints(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    assert!(fixture.can_patch(cx, 1), "the rows are recorded where they are");
    fixture.patch(cx, &[1], true);
    let counters = fixture.patched(cx);
    assert_eq!((counters.patched_frames, counters.patched_units, counters.total_declines()), (1, 1, 0), "{counters:?}");
    let patched = fixture.scene(cx);
    fixture.whole(cx, Some(1));
    assert_eq!(patched, fixture.scene(cx), "the patch wrote what the frame paints");

    // And back, in a frame that replays the rows built before.
    fixture.patch(cx, &[1], false);
    let counters = fixture.patched(cx);
    assert_eq!((counters.patched_frames, counters.total_declines()), (2, 0), "{counters:?}");
    let patched = fixture.scene(cx);
    fixture.whole(cx, None);
    assert_eq!(patched, fixture.scene(cx));
}

#[gpui::test]
fn two_rows_in_one_patch_write_their_primitives_in_place(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.patch(cx, &[1, 2], true);
    let counters = fixture.patched(cx);
    assert_eq!((counters.patched_frames, counters.patched_units, counters.total_declines()), (1, 2, 0), "{counters:?}");
    // The first patch of a frame makes the scene again and works out where the primitives are.
    fixture.patch(cx, &[1, 2], false);
    fixture.patch(cx, &[1], true);
    let counters = fixture.patched(cx);
    assert_eq!(counters.patched_frames, 3, "{counters:?}");
    assert_eq!(counters.scene_rebuilds, 1, "{counters:?}");
    assert!(counters.in_place_primitives >= 3, "{counters:?}");
}

#[gpui::test]
fn a_row_put_elsewhere_than_it_was_recorded_is_declined_as_relocated(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    // The list scrolls by 5: the rows are replayed 5 from their recordings.
    fixture.wheel(cx, 5.);
    assert!(!fixture.can_patch(cx, 1));
    fixture.patch(cx, &[1], true);
    let counters = fixture.patched(cx);
    assert_eq!(counters.patched_frames, 0, "{counters:?}");
    assert_eq!(counters.declines(PatchDecline::Relocated), 1, "{counters:?}");
    assert_eq!(counters.declines(PatchDecline::UnknownUnit), 0, "{counters:?}");
}

#[gpui::test]
fn a_row_built_where_it_stands_is_patched_after_a_scroll(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.wheel(cx, 5.);
    // A frame built whole records the rows where they stand now.
    fixture.whole(cx, None);
    fixture.knobs.key.set(2);
    fixture.whole(cx, None);
    assert!(fixture.can_patch(cx, 1));
    fixture.patch(cx, &[1], true);
    let counters = fixture.patched(cx);
    assert_eq!((counters.patched_frames, counters.total_declines()), (1, 0), "{counters:?}");
    let patched = fixture.scene(cx);
    fixture.whole(cx, Some(1));
    assert_eq!(patched, fixture.scene(cx));
}

#[gpui::test]
fn a_patched_row_keeps_a_scroll_from_putting_back_the_paint_before(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.patch(cx, &[1], true);
    let before = fixture.patched(cx);
    assert_eq!(before.patched_frames, 1);
    let eligible = |cx: &mut TestAppContext| {
        cx.update_window(fixture.window, |_, window, _| window.scroll_patch_counters().eligible).unwrap()
    };
    let eligible_before = eligible(cx);
    // The recording of the row is of the paint before the patch: a translate would show it.
    fixture.wheel(cx, 5.);
    assert_eq!(eligible(cx), eligible_before, "the wheel after a patch is a frame built whole");
    fixture.wheel(cx, 5.);
    assert_eq!(eligible(cx), eligible_before + 1, "and the frame after it is a translate again");
}
