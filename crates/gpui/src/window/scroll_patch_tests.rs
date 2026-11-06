//! The classifier of the frames drawn after a list scroll (see [`super::scroll_patch`]): each
//! reason, the frame that has none, and that nothing is drawn differently.

use crate::{
    AnyWindowHandle, AppContext as _, Context, ElementId, FocusHandle, GlobalElementId, InteractiveElement,
    IntoElement, ListAlignment, ListState, MouseMoveEvent, ParentElement, PlatformInput, Render, ScrollDelta,
    ScrollPatchCause, ScrollPatchReason, ScrollWheelEvent, StatefulInteractiveElement, Styled, TestAppContext,
    Window, div, list, point, px, retained, size,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ROW: f32 = 30.;
const LIST: f32 = 100.;
/// The row that holds the focus in the test of the focus: far from the view.
const FOCUSED: usize = 50;

#[derive(Clone)]
struct Page {
    state: ListState,
    /// The overscan of the rows, if they are relocatable.
    overscan: Option<f32>,
    /// The rows have a hitbox.
    hitboxes: bool,
    /// A relocatable root inside each row, with the overscan of the row.
    nested: bool,
    /// The key of the rows: another one builds them again.
    key: Rc<Cell<u64>>,
    focus: Rc<RefCell<Option<FocusHandle>>>,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let page = self.clone();
        div().size_full().child(
            list(self.state.clone(), move |ix, _, _| {
                let hitboxes = page.hitboxes;
                let key = page.key.get();
                let nested = page.nested.then_some(page.overscan).flatten();
                let focus = (ix == FOCUSED).then(|| page.focus.borrow().clone()).flatten();
                let mut row = retained(("row", ix), key, move |_, _| {
                    let row = div().id(("row", ix)).w_full().h(px(ROW)).bg(crate::blue());
                    let row = match nested {
                        Some(overscan) => row.child(
                            retained(("inner", ix), key, |_, _| div().size_full().into_any_element())
                                .relocatable(px(overscan)),
                        ),
                        None => row,
                    };
                    let row = if hitboxes { row.always_hitbox() } else { row };
                    match focus {
                        Some(focus) => row.track_focus(&focus).into_any_element(),
                        None => row.into_any_element(),
                    }
                });
                if let Some(overscan) = page.overscan {
                    row = row.relocatable(px(overscan));
                }
                row.into_any_element()
            })
            .w(px(100.))
            .h(px(LIST)),
        )
    }
}

struct Fixture {
    window: AnyWindowHandle,
    page: Page,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, overscan: Option<f32>, hitboxes: bool) -> Self {
        Self::with_nested(cx, overscan, hitboxes, false)
    }

    fn with_nested(cx: &mut TestAppContext, overscan: Option<f32>, hitboxes: bool, nested: bool) -> Self {
        let state = ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(ROW));
        let page = Page {
            state,
            overscan,
            hitboxes,
            nested,
            key: Rc::new(Cell::new(1)),
            focus: Rc::default(),
        };
        let view = page.clone();
        let window = cx.open_window(size(px(400.), px(300.)), move |_, _| view);
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, _| {
            window.set_dom_hit_testing(true);
            window.set_scroll_patch(true);
            window.set_scroll_patch_gate(Some(Rc::new(|_| None)));
        })
        .unwrap();
        let fixture = Self { window, page };
        // The first frame builds and records the rows, the second puts them back.
        fixture.draw(cx);
        fixture.draw(cx);
        fixture
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    /// Puts the pointer at `(x, y)` and draws the frame that shows it there, as the wheel events before
    /// the one that is classified do.
    fn point_at(&self, cx: &mut TestAppContext, x: f32, y: f32) {
        let at = point(px(x), px(y));
        if self.with_window(cx, |window| window.mouse_position()) != at {
            cx.update_window(self.window, |_, window, cx| {
                let moved = MouseMoveEvent { position: at, pressed_button: None, modifiers: Default::default() };
                window.dispatch_event(PlatformInput::MouseMove(moved), cx);
            })
            .unwrap();
            self.draw(cx);
        }
    }

    /// A wheel event at `(x, y)` that moves the rows up by `by`, and the frame it draws. `with` runs in
    /// the update that dispatches it: the test window draws when an update ends.
    fn wheel_with(&self, cx: &mut TestAppContext, x: f32, y: f32, by: f32, with: impl FnOnce(&mut Window, &mut crate::App)) {
        cx.update_window(self.window, |_, window, cx| {
            with(window, cx);
            let event = ScrollWheelEvent {
                position: point(px(x), px(y)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
        self.draw(cx);
    }

    fn scroll(&self, cx: &mut TestAppContext, x: f32, y: f32, by: f32) {
        self.point_at(cx, x, y);
        self.wheel_with(cx, x, y, by, |_, _| {});
    }

    fn counters(&self, cx: &mut TestAppContext) -> crate::ScrollPatchCounters {
        cx.update_window(self.window, |_, window, _| window.scroll_patch_counters())
            .unwrap()
    }

    fn with_window<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Window) -> R) -> R {
        cx.update_window(self.window, |_, window, _| f(window)).unwrap()
    }
}

fn only(counters: &crate::ScrollPatchCounters, reason: ScrollPatchReason) {
    for other in ScrollPatchReason::ALL {
        let expected = u64::from(other == reason);
        assert_eq!(counters.reasons(other), expected, "{other:?}: {counters:?}");
    }
    assert_eq!((counters.frames, counters.eligible), (1, 0), "{counters:?}");
}

#[gpui::test]
fn a_fraction_of_a_row_that_brings_in_no_row_is_a_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.mispredicted), (1, 1, 0), "{counters:?}");
    for reason in ScrollPatchReason::ALL {
        assert_eq!(counters.reasons(reason), 0, "{reason:?}");
    }

    // The frame after that one is the new frame shown: another fraction is a translate from it.
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.mispredicted), (2, 2, 0), "{counters:?}");
}

#[gpui::test]
fn a_scroll_that_brings_in_a_row_is_a_membership_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    // The rows 0 to 3 show; at 40 the row 4 does.
    fixture.scroll(cx, 10., 10., 40.);
    only(&fixture.counters(cx), ScrollPatchReason::Membership);
}

#[gpui::test]
fn a_row_that_leaves_is_not_a_membership_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    // At 25 the rows 0 to 4 show; at 35 the row 0 has left and no row is new.
    fixture.scroll(cx, 10., 10., 25.);
    fixture.scroll(cx, 10., 10., 10.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.mispredicted), (2, 1, 0), "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Membership), 1, "{counters:?}");
}

#[gpui::test]
fn a_pointer_that_keeps_its_hitboxes_is_not_a_hover_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    // The pointer at 10 is in the row 0 before and after a scroll by 5.
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.mispredicted), (1, 0), "{counters:?}");
}

#[gpui::test]
fn a_pointer_that_crosses_a_row_is_a_hover_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    // The row 0 spans 0 to 30: the pointer at 28 is in the row 1 after a scroll by 5.
    fixture.scroll(cx, 10., 28., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Hover);
}

#[gpui::test]
fn the_hover_change_is_the_only_reason_when_it_is_counted_apart(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    fixture.scroll(cx, 10., 28., 5.);
    let counters = fixture.counters(cx);
    assert_eq!(counters.hover_only, 1, "{counters:?}");
}

/// Names the hitbox ids of a chain by the last segment of their path, topmost first.
fn named(chain: &[GlobalElementId]) -> Vec<String> {
    chain.iter().map(|id| format!("{:?}", id.last().expect("a hitbox id has a path"))).collect()
}

#[gpui::test]
fn a_hitbox_change_the_owner_does_not_mind_is_counted_and_not_a_blocker(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    let asked = Rc::new(RefCell::new(Vec::new()));
    let seen = asked.clone();
    fixture.with_window(cx, move |window| {
        window.set_scroll_patch_hover(Some(Rc::new(move |_, before, after| {
            seen.borrow_mut().push((named(before), named(after)));
            false
        })))
    });
    // The row 0 spans 0 to 30: the pointer at 28 is in the row 1 after a scroll by 5.
    fixture.scroll(cx, 10., 28., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.mispredicted), (1, 1, 0), "{counters:?}");
    assert_eq!(counters.held(ScrollPatchReason::HitboxChain), 1, "{counters:?}");
    assert_eq!(counters.held(ScrollPatchReason::Hover), 0, "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Hover), 0, "{counters:?}");
    let asked = asked.borrow();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let (before, after) = &asked[0];
    assert!(before[0].ends_with("0)"), "the row 0 is under the pointer before: {asked:?}");
    assert!(after[0].ends_with("1)"), "the row 1 is under the pointer after: {asked:?}");
}

#[gpui::test]
fn a_hitbox_change_the_owner_minds_is_a_hover_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    fixture.with_window(cx, |window| window.set_scroll_patch_hover(Some(Rc::new(|_, _, _| true))));
    fixture.scroll(cx, 10., 28., 5.);
    let counters = fixture.counters(cx);
    assert_eq!(counters.held(ScrollPatchReason::HitboxChain), 1, "{counters:?}");
    only(&counters, ScrollPatchReason::Hover);
    assert_eq!(counters.hover_only, 1, "{counters:?}");
}

#[gpui::test]
fn the_owner_is_not_asked_when_the_hitboxes_under_the_pointer_are_the_same(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    let asked = Rc::new(Cell::new(0));
    let seen = asked.clone();
    fixture.with_window(cx, move |window| {
        window.set_scroll_patch_hover(Some(Rc::new(move |_, _, _| {
            seen.set(seen.get() + 1);
            true
        })))
    });
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, asked.get()), (1, 0), "{counters:?}");
    assert_eq!(counters.held(ScrollPatchReason::HitboxChain), 0, "{counters:?}");
}

#[gpui::test]
fn a_pointer_that_moved_is_a_hover_change_whatever_the_owner_says(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    fixture.with_window(cx, |window| window.set_scroll_patch_hover(Some(Rc::new(|_, _, _| false))));
    fixture.point_at(cx, 10., 10.);
    // The wheel event is at another place than the pointer was drawn at.
    fixture.wheel_with(cx, 10., 50., 5., |_, _| {});
    only(&fixture.counters(cx), ScrollPatchReason::Hover);
}

#[gpui::test]
fn rows_recorded_with_no_overscan_miss_the_region(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(0.), false);
    // The row 3 spans 90 to 120 and the region is the view: a row moved up by 5 leaves 95 to 100 of the
    // view with nothing recorded.
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    only(&counters, ScrollPatchReason::Region);
    let causes: Vec<_> = ScrollPatchCause::ALL.into_iter().filter(|cause| counters.held_cause(*cause) == 1).collect();
    assert_eq!(causes, [ScrollPatchCause::RegionBottom], "the rows moved up and the region ends where the view does: {counters:?}");
}

#[gpui::test]
fn rows_moved_down_miss_the_top_of_the_region(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(0.), false);
    fixture.scroll(cx, 10., 10., 40.);
    fixture.scroll(cx, 10., 10., -5.);
    let counters = fixture.counters(cx);
    assert_eq!(counters.held_cause(ScrollPatchCause::RegionTop), 1, "{counters:?}");
}

#[gpui::test]
fn rows_that_are_not_relocatable_are_not_runs(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, None, false);
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    only(&counters, ScrollPatchReason::NotRelocatable);
    let causes: Vec<_> = ScrollPatchCause::ALL.into_iter().filter(|cause| counters.held_cause(*cause) == 1).collect();
    assert_eq!(causes, [ScrollPatchCause::NoRun], "{counters:?}");
}

#[gpui::test]
fn a_root_inside_a_row_that_was_built_is_no_run(cx: &mut TestAppContext) {
    let fixture = Fixture::with_nested(cx, Some(100.), false, true);
    // The rows are built again: the roots inside them are built in the region of the row, and keep no
    // recording of their own.
    fixture.point_at(cx, 10., 10.);
    fixture.page.key.set(2);
    // The window draws when the update ends, and that frame is the one shown.
    fixture.with_window(cx, |window| window.refresh());
    fixture.wheel_with(cx, 10., 10., 5., |_, _| {});
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.mispredicted), (1, 1, 0), "{counters:?}");
    for cause in ScrollPatchCause::ALL {
        assert_eq!(counters.held_cause(cause), 0, "{cause:?}: {counters:?}");
    }
}

#[gpui::test]
fn a_commit_the_owner_reports_is_an_edit(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| {
        window.set_scroll_patch_gate(Some(Rc::new(|_| Some(ScrollPatchReason::Edited))))
    });
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Edited);
}

#[gpui::test]
fn a_scroll_animation_the_owner_reports_is_one(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| {
        window.set_scroll_patch_gate(Some(Rc::new(|_| Some(ScrollPatchReason::ScrollAnimation))))
    });
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::ScrollAnimation);
}

#[gpui::test]
fn a_window_without_an_owner_has_no_frame_to_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| window.set_scroll_patch_gate(None));
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Other);
}

#[gpui::test]
fn the_accessibility_tree_is_a_reason(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| window.set_a11y_active_for_tests(true));
    fixture.draw(cx);
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::A11y);
}

#[gpui::test]
fn another_view_notified_is_an_edit(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    let other = cx.update_window(fixture.window, |_, _, cx| cx.new(|_| ()).entity_id()).unwrap();
    fixture.point_at(cx, 10., 10.);
    fixture.wheel_with(cx, 10., 10., 5., |window, cx| {
        window.invalidator.invalidate_view(other, cx);
    });
    only(&fixture.counters(cx), ScrollPatchReason::Edited);
}

#[gpui::test]
fn a_focus_in_a_row_the_list_builds_for_it_alone_is_a_reason(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    let handle = cx.update(|cx| cx.focus_handle());
    *fixture.page.focus.borrow_mut() = Some(handle.clone());
    fixture
        .page
        .state
        .splice_focusable(0..100, (0..100).map(|ix| (ix == FOCUSED).then(|| handle.clone())));
    fixture.page.key.set(2);
    cx.update_window(fixture.window, |_, window, cx| {
        window.focus(&handle, cx);
        window.refresh();
    })
    .unwrap();
    // The list builds the row of the focus though it is far from the view, and the frames after
    // record it.
    fixture.draw(cx);
    fixture.draw(cx);
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Focus);
}

#[gpui::test]
fn every_reason_that_holds_is_counted_and_the_first_is_the_reason(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| {
        window.set_scroll_patch_gate(Some(Rc::new(|_| Some(ScrollPatchReason::Edited))));
        window.set_a11y_active_for_tests(true);
    });
    fixture.draw(cx);
    fixture.scroll(cx, 10., 10., 40.);
    let counters = fixture.counters(cx);
    assert_eq!(counters.reasons(ScrollPatchReason::Edited), 1, "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Membership), 0, "{counters:?}");
    let held: Vec<_> = ScrollPatchReason::ALL.into_iter().filter(|reason| counters.held(*reason) == 1).collect();
    assert_eq!(
        held,
        [ScrollPatchReason::Edited, ScrollPatchReason::Membership, ScrollPatchReason::A11y],
        "{counters:?}"
    );
}

#[gpui::test]
fn a_window_that_does_not_classify_counts_nothing(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| window.set_scroll_patch(false));
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!(counters.frames, 0, "{counters:?}");
}

#[gpui::test]
fn a_frame_after_a_scroll_is_drawn_as_it_is_without_the_classifier(cx: &mut TestAppContext) {
    let describe = |cx: &mut TestAppContext, classify: bool| {
        let fixture = Fixture::new(cx, Some(100.), true);
        fixture.with_window(cx, |window| window.set_scroll_patch(classify));
        for by in [5., 10., 40., -30.] {
            fixture.scroll(cx, 10., 28., by);
        }
        fixture.with_window(cx, |window| {
            let scene = &window.rendered_frame.scene;
            format!("{}\n{:?}", scene.describe_operations(0..scene.len(), false), window.mouse_position())
        })
    };
    let with = describe(cx, true);
    let without = describe(cx, false);
    assert_eq!(with, without);
}
