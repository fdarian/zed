//! The classifier of the frames drawn after a list scroll (see [`super::scroll_patch`]): each
//! reason, the frame that has none, and that nothing is drawn differently; and the translate of
//! the rows of the frame shown that an eligible frame is drawn by: it is the frame built whole,
//! it declines before it writes, and the verifier checks it.

use crate::{
    AnyWindowHandle, AppContext as _, Bounds, Context, FocusHandle, GlobalElementId, InteractiveElement,
    IntoElement, ListAlignment, ListState, MouseMoveEvent, ParentElement, PlatformInput, Render, ScrollDelta,
    ScrollApplyDecline, ScrollPatchCause, ScrollPatchReason, ScrollThumbMiss, ScrollWheelEvent,
    StatefulInteractiveElement as _, Styled, TestAppContext, Window, div, fill, list, point, px, retained, size,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ROW: f32 = 30.;
const LIST: f32 = 100.;
/// The row that holds the focus in the test of the focus: far from the view.
const FOCUSED: usize = 50;

/// The bar a list is given in the tests of the thumb: a thumb of `length` at `alpha` that lies along
/// the list as far down as the list is scrolled. The cells are what the bar would be in the next
/// frame, which a test changes to say the thumb cannot be put there.
#[derive(Clone)]
struct Bar {
    alpha: Rc<Cell<f32>>,
    length: Rc<Cell<f32>>,
    track: Rc<Cell<f32>>,
    /// How far from where the bar paints it the plan says the thumb goes.
    lie: Rc<Cell<f32>>,
    /// The thumb is painted under a rotation, which a quad cannot be moved under in place.
    rotated: Rc<Cell<bool>>,
    /// How many frames that moved the rows told the bar.
    moved: Rc<Cell<u32>>,
    /// How many repaints of the thumb were written and told the bar.
    repainted: Rc<Cell<u32>>,
}

impl Bar {
    fn new() -> Self {
        Self {
            alpha: Rc::new(Cell::new(1.)),
            length: Rc::new(Cell::new(20.)),
            track: Rc::new(Cell::new(0.)),
            lie: Rc::default(),
            rotated: Rc::default(),
            moved: Rc::default(),
            repainted: Rc::default(),
        }
    }

    /// The thumb for the list scrolled where it is: 3000 of content in a window of 100.
    fn thumb(state: &ListState, length: f32, track: f32) -> Bounds<crate::Pixels> {
        let travel = LIST - length;
        let top = f32::from(state.scrolled()) / (ROW * 100. - LIST) * travel;
        Bounds::new(point(px(92. + track), px(top)), size(px(6.), px(length)))
    }
}

#[derive(Clone)]
struct Page {
    bar: Option<Bar>,
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
        let bar = self.bar.clone().map(|bar| (bar, self.state.clone()));
        div().size_full().child({
            let rows = list(self.state.clone(), move |ix, _, _| {
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
            .h(px(LIST));
            match bar {
                None => rows.into_any_element(),
                Some((bar, state)) => rows
                    .paint_overlay(move |_, window, _| {
                        let painted = (bar.alpha.get(), bar.length.get(), bar.track.get());
                        let (alpha, length, track) = painted;
                        let thumb = (alpha > 0.).then(|| {
                            let mut color = crate::black();
                            color.a = alpha;
                            fill(Bar::thumb(&state, length, track), color)
                        });
                        let plan_state = state.clone();
                        let plan_bar = bar.clone();
                        let plan = Rc::new(move |_: &Window, drawn: Option<Bounds<crate::Pixels>>| {
                            if painted.0 <= 0. {
                                return if plan_bar.alpha.get() > 0. { Err(ScrollThumbMiss::Appear) } else { Ok(None) };
                            }
                            if plan_bar.alpha.get() != painted.0 {
                                return Err(ScrollThumbMiss::Alpha);
                            }
                            if plan_bar.track.get() != painted.2 {
                                return Err(ScrollThumbMiss::Track);
                            }
                            if plan_bar.length.get() != painted.1 {
                                return Err(ScrollThumbMiss::Size);
                            }
                            let mut next = Bar::thumb(&plan_state, painted.1, painted.2);
                            let moved_to = next;
                            next.origin.y += px(plan_bar.lie.get());
                            Ok((drawn != Some(moved_to)).then_some(next))
                        });
                        let told = bar.moved.clone();
                        let moved = Rc::new(move |_: &mut Window, _: &mut crate::App| told.set(told.get() + 1));
                        let (repaint_bar, told) = (bar.clone(), bar.repainted.clone());
                        let repaint = Rc::new(move |_: &Window, _: Option<Bounds<crate::Pixels>>| {
                            let target = repaint_bar.alpha.get();
                            if target == painted.0 {
                                return Ok(None);
                            }
                            if painted.0 <= 0. || repaint_bar.length.get() != painted.1 || repaint_bar.track.get() != painted.2 {
                                return Err(ScrollThumbMiss::Appear);
                            }
                            let mut color = crate::black();
                            color.a = target;
                            let told = told.clone();
                            Ok(Some(crate::ScrollThumbRepaint {
                                background: color.into(),
                                done: Box::new(move |_, _| told.set(told.get() + 1)),
                            }))
                        });
                        if bar.rotated.get() {
                            let turn = crate::TransformationMatrix::unit().rotate(crate::radians(0.1));
                            window.with_transform(turn, |window| { window.paint_scroll_thumb(&state, thumb, plan, moved, repaint); });
                        } else {
                            window.paint_scroll_thumb(&state, thumb, plan, moved, repaint);
                        }
                    })
                    .into_any_element(),
            }
        })
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
        Self::open(cx, overscan, hitboxes, nested, None)
    }

    fn with_bar(cx: &mut TestAppContext, bar: &Bar) -> Self {
        Self::open(cx, Some(100.), true, false, Some(bar.clone()))
    }

    fn open(cx: &mut TestAppContext, overscan: Option<f32>, hitboxes: bool, nested: bool, bar: Option<Bar>) -> Self {
        let state = ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(ROW));
        let page = Page {
            bar,
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
            window.set_scroll_patch_gate(Some(Rc::new(|_, _| None)));
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

    /// As `scroll`, without the draw after the update: the frame shown is the one the update drew, which a translate leaves
    /// as it made it.
    fn scroll_once(&self, cx: &mut TestAppContext, x: f32, y: f32, by: f32) {
        self.point_at(cx, x, y);
        cx.update_window(self.window, |_, window, cx| {
            let event = ScrollWheelEvent {
                position: point(px(x), px(y)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
                ..Default::default()
            };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
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
fn rows_recorded_with_no_overscan_are_built_again_where_they_are(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(0.), false);
    // The row 3 spans 90 to 120 and the region is the view: a row moved up by 5 leaves 95 to 100 of the
    // view with nothing recorded, so the rows are built where they are put.
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched, counters.mispredicted), (1, 1, 1, 0), "{counters:?}");
    assert_eq!((counters.rows_rebuilt, counters.rows_removed), (counters.rows_built, 0), "{counters:?}");
    assert!(counters.rows_rebuilt >= 1, "{counters:?}");
    for reason in ScrollPatchReason::ALL {
        assert_eq!(counters.reasons(reason), 0, "{reason:?}: {counters:?}");
    }
}

#[gpui::test]
fn rows_moved_down_are_built_again_where_they_are(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(0.), false);
    fixture.scroll(cx, 10., 10., 40.);
    let before = fixture.counters(cx);
    fixture.scroll(cx, 10., 10., -5.);
    let counters = fixture.counters(cx);
    assert_eq!(counters.eligible - before.eligible, 1, "{counters:?}");
    assert!(counters.rows_rebuilt > before.rows_rebuilt, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
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
        window.set_scroll_patch_gate(Some(Rc::new(|_, _| Some(ScrollPatchReason::Edited))))
    });
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Edited);
}

#[gpui::test]
fn a_translate_takes_the_view_the_scroll_notified_off_the_queue(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.point_at(cx, 10., 10.);
    fixture.scroll_once(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");
    // A tick drawn next would decline `PendingView` with the list's view still queued.
    assert!(!fixture.with_window(cx, |window| window.invalidator.has_views()));
}

#[gpui::test]
fn the_recorder_kept_for_the_next_relocation_holds_nothing_it_painted(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| {
        let spare = window.retained.spare_recorder.as_ref().expect("the rows were painted through a recorder");
        assert_eq!(spare.describe_primitives(), "", "what a row painted is in its recording and not in the kept recorder");
    });
}

#[gpui::test]
fn a_scroll_animation_the_owner_reports_is_one(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| {
        window.set_scroll_patch_gate(Some(Rc::new(|_, _| Some(ScrollPatchReason::ScrollAnimation))))
    });
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::ScrollAnimation);
}

#[gpui::test]
fn a_tick_due_in_the_draw_is_a_reason_and_is_not_taken_by_a_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.point_at(cx, 10., 10.);
    fixture.wheel_with(cx, 10., 10., 5., |window, _| window.request_tick());
    only(&fixture.counters(cx), ScrollPatchReason::Clock);

    // Without the tick the next scroll is a translate again.
    fixture.wheel_with(cx, 10., 10., 5., |_, _| {});
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.reasons(ScrollPatchReason::Clock)), (2, 1, 1), "{counters:?}");
}

#[gpui::test]
fn a_frame_the_owner_says_asked_for_the_next_is_a_clock(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| {
        window.set_scroll_patch_gate(Some(Rc::new(|_, _| Some(ScrollPatchReason::Clock))))
    });
    fixture.scroll(cx, 10., 10., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Clock);
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
        window.set_scroll_patch_gate(Some(Rc::new(|_, _| Some(ScrollPatchReason::Edited))));
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

/// The frame shown as text: the scene with its draw orders and every hitbox by value, and the
/// hit test at the pointer.
fn shown(window: &mut Window) -> String {
    use std::fmt::Write as _;

    let frame = &window.rendered_frame;
    let mut out = frame.scene.describe_primitives();
    out.push_str(&frame.scene.describe_operations(0..frame.scene.len(), false));
    for hitbox in &frame.hitboxes {
        writeln!(out, "hitbox {}", hitbox.describe()).ok();
    }
    let pointer = window.mouse_position();
    let chain = window.rendered_frame.hit_test(pointer, true).chain.iter().filter_map(|link| link.identity.clone()).collect::<Vec<_>>();
    writeln!(out, "pointer {pointer:?} over {}", named(&chain).join(" ")).ok();
    without_identities_of_the_window(&out)
}

/// `text` without the entity of the window and the hash of the paths, which two windows of one
/// test differ in.
fn without_identities_of_the_window(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((at, marker)) = ["EntityId(", "hash: "]
        .into_iter()
        .filter_map(|marker| rest.find(marker).map(|at| (at, marker)))
        .min_by_key(|(at, _)| *at)
    {
        out.push_str(&rest[..at + marker.len()]);
        rest = &rest[at + marker.len()..];
        let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The frame shown after each of `wheels`, with the translate on or off.
fn after_wheels(cx: &mut TestAppContext, nested: bool, patch: bool, at: (f32, f32), wheels: &[f32]) -> (Vec<String>, crate::ScrollPatchCounters) {
    let fixture = Fixture::with_nested(cx, Some(100.), true, nested);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let frames = wheels
        .iter()
        .map(|by| {
            fixture.scroll(cx, at.0, at.1, *by);
            fixture.with_window(cx, shown)
        })
        .collect();
    (frames, fixture.counters(cx))
}

#[gpui::test]
fn a_translate_is_the_frame_built_whole(cx: &mut TestAppContext) {
    // Fractions of a row inside the pointer's row, up and down, and past the region of the rows
    // (the region reaches 100 beyond the list, so 30 of 3 rows at 12 is a miss and builds).
    let wheels = [5., 5., 7., -3., -4., 6., 2., -15., 1.];
    for nested in [false, true] {
        let (translated, counters) = after_wheels(cx, nested, true, (10., 12.), &wheels);
        let (built, off) = after_wheels(cx, nested, false, (10., 12.), &wheels);
        assert_eq!(off.frames, 0, "{off:?}");
        assert!(counters.patched >= 4, "{counters:?}");
        assert_eq!(counters.mispredicted, 0, "{counters:?}");
        for (frame, (translated, built)) in translated.iter().zip(&built).enumerate() {
            assert!(
                translated == built,
                "frame {frame}, nested {nested}, {counters:?}\n{}",
                super::retained::first_difference(translated, built)
            );
        }
    }
}

#[gpui::test]
fn a_translate_keeps_the_recording_of_the_rows_for_the_frame_after(cx: &mut TestAppContext) {
    // A frame built whole after translated ones puts the rows from their recordings, and a
    // translate after it is from that frame: the two kinds interleave.
    let wheels = [5., 5., 25., 5., 5., 40., 4., 4.];
    let (translated, counters) = after_wheels(cx, false, true, (10., 12.), &wheels);
    let (built, _) = after_wheels(cx, false, false, (10., 12.), &wheels);
    assert!(counters.patched >= 3 && counters.patched < counters.frames, "{counters:?}");
    assert_eq!(translated, built, "{counters:?}");
}

#[gpui::test]
fn a_translate_follows_the_pointer_over_the_rows_it_moved(cx: &mut TestAppContext) {
    // The pointer at 28 is over the row 1 after 5 and the hit test is the moved rows'.
    let wheels = [5., 5., -5., 2.];
    let (translated, counters) = after_wheels(cx, false, true, (10., 28.), &wheels);
    let (built, _) = after_wheels(cx, false, false, (10., 28.), &wheels);
    assert_eq!(translated, built, "{counters:?}");
}

#[gpui::test]
fn a_translate_that_cannot_be_made_is_a_whole_frame_and_says_why(cx: &mut TestAppContext) {
    let broken = |cx: &mut TestAppContext, patch: bool| {
        let fixture = Fixture::new(cx, Some(100.), true);
        fixture.point_at(cx, 10., 12.);
        fixture.with_window(cx, |window| {
            window.set_scroll_patch(patch);
            // The hitbox of the last row is not the one it recorded.
            let last = window.rendered_frame.hitboxes.last_mut().expect("the rows have hitboxes");
            last.id = crate::window::HitboxId(u64::MAX);
        });
        fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
        (fixture.counters(cx), fixture.with_window(cx, shown))
    };
    let (counters, frame) = broken(cx, true);
    assert_eq!((counters.eligible, counters.patched), (1, 0), "{counters:?}");
    assert_eq!(counters.apply_declines(ScrollApplyDecline::Hitboxes), 1, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    let (_, whole) = broken(cx, false);
    assert_eq!(frame, whole);
}

#[gpui::test]
fn a_thumb_the_owner_reports_is_a_reason_and_nothing_is_moved(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), false);
    fixture.with_window(cx, |window| window.set_scroll_patch_gate(Some(Rc::new(|_, _| Some(ScrollPatchReason::Thumb)))));
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    only(&counters, ScrollPatchReason::Thumb);
    assert_eq!(counters.patched, 0, "{counters:?}");
}

#[gpui::test]
fn a_translate_is_checked_against_the_frame_built_whole_after_it(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    let described = Rc::new(Cell::new(0));
    let verifier = described.clone();
    // The frame that shows the pointer is built before the verifier is on: one that a verifier
    // checks is built whole a second time, and its rows are not recorded to be moved.
    fixture.point_at(cx, 10., 12.);
    fixture.with_window(cx, move |window| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        window.set_scroll_patch_registries(Some(Rc::new(move |_| {
            verifier.set(verifier.get() + 1);
            String::new()
        })));
    });
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");
    assert_eq!(described.get(), 2, "the owner described the registries of the translate and of the build");
    fixture.with_window(cx, |window| {
        assert_eq!(window.retention_mismatches(), &[] as &[String]);
        assert!(window.retained_counters().verified >= 1);
    });
}

#[gpui::test]
fn a_translate_that_differs_from_the_frame_built_whole_is_a_mismatch(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, Some(100.), true);
    let described = Rc::new(Cell::new(0));
    let verifier = described.clone();
    fixture.point_at(cx, 10., 12.);
    fixture.with_window(cx, move |window| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        // What the owner keeps outside the frame is not the same after the build.
        window.set_scroll_patch_registries(Some(Rc::new(move |_| {
            verifier.set(verifier.get() + 1);
            format!("described {}", verifier.get())
        })));
    });
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    fixture.with_window(cx, |window| {
        assert_eq!(window.retention_mismatches().len(), 1);
        assert!(window.retention_mismatches()[0].contains("outside the frame"), "{:?}", window.retention_mismatches());
    });
}

/// The frame shown after each of `wheels` with a bar on the list, the translate on or off, and how
/// many frames told the bar.
fn after_wheels_with_bar(cx: &mut TestAppContext, patch: bool, wheels: &[f32]) -> (Vec<String>, crate::ScrollPatchCounters, u32) {
    after_wheels_with_bar_at(cx, patch, wheels, true)
}

/// As [`after_wheels_with_bar`], with rows that have a hitbox or not.
fn after_wheels_with_bar_at(
    cx: &mut TestAppContext,
    patch: bool,
    wheels: &[f32],
    hitboxes: bool,
) -> (Vec<String>, crate::ScrollPatchCounters, u32) {
    let bar = Bar::new();
    let fixture = Fixture::open(cx, Some(100.), hitboxes, false, Some(bar.clone()));
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let frames = wheels
        .iter()
        .map(|by| {
            fixture.scroll_once(cx, 10., 12., *by);
            fixture.with_window(cx, shown)
        })
        .collect();
    (frames, fixture.counters(cx), bar.moved.get())
}

#[gpui::test]
fn a_translate_moves_the_thumb_of_the_bar_and_is_the_frame_built_whole(cx: &mut TestAppContext) {
    let wheels = [5., 5., 7., -3., -4., 6., 2., -15., 1.];
    let (translated, counters, told) = after_wheels_with_bar(cx, true, &wheels);
    let (built, off, _) = after_wheels_with_bar(cx, false, &wheels);
    assert_eq!(off.frames, 0, "{off:?}");
    assert!(counters.patched >= 4, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    assert_eq!(u64::from(told), counters.patched, "the bar is told once for each frame that moved the rows");
    for cause in ScrollPatchCause::ALL {
        assert_eq!(counters.held_cause(cause), 0, "{cause:?}: {counters:?}");
    }
    for (frame, (translated, built)) in translated.iter().zip(&built).enumerate() {
        assert!(
            translated == built,
            "frame {frame}, {counters:?}\n{}",
            super::retained::first_difference(translated, built)
        );
    }
}

#[gpui::test]
fn a_scroll_that_brings_the_thumb_back_to_where_the_frame_shown_painted_it_moves_it_back(cx: &mut TestAppContext) {
    // The second and the fourth wheel put the rows, and the thumb, where the frame shown had them. The rows built are the first
    // four, so 20 is as far as they move: the thumb moves by more than half a pixel. The rows have no hitbox, so the pointer is
    // no reason.
    let wheels = [20., -20., 18., -18., 3.];
    let (translated, counters, _) = after_wheels_with_bar_at(cx, true, &wheels, false);
    let (built, _, _) = after_wheels_with_bar_at(cx, false, &wheels, false);
    assert_eq!(counters.patched, wheels.len() as u64, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    for (frame, (translated, built)) in translated.iter().zip(&built).enumerate() {
        assert!(
            translated == built,
            "frame {frame}, {counters:?}\n{}",
            super::retained::first_difference(translated, built)
        );
    }
}

#[gpui::test]
fn a_thumb_that_the_scroll_moves_is_checked_against_the_frame_built_whole(cx: &mut TestAppContext) {
    let fixture = Fixture::with_bar(cx, &Bar::new());
    fixture.point_at(cx, 10., 12.);
    fixture.with_window(cx, |window| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
    });
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");
    fixture.with_window(cx, |window| {
        assert_eq!(window.retention_mismatches(), &[] as &[String]);
        assert!(window.retained_counters().verified >= 1);
    });
}

#[gpui::test]
fn a_thumb_put_where_the_bar_would_not_paint_it_is_a_mismatch(cx: &mut TestAppContext) {
    let bar = Bar::new();
    let fixture = Fixture::with_bar(cx, &bar);
    fixture.point_at(cx, 10., 12.);
    fixture.with_window(cx, |window| window.set_retention_verifier(Some(Rc::new(|_| String::new()))));
    bar.lie.set(3.);
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    fixture.with_window(cx, |window| {
        assert_eq!(window.retention_mismatches().len(), 1, "{:?}", window.retention_mismatches());
        assert!(window.retention_mismatches()[0].contains("the scene"), "{:?}", window.retention_mismatches());
    });
}

#[gpui::test]
fn a_bar_that_would_be_painted_otherwise_is_a_thumb_and_the_frame_is_whole(cx: &mut TestAppContext) {
    let causes = [
        ("alpha", ScrollPatchCause::ThumbAlpha, (|bar: &Bar| bar.alpha.set(0.5)) as fn(&Bar)),
        ("size", ScrollPatchCause::ThumbSize, |bar| bar.length.set(30.)),
        ("track", ScrollPatchCause::ThumbTrack, |bar| bar.track.set(1.)),
    ];
    for (name, cause, change) in causes {
        let bar = Bar::new();
        let fixture = Fixture::with_bar(cx, &bar);
        fixture.point_at(cx, 10., 12.);
        change(&bar);
        fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
        let counters = fixture.counters(cx);
        only(&counters, ScrollPatchReason::Thumb);
        assert_eq!(counters.patched, 0, "{name}: {counters:?}");
        for other in ScrollPatchCause::ALL {
            assert_eq!(counters.held_cause(other), u64::from(other == cause), "{name} {other:?}: {counters:?}");
        }
        assert_eq!(bar.moved.get(), 0, "{name}");
    }
}

#[gpui::test]
fn a_bar_that_is_not_painted_and_would_be_is_a_thumb(cx: &mut TestAppContext) {
    let bar = Bar::new();
    bar.alpha.set(0.);
    let fixture = Fixture::with_bar(cx, &bar);
    fixture.point_at(cx, 10., 12.);

    // Not painted and not to be: nothing of the bar to move.
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");

    bar.alpha.set(1.);
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::Thumb), 1, "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::ThumbAppear), 1, "{counters:?}");
}

#[gpui::test]
fn a_thumb_that_is_not_the_quad_it_noted_declines_the_translate_before_it_writes(cx: &mut TestAppContext) {
    let broken = |cx: &mut TestAppContext, patch: bool| {
        let fixture = Fixture::with_bar(cx, &Bar::new());
        fixture.point_at(cx, 10., 12.);
        fixture.with_window(cx, |window| {
            window.set_scroll_patch(patch);
            // The thumb is the last thing the list paints.
            let last = window.rendered_frame.scene.paint_operations.last_mut().expect("the frame has operations");
            *last = crate::scene::PaintOperation::EndLayer;
        });
        fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
        fixture.counters(cx)
    };
    let counters = broken(cx, true);
    assert_eq!((counters.eligible, counters.patched), (1, 0), "{counters:?}");
    assert_eq!(counters.apply_declines(ScrollApplyDecline::Thumb), 1, "{counters:?}");
}

#[gpui::test]
fn a_thumb_painted_under_a_transform_is_one_that_cannot_be_moved(cx: &mut TestAppContext) {
    let bar = Bar::new();
    bar.rotated.set(true);
    let fixture = Fixture::with_bar(cx, &bar);
    fixture.point_at(cx, 10., 12.);
    fixture.wheel_with(cx, 10., 12., 5., |_, _| {});
    let counters = fixture.counters(cx);
    only(&counters, ScrollPatchReason::Thumb);
    assert_eq!(counters.held_cause(ScrollPatchCause::ThumbTrack), 1, "{counters:?}");
}

/// Plans and writes the repaint of the thumbs of the frame shown, as a tick does.
fn repaint_thumbs(fixture: &Fixture, cx: &mut TestAppContext) -> Result<(), ScrollThumbMiss> {
    cx.update_window(fixture.window, |_, window, cx| {
        let repaints = window.plan_scroll_thumb_repaints()?;
        window.repaint_scroll_thumbs(repaints, cx);
        Ok(())
    })
    .unwrap()
}

#[gpui::test]
fn a_thumb_repainted_with_no_scroll_is_the_frame_built_whole(cx: &mut TestAppContext) {
    let bar = Bar::new();
    let fixture = Fixture::with_bar(cx, &bar);
    let before = fixture.with_window(cx, shown);
    for alpha in [0.75, 0.25, 0.1] {
        bar.alpha.set(alpha);
        // The bar of the frame shown has the alpha it was built with, so the next step is from that frame again.
        repaint_thumbs(&fixture, cx).unwrap();
        let repainted = fixture.with_window(cx, shown);
        assert_ne!(repainted, before);

        let built = Bar::new();
        built.alpha.set(alpha);
        let whole = Fixture::with_bar(cx, &built);
        let expected = whole.with_window(cx, shown);
        assert!(
            repainted == expected,
            "alpha {alpha}\n{}",
            super::retained::first_difference(&repainted, &expected)
        );
    }
    assert_eq!(bar.repainted.get(), 3, "the owner is told once for each repaint");
    assert!(fixture.with_window(cx, |window| window.patch_counters().thumb_repaints) >= 3);
    assert_eq!(fixture.counters(cx).frames, 0, "no scroll was classified: nothing was built");
}

#[gpui::test]
fn a_thumb_that_is_the_same_writes_nothing_and_tells_no_one(cx: &mut TestAppContext) {
    let bar = Bar::new();
    let fixture = Fixture::with_bar(cx, &bar);
    let before = fixture.with_window(cx, shown);
    repaint_thumbs(&fixture, cx).unwrap();
    assert_eq!(fixture.with_window(cx, shown), before);
    assert_eq!(bar.repainted.get(), 0);
    assert_eq!(fixture.with_window(cx, |window| window.patch_counters().thumb_repaints), 0);
}

#[gpui::test]
fn a_thumb_that_cannot_be_tracked_and_did_not_change_does_not_fail_the_plan(cx: &mut TestAppContext) {
    let bar = Bar::new();
    bar.rotated.set(true);
    let fixture = Fixture::with_bar(cx, &bar);
    let before = fixture.with_window(cx, shown);
    repaint_thumbs(&fixture, cx).unwrap();
    assert_eq!(fixture.with_window(cx, shown), before);
    assert_eq!(bar.repainted.get(), 0);

    bar.alpha.set(0.5);
    assert_eq!(repaint_thumbs(&fixture, cx), Err(ScrollThumbMiss::Track));
    assert_eq!(bar.repainted.get(), 0);
}

#[gpui::test]
fn a_bar_that_says_it_cannot_be_repainted_alone_writes_nothing(cx: &mut TestAppContext) {
    let bar = Bar::new();
    let fixture = Fixture::with_bar(cx, &bar);
    let before = fixture.with_window(cx, shown);
    bar.alpha.set(0.5);
    bar.length.set(30.);
    assert_eq!(repaint_thumbs(&fixture, cx), Err(ScrollThumbMiss::Appear));
    assert_eq!(fixture.with_window(cx, shown), before);
    assert_eq!(bar.repainted.get(), 0);

    // A thumb painted under a rotation is not one quad that can be written over.
    let turned = Bar::new();
    turned.rotated.set(true);
    let fixture = Fixture::with_bar(cx, &turned);
    turned.alpha.set(0.5);
    assert_eq!(repaint_thumbs(&fixture, cx), Err(ScrollThumbMiss::Track));
}
