//! A patch of the rows of a list: a row is recorded to be put elsewhere when the list scrolls
//! (`retained(..).relocatable(..)`), and is patched while the frame has it where it was recorded. The
//! rows are a list of boxes whose colour a hover changes, built in the frame as the list asks for them
//! and built again by the patcher with the colour the pointer is giving them.

use crate::{
    AppContext as _, AnyElement, AnyWindowHandle, App, Context, ElementId, InteractiveElement, IntoElement, ListAlignment, ListState,
    ParentElement, PatchDecline, Render, ScrollDelta, ScrollPatchReason, ScrollWheelEvent, Styled, TestAppContext, Window, div, list,
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
    /// Whether a row that is not hot has a fully transparent background that is kept.
    transparent: Cell<bool>,
}

fn row_element(ix: usize, hot: bool, transparent: bool) -> AnyElement {
    let mut row = div().id(("row", ix)).w_full().h(px(ROW)).always_hitbox();
    if transparent {
        row.style().keep_transparent_background = Some(true);
        row = row.bg(if hot { crate::red() } else { crate::transparent_black() });
    } else {
        row = row.bg(if hot { crate::red() } else { crate::blue() });
    }
    row.into_any_element()
}

#[derive(Clone)]
struct Page {
    knobs: Rc<Knobs>,
    state: ListState,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let knobs = self.knobs.clone();
        let transparent = knobs.transparent.get();
        div().size_full().child(
            list(self.state.clone(), move |ix, _, _| {
                let hot = knobs.hot.get() == Some(ix);
                retained(("row", ix), knobs.key.get() * 1_000 + u64::from(hot), move |_, _| row_element(ix, hot, transparent))
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
        Self::with_transparent(cx, false)
    }

    fn with_transparent(cx: &mut TestAppContext, transparent: bool) -> Self {
        let knobs = Rc::new(Knobs::default());
        knobs.key.set(1);
        knobs.transparent.set(transparent);
        let state = ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(ROW));
        let page = Page { knobs: knobs.clone(), state };
        let window = AnyWindowHandle::from(cx.open_window(size(px(400.), px(300.)), move |_, _| page));
        cx.update_window(window, |_, window, _| {
            window.set_dom_hit_testing(true);
            window.set_scroll_patch(true);
            window.set_scroll_patch_gate(Some(Rc::new(|_, _| None)));
            let knobs = knobs.clone();
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                let units = knobs
                    .patch
                    .borrow()
                    .iter()
                    .map(|&ix| (ElementId::from(("row", ix)), row_element(ix, knobs.painted.get(), knobs.transparent.get())))
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
fn a_row_with_a_kept_transparent_background_is_patched_to_the_colour_of_the_hover(cx: &mut TestAppContext) {
    let fixture = Fixture::with_transparent(cx, true);
    let quads = cx.update_window(fixture.window, |_, window, _| window.painted_quads().len()).unwrap();
    assert_eq!(quads, 4, "the rows in view each paint a quad of the kept background");
    assert!(fixture.can_patch(cx, 1));
    fixture.patch(cx, &[1], true);
    let counters = fixture.patched(cx);
    assert_eq!((counters.patched_frames, counters.patched_units, counters.total_declines()), (1, 1, 0), "{counters:?}");
    let patched = fixture.scene(cx);
    fixture.whole(cx, Some(1));
    assert_eq!(patched, fixture.scene(cx), "the patch wrote what the frame paints");

    fixture.patch(cx, &[1], false);
    let counters = fixture.patched(cx);
    assert_eq!((counters.patched_frames, counters.total_declines()), (2, 0), "{counters:?}");
    let patched = fixture.scene(cx);
    fixture.whole(cx, None);
    assert_eq!(patched, fixture.scene(cx));
}

#[gpui::test]
fn a_transparent_background_paints_no_quad_unless_it_is_kept(cx: &mut TestAppContext) {
    let kept = |cx: &mut TestAppContext, keep: bool| {
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(40.)), |_, _| {
            let mut row = div().w_full().h(px(ROW)).bg(crate::transparent_black());
            if keep {
                row.style().keep_transparent_background = Some(true);
            }
            row
        });
        cx.update(|window, _| window.painted_quads())
    };
    assert!(kept(cx, false).is_empty());
    let quads = kept(cx, true);
    assert_eq!(quads.len(), 1);
    assert!(quads[0].background.is_transparent());
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
    // The patch rewrote the recording of the row, so the translate puts the paint the patch made.
    fixture.wheel(cx, 5.);
    fixture.wheel(cx, 5.);
    assert!(eligible(cx) > eligible_before, "the wheel after a patch is a translate");
    let translated = fixture.scene(cx);
    fixture.whole(cx, Some(1));
    assert_eq!(translated, fixture.scene(cx), "the rows moved show the paint of the patch");
}

/// A pointer that stays at one place while a wheel moves the rows under it, as the owner of the window
/// follows it: the owner does not mind which row has the hover when it only paints (it says so to the
/// classifier), and paints it with the patch it asks for after the frame that moved the rows.
struct Pointed {
    fixture: Fixture,
    hot: Option<usize>,
    scrolled: f32,
}

const POINTER_Y: f32 = 28.;

impl Pointed {
    fn new(cx: &mut TestAppContext, translates: bool) -> Self {
        let fixture = Fixture::new(cx);
        cx.update_window(fixture.window, |_, window, cx| {
            window.set_scroll_patch(translates);
            window.set_scroll_patch_hover(Some(Rc::new(|_, _, _| false)));
            let moved = crate::MouseMoveEvent { position: point(px(10.), px(POINTER_Y)), ..Default::default() };
            window.dispatch_event(crate::PlatformInput::MouseMove(moved), cx);
        })
        .unwrap();
        fixture.draw(cx);
        fixture.draw(cx);
        let mut pointed = Self { fixture, hot: None, scrolled: 0. };
        pointed.follow_pointer(cx, translates);
        pointed
    }

    fn row_under_pointer(&self) -> usize {
        ((POINTER_Y + self.scrolled) / ROW) as usize
    }

    /// Paints the hover on the row the pointer is over, as the owner does after a frame: by a patch if the frame has
    /// the rows where one can be made, by a frame built whole if not.
    fn follow_pointer(&mut self, cx: &mut TestAppContext, patches: bool) {
        let under = self.row_under_pointer();
        if self.hot == Some(under) {
            return;
        }
        let before = self.fixture.patched(cx).patched_frames;
        if patches {
            if let Some(hot) = self.hot {
                self.fixture.patch(cx, &[hot], false);
            }
            self.fixture.patch(cx, &[under], true);
        }
        let patched = self.fixture.patched(cx).patched_frames - before;
        let wanted = u64::from(self.hot.is_some()) + 1;
        if !patches || patched != wanted {
            self.fixture.whole(cx, Some(under));
        }
        self.hot = Some(under);
    }

    fn wheel(&mut self, cx: &mut TestAppContext, by: f32) {
        self.scrolled += by;
        cx.update_window(self.fixture.window, |_, window, cx| {
            let event = ScrollWheelEvent {
                position: point(px(10.), px(POINTER_Y)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(crate::PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
    }

    fn counters(&self, cx: &mut TestAppContext) -> crate::ScrollPatchCounters {
        cx.update_window(self.fixture.window, |_, window, _| window.scroll_patch_counters()).unwrap()
    }
}

#[gpui::test]
fn a_hover_the_owner_paints_with_a_patch_after_a_translate_is_what_a_frame_built_whole_paints(cx: &mut TestAppContext) {
    let mut translated = Pointed::new(cx, true);
    let mut whole = Pointed::new(cx, false);
    // Down across the border of the rows 1 and 2 at 32 and back up over it.
    for by in [5., 5., 5., 5., 5., 5., 5., 5., -5., -5., -5., -5., -5., -5., -5., -5.] {
        translated.wheel(cx, by);
        whole.wheel(cx, by);
        translated.follow_pointer(cx, true);
        whole.follow_pointer(cx, false);
        assert_eq!(translated.fixture.scene(cx), whole.fixture.scene(cx), "scrolled to {}", translated.scrolled);
    }
    let counters = translated.counters(cx);
    // The rows of the list have no sizes measured but the ones in view, so one step that brings a row in is built whole.
    assert_eq!((counters.frames, counters.eligible, counters.mispredicted), (16, 15, 0), "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Membership), 1, "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Hover), 0, "{counters:?}");
    assert!(counters.held(ScrollPatchReason::HitboxChain) >= 3, "the hover changed at the borders the pointer crossed: {counters:?}");
    let patched = translated.fixture.patched(cx);
    // What a patch declines is a row the frame built whole put elsewhere than it recorded it, after the step that was
    // built whole; the owner builds the hover then.
    assert_eq!(patched.declines(PatchDecline::Relocated), patched.total_declines(), "{patched:?}");
    assert!(patched.patched_frames >= 6, "the rows a translate moved are patched where they stand: {patched:?}");
}

#[gpui::test]
fn fifty_random_steps_under_a_still_pointer_paint_what_frames_built_whole_paint(cx: &mut TestAppContext) {
    let mut translated = Pointed::new(cx, true);
    let mut whole = Pointed::new(cx, false);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for step in 0..50 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        // Within what the rows recorded cover, so that every step is a translate.
        let by = ((seed % 19) as f32 - 9.).clamp(-translated.scrolled, 90. - translated.scrolled);
        translated.wheel(cx, by);
        whole.wheel(cx, by);
        translated.follow_pointer(cx, true);
        whole.follow_pointer(cx, false);
        assert_eq!(translated.fixture.scene(cx), whole.fixture.scene(cx), "step {step}, scrolled to {}", translated.scrolled);
    }
    let counters = translated.counters(cx);
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    let held: u64 = ScrollPatchReason::ALL.iter().map(|reason| counters.reasons(*reason)).sum();
    assert_eq!(counters.reasons(ScrollPatchReason::Hover), 0, "{counters:?}");
    assert_eq!(counters.frames, counters.eligible + held, "{counters:?}");
    assert!(counters.eligible > 30, "{counters:?}");
}
