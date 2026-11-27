//! The frames drawn after the wheel moves a `div` scroller (see [`super::scroll_patch::div`]): the ones that move
//! what the roots recorded and build nothing are the frames built whole, and each thing that cannot be moved says why.

use super::scroll_patch_tests::shown;
use crate::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Edges, InteractiveElement, IntoElement, MouseMoveEvent, ParentElement,
    PlatformInput, Render, ScrollDelta, ScrollHandle, ScrollPatchCause, ScrollPatchReason, ScrollThumbMiss, ScrollWheelEvent,
    Position, ScrollApplyDecline, StatefulInteractiveElement as _, Styled, TestAppContext, Window, div, fill, point, px, retained, size,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ROW: f32 = 30.;
const VIEW: f32 = 100.;
const ROWS: usize = 100;
const OVERSCAN: f32 = 100.;

/// A thumb for the scroller of `length` at `alpha`, that lies along it as far down as it is scrolled. The cells are what
/// the bar would be in the next frame, which a test changes to say the thumb cannot be put there.
#[derive(Clone)]
struct Bar {
    alpha: Rc<Cell<f32>>,
    length: Rc<Cell<f32>>,
    /// How many frames that moved the content told the bar.
    moved: Rc<Cell<u32>>,
}

impl Bar {
    fn new() -> Self {
        Self { alpha: Rc::new(Cell::new(1.)), length: Rc::new(Cell::new(20.)), moved: Rc::default() }
    }

    fn thumb(handle: &ScrollHandle, length: f32, horizontal: bool) -> Bounds<crate::Pixels> {
        let travel = VIEW - length;
        let offset = if horizontal { handle.offset().x } else { handle.offset().y };
        let along = f32::from(-offset) / (ROW * ROWS as f32 - VIEW) * travel;
        match horizontal {
            true => Bounds::new(point(px(along), px(92.)), size(px(length), px(6.))),
            false => Bounds::new(point(px(92.), px(along)), size(px(6.), px(length))),
        }
    }
}

/// What the scroller holds besides its roots that is painted in an order of its own.
#[derive(Clone, Copy, PartialEq)]
enum Layer {
    None,
    /// The scroller is a stacking context of its own.
    Context,
    /// A `position: sticky` child.
    Sticky,
    /// A positioned child with a `z-index`, which its stacking context paints.
    ZIndex,
}

#[derive(Clone)]
struct Page {
    handle: ScrollHandle,
    /// The overscan of the roots, if they are relocatable.
    overscan: Option<f32>,
    /// The roots are left out of the frame when they are further than the overscan from the clip.
    culls: bool,
    /// The roots are held by a box taller than the scroller, as TanStack's are.
    spacer: bool,
    /// The box that holds them has a hitbox, or paints.
    spacer_hitbox: bool,
    spacer_paints: bool,
    /// The roots have a hitbox.
    hitboxes: bool,
    /// Another scroller in the content.
    nested: bool,
    /// Each root is a stacking context of its own that an absolute box in it is handed to.
    row_context: bool,
    layer: Layer,
    /// The scroller scrolls along x, and the roots are columns (only with a box that holds them).
    horizontal: bool,
    /// Whether the owner of the scroller can be told that a frame moved its content.
    ready: Rc<Cell<bool>>,
    bar: Option<Bar>,
    key: Rc<Cell<u64>>,
    /// How far each frame that moved the content in place said it moved it.
    told: Rc<RefCell<Vec<crate::Point<crate::Pixels>>>>,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let page = self.clone();
        let rows: Vec<_> = (0..ROWS)
            .map(|ix| {
                let hitboxes = page.hitboxes;
                let row_context = page.row_context;
                let spacer = page.spacer;
                let horizontal = page.horizontal;
                let mut row = retained(("row", ix), page.key.get(), move |_, _| {
                    let row = div().id(("row", ix)).bg(crate::hsla(ix as f32 / 100., 0.5, 0.5, 1.));
                    let row = if horizontal { row.w(px(ROW)).h(px(80.)) } else { row.w(px(80.)).h(px(ROW)) };
                    let row = match (spacer, horizontal) {
                        (true, true) => row.absolute().left(px(ix as f32 * ROW)),
                        (true, false) => row.absolute().top(px(ix as f32 * ROW)),
                        (false, _) => row,
                    };
                    let row = if hitboxes { row.always_hitbox() } else { row };
                    let row = if row_context {
                        row.stacking_context().child(div().absolute().top(px(2.)).left(px(2.)).w(px(10.)).h(px(10.)).bg(crate::red()))
                    } else {
                        row
                    };
                    row.into_any_element()
                });
                if let Some(overscan) = page.overscan {
                    row = row.relocatable(px(overscan));
                }
                if page.culls {
                    row = row.cull_outset(Some(Edges::all(px(0.))));
                }
                if spacer {
                    row = row.lift_z(0);
                }
                row.into_any_element()
            })
            .collect();
        let scroller = div()
            .id("scroller")
            .w(px(100.))
            .h(px(VIEW))
            .track_scroll(&self.handle)
            .on_scroll_translated(
                {
                    let ready = self.ready.clone();
                    move || ready.get()
                },
                {
                    let told = self.told.clone();
                    move |_, _, translation| told.borrow_mut().push(translation.shift)
                },
            );
        let mut scroller = if self.horizontal { scroller.overflow_x_scroll() } else { scroller.overflow_y_scroll() };
        scroller.style().allow_concurrent_scroll = Some(true);
        let scroller = match self.bar.clone() {
            None => scroller,
            Some(bar) => {
                let (handle, horizontal) = (self.handle.clone(), self.horizontal);
                scroller.paint_overlay(move |_, window, _| {
                    let painted = (bar.alpha.get(), bar.length.get());
                    let thumb = (painted.0 > 0.).then(|| {
                        let mut color = crate::black();
                        color.a = painted.0;
                        fill(Bar::thumb(&handle, painted.1, horizontal), color)
                    });
                    let plan = Rc::new({
                        let (bar, handle) = (bar.clone(), handle.clone());
                        move |_: &Window, drawn: Option<Bounds<crate::Pixels>>| {
                            if painted.0 <= 0. {
                                return if bar.alpha.get() > 0. { Err(ScrollThumbMiss::Appear) } else { Ok(None) };
                            }
                            if bar.alpha.get() != painted.0 {
                                return Err(ScrollThumbMiss::Alpha);
                            }
                            if bar.length.get() != painted.1 {
                                return Err(ScrollThumbMiss::Size);
                            }
                            let next = Bar::thumb(&handle, painted.1, horizontal);
                            Ok((drawn != Some(next)).then_some(next))
                        }
                    });
                    let told = bar.moved.clone();
                    let moved = Rc::new(move |_: &mut Window, _: &mut crate::App| told.set(told.get() + 1));
                    let repaint = Rc::new(|_: &Window, _: Option<Bounds<crate::Pixels>>| Ok(None));
                    window.paint_scroll_thumb(handle.scroll_key(), thumb, plan, moved, repaint);
                })
            }
        };
        let scroller = if self.nested {
            let mut inner = div().id("inner").w(px(50.)).h(px(40.)).overflow_y_scroll().child(div().h(px(400.)));
            inner.style().allow_concurrent_scroll = Some(true);
            scroller.child(inner)
        } else {
            scroller
        };
        let scroller = match self.layer {
            Layer::None => scroller,
            Layer::Context => scroller.stacking_context(),
            Layer::Sticky => {
                let mut sticky = div().id("sticky").w(px(40.)).h(px(10.)).bg(crate::black());
                sticky.style().position = Some(Position::Sticky);
                sticky.style().inset.top = Some(px(0.).into());
                scroller.child(sticky)
            }
            Layer::ZIndex => scroller.child(div().id("raised").relative().z_index(1).w(px(10.)).h(px(10.)).bg(crate::red())),
        };
        let content = if self.spacer {
            let spacer = div().relative();
            let mut spacer = if self.horizontal { spacer.w(px(ROW * ROWS as f32)).h_full() } else { spacer.w_full().h(px(ROW * ROWS as f32)) };
            if self.spacer_hitbox {
                spacer = spacer.always_hitbox();
            }
            if self.spacer_paints {
                spacer = spacer.bg(crate::hsla(0.9, 0.2, 0.2, 1.));
            }
            scroller.child(spacer.children(rows))
        } else {
            scroller.children(rows)
        };
        // What the window paints before the scroller, so that the operations of its roots do not start where a recording's do.
        let before = (0..if self.row_context { 20 } else { 0 })
            .map(|ix| div().absolute().unlifted().left(px(300. + ix as f32)).top(px(0.)).w(px(1.)).h(px(1.)).bg(crate::blue()));
        // The window's own stacking context, which holds what is positioned.
        div().size_full().stacking_context().children(before).child(content).into_any_element()
    }
}

struct Fixture {
    window: AnyWindowHandle,
    page: Page,
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        Self::open(cx, Page::standard())
    }

    fn open(cx: &mut TestAppContext, page: Page) -> Self {
        let view = page.clone();
        let window = cx.open_window(size(px(400.), px(300.)), move |_, _| view);
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, _| {
            window.set_dom_hit_testing(true);
            window.set_scroll_patch(true);
            window.set_scroll_patch_gate(Some(Rc::new(|_, _, _| None)));
        })
        .unwrap();
        let fixture = Self { window, page };
        // The first frame builds and records the roots, the second puts them back.
        fixture.draw(cx);
        fixture.draw(cx);
        fixture
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    }

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

    /// A wheel event that moves the content up by `by`, and the frame the update that dispatches it draws, which is
    /// the one shown after it.
    fn scroll(&self, cx: &mut TestAppContext, x: f32, y: f32, by: f32) {
        self.point_at(cx, x, y);
        let delta = if self.page.horizontal { point(px(-by), px(0.)) } else { point(px(0.), px(-by)) };
        cx.update_window(self.window, |_, window, cx| {
            let event = ScrollWheelEvent { position: point(px(x), px(y)), delta: ScrollDelta::Pixels(delta), ..Default::default() };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
    }

    fn counters(&self, cx: &mut TestAppContext) -> crate::ScrollPatchCounters {
        cx.update_window(self.window, |_, window, _| window.scroll_patch_counters()).unwrap()
    }

    fn with_window<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Window) -> R) -> R {
        cx.update_window(self.window, |_, window, _| f(window)).unwrap()
    }
}

impl Page {
    /// Roots held by a box that is taller than the scroller, relocatable and culled when they are far.
    fn standard() -> Self {
        Self {
            handle: ScrollHandle::new(),
            overscan: Some(OVERSCAN),
            culls: true,
            spacer: true,
            spacer_hitbox: false,
            spacer_paints: false,
            hitboxes: true,
            nested: false,
            row_context: false,
            layer: Layer::None,
            horizontal: false,
            ready: Rc::new(Cell::new(true)),
            bar: None,
            key: Rc::new(Cell::new(1)),
            told: Rc::default(),
        }
    }
}

fn only(counters: &crate::ScrollPatchCounters, reason: ScrollPatchReason, causes: &[ScrollPatchCause]) {
    for other in ScrollPatchReason::ALL {
        let expected = u64::from(other == reason);
        assert_eq!(counters.reasons(other), expected, "{other:?}: {counters:?}");
    }
    assert_eq!((counters.frames, counters.eligible), (1, 0), "{counters:?}");
    let held: Vec<_> = ScrollPatchCause::ALL.into_iter().filter(|cause| counters.held_cause(*cause) == 1).collect();
    assert_eq!(held, causes, "{counters:?}");
}

#[gpui::test]
fn a_wheel_over_a_scroller_of_roots_is_a_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched, counters.mispredicted), (1, 1, 1, 0), "{counters:?}");
    for reason in ScrollPatchReason::ALL {
        assert_eq!(counters.reasons(reason), 0, "{reason:?}: {counters:?}");
    }
    assert_eq!(fixture.page.handle.offset().y, px(-5.));

    // The frame after is the new frame shown: another wheel is a translate from it.
    fixture.scroll(cx, 10., 10., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched, counters.mispredicted), (2, 2, 2, 0), "{counters:?}");
    assert_eq!(fixture.page.handle.offset().y, px(-10.));
}

/// The frame shown after each of `wheels` with the translate on or off.
fn after_wheels(cx: &mut TestAppContext, page: Page, patch: bool, wheels: &[f32]) -> (Vec<String>, crate::ScrollPatchCounters) {
    let fixture = Fixture::open(cx, page);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let frames = wheels
        .iter()
        .map(|by| {
            fixture.scroll(cx, 10., 12., *by);
            fixture.with_window(cx, shown)
        })
        .collect();
    (frames, fixture.counters(cx))
}

fn assert_the_same(translated: &[String], built: &[String], counters: &crate::ScrollPatchCounters, what: &str) {
    for (frame, (translated, built)) in translated.iter().zip(built).enumerate() {
        assert!(
            translated == built,
            "frame {frame}, {what}, {counters:?}\n{}",
            super::retained::first_difference(translated, built)
        );
    }
}

#[gpui::test]
fn a_translate_of_a_scroller_is_the_frame_built_whole(cx: &mut TestAppContext) {
    // Up and down by fractions of a row. The roots further than the overscan from the clip are culled, and one that
    // comes within it is built (at 15 the seventh row is 195 from the top): no translate for that wheel.
    let wheels = [5., 5., 7., -3., -4., 6., 2., -2., 1.];
    let variants: [(&str, fn(&mut Page)); 5] = [
        ("a box that holds the roots", |_| {}),
        ("a box with a hitbox", |page| page.spacer_hitbox = true),
        ("roots in the flow", |page| page.spacer = false),
        ("roots with no hitbox", |page| page.hitboxes = false),
        ("roots that are not culled", |page| page.culls = false),
    ];
    for (what, vary) in variants {
        let page = || {
            let mut page = Page::standard();
            vary(&mut page);
            page
        };
        let (translated, counters) = after_wheels(cx, page(), true, &wheels);
        let (built, off) = after_wheels(cx, page(), false, &wheels);
        assert_eq!(off.frames, 0, "{off:?}");
        assert!(counters.patched >= 4, "{what}: {counters:?}");
        assert_eq!(counters.mispredicted, 0, "{what}: {counters:?}");
        assert_the_same(&translated, &built, &counters, what);
    }
}

#[gpui::test]
fn a_culled_root_that_comes_into_reach_is_built(cx: &mut TestAppContext) {
    let flow = || {
        let mut page = Page::standard();
        page.spacer = false;
        page
    };
    let fixture = Fixture::open(cx, flow());
    // The seventh row is at 210 and is culled from 200 (the clip ends at 100, and the overscan is 100): at 195 it is not.
    fixture.scroll(cx, 10., 12., 5.);
    fixture.scroll(cx, 10., 12., 10.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched), (2, 1, 1), "{counters:?}");
    assert_eq!(counters.reasons(ScrollPatchReason::NotRelocatable), 1, "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::Entered), 1, "{counters:?}");
    // The frame built whole is what the control builds.
    let (translated, built) = (fixture.with_window(cx, shown), {
        let (frames, _) = after_wheels(cx, flow(), false, &[5., 10.]);
        frames[1].clone()
    });
    assert!(translated == built, "{}", super::retained::first_difference(&translated, &built));
}

#[gpui::test]
fn roots_that_are_not_relocatable_are_not_runs(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.overscan = None;
    page.culls = false;
    let fixture = Fixture::open(cx, page);
    fixture.scroll(cx, 10., 12., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.reasons(ScrollPatchReason::NotRelocatable)), (1, 0, 1), "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::NoRun), 1, "{counters:?}");
}

#[gpui::test]
fn what_the_scroller_paints_that_no_root_holds_is_unrooted(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.spacer_paints = true;
    let fixture = Fixture::open(cx, page);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::NotRelocatable, &[ScrollPatchCause::Unrooted]);
}

#[gpui::test]
fn what_a_root_built_in_the_frame_hands_to_a_context_of_its_own_is_the_roots(cx: &mut TestAppContext) {
    let page = || {
        let mut page = Page::standard();
        page.row_context = true;
        page
    };
    let fixture = Fixture::open(cx, page());
    fixture.point_at(cx, 10., 12.);
    // Every root is built in the frame shown: the boxes it hands to its own context are painted in its recording (a recording
    // buffer of its own, whose operations are numbered there and not in the frame), and are no operations that no root holds.
    fixture.page.key.set(2);
    fixture.with_window(cx, |window| window.refresh());
    fixture.draw(cx);
    fixture.scroll(cx, 10., 12., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched, counters.mispredicted), (1, 1, 1, 0), "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::Unrooted), 0, "{counters:?}");
    // And what it leaves is what the build gives.
    let translated = fixture.with_window(cx, shown);
    let built = {
        let fixture = Fixture::open(cx, page());
        fixture.point_at(cx, 10., 12.);
        fixture.with_window(cx, |window| window.set_scroll_patch(false));
        fixture.page.key.set(2);
        fixture.with_window(cx, |window| window.refresh());
        fixture.draw(cx);
        fixture.scroll(cx, 10., 12., 5.);
        fixture.with_window(cx, shown)
    };
    assert!(translated == built, "{}", super::retained::first_difference(&translated, &built));
}

#[gpui::test]
fn a_scroller_inside_the_scroller_is_nested(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.nested = true;
    let fixture = Fixture::open(cx, page);
    // At (80, 80), clear of the scroller that is inside.
    fixture.scroll(cx, 80., 80., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.reasons(ScrollPatchReason::NotRelocatable)), (1, 0, 1), "{counters:?}");
    assert_eq!(counters.held_cause(ScrollPatchCause::Nested), 1, "{counters:?}");
}

#[gpui::test]
fn a_pointer_that_moved_is_a_hover_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.point_at(cx, 10., 10.);
    cx.update_window(fixture.window, |_, window, cx| {
        let event = ScrollWheelEvent {
            position: point(px(10.), px(50.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-5.))),
            ..Default::default()
        };
        window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
    })
    .unwrap();
    let counters = fixture.counters(cx);
    only(&counters, ScrollPatchReason::Hover, &[]);
    assert_eq!(counters.hover_only, 1, "{counters:?}");
}

#[gpui::test]
fn a_commit_the_owner_reports_is_an_edit(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.with_window(cx, |window| window.set_scroll_patch_gate(Some(Rc::new(|_, _, _| Some(ScrollPatchReason::Edited)))));
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Edited, &[]);
}

#[gpui::test]
fn the_accessibility_tree_holds_the_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.with_window(cx, |window| window.set_a11y_active_for_tests(true));
    fixture.draw(cx);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::A11y, &[]);
}

#[gpui::test]
fn a_translate_takes_the_view_off_the_queue(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.scroll(cx, 10., 12., 5.);
    assert!(!fixture.with_window(cx, |window| window.invalidator.has_views()));
}

#[gpui::test]
fn a_translate_is_checked_against_the_frame_built_whole_after_it(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.spacer_hitbox = true;
    let fixture = Fixture::open(cx, page);
    let described = Rc::new(Cell::new(0));
    let verifier = described.clone();
    fixture.point_at(cx, 10., 12.);
    fixture.with_window(cx, move |window| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        window.set_scroll_patch_registries(Some(Rc::new(move |_| {
            verifier.set(verifier.get() + 1);
            String::new()
        })));
    });
    fixture.scroll(cx, 10., 12., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");
    fixture.with_window(cx, |window| {
        assert_eq!(window.retention_mismatches(), &[] as &[String]);
        assert!(window.retained_counters().verified >= 1);
    });
    assert_eq!(described.get(), 2, "the owner described the registries of the translate and of the build");
}

#[gpui::test]
fn the_owner_of_the_scroller_is_told_how_far_it_moved(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.scroll(cx, 10., 12., 5.);
    fixture.scroll(cx, 10., 12., 7.);
    assert_eq!(*fixture.page.told.borrow(), [point(px(0.), px(-5.)), point(px(0.), px(-7.))]);
}

#[gpui::test]
fn the_anchoring_of_the_scroller_notes_the_offset_a_translate_left(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    // A frame built whole at an offset picks an anchor.
    fixture.scroll(cx, 10., 12., 5.);
    fixture.with_window(cx, |window| window.set_scroll_patch(false));
    fixture.scroll(cx, 10., 12., 5.);
    fixture.draw(cx);
    let (anchored, left) = fixture.page.handle.anchoring_for_tests();
    assert!(anchored, "a scroller that is not at its start has an anchor");
    assert_eq!(left, point(px(0.), px(-10.)));
    // The next wheel is a translate: the anchor is dropped, as the prepaint it replaces drops it, and the offset is the one it left.
    fixture.with_window(cx, |window| window.set_scroll_patch(true));
    fixture.point_at(cx, 10., 12.);
    fixture.draw(cx);
    let patched = fixture.counters(cx).patched;
    fixture.scroll(cx, 10., 12., 5.);
    assert_eq!(fixture.counters(cx).patched, patched + 1, "{:?}", fixture.counters(cx));
    let (anchored, left) = fixture.page.handle.anchoring_for_tests();
    assert!(!anchored);
    assert_eq!(left, point(px(0.), px(-15.)));
    // A frame built whole after it picks one again.
    fixture.draw(cx);
    assert!(fixture.page.handle.anchoring_for_tests().0);
}

#[gpui::test]
fn a_scroller_that_moved_by_other_means_is_not_classified(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.page.handle.set_offset(point(px(0.), px(-5.)));
    fixture.with_window(cx, |window| window.refresh());
    fixture.draw(cx);
    assert_eq!(fixture.counters(cx).frames, 0);
}

fn after_wheels_with_bar(cx: &mut TestAppContext, patch: bool, wheels: &[f32]) -> (Vec<String>, crate::ScrollPatchCounters, u32) {
    let bar = Bar::new();
    let mut page = Page::standard();
    page.bar = Some(bar.clone());
    let (frames, counters) = after_wheels(cx, page, patch, wheels);
    (frames, counters, bar.moved.get())
}

#[gpui::test]
fn a_translate_moves_the_thumb_of_the_bar_and_is_the_frame_built_whole(cx: &mut TestAppContext) {
    let wheels = [5., 5., 7., -3., -4., 6., 2., -2., 1.];
    let (translated, counters, told) = after_wheels_with_bar(cx, true, &wheels);
    let (built, off, _) = after_wheels_with_bar(cx, false, &wheels);
    assert_eq!(off.frames, 0, "{off:?}");
    assert!(counters.patched >= 4, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    assert_eq!(u64::from(told), counters.patched, "the bar is told once for each frame that moved the content");
    for cause in [ScrollPatchCause::ThumbSize, ScrollPatchCause::ThumbAlpha, ScrollPatchCause::ThumbAppear, ScrollPatchCause::ThumbTrack] {
        assert_eq!(counters.held_cause(cause), 0, "{cause:?}: {counters:?}");
    }
    assert_the_same(&translated, &built, &counters, "a bar");
}

#[gpui::test]
fn a_bar_that_would_be_painted_otherwise_holds_the_translate(cx: &mut TestAppContext) {
    let bar = Bar::new();
    let mut page = Page::standard();
    page.bar = Some(bar.clone());
    let fixture = Fixture::open(cx, page);
    fixture.point_at(cx, 10., 12.);
    bar.alpha.set(0.5);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Thumb, &[ScrollPatchCause::ThumbAlpha]);
}

#[gpui::test]
fn a_thumb_that_the_scroll_moves_is_checked_against_the_frame_built_whole(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.bar = Some(Bar::new());
    page.spacer_hitbox = true;
    let fixture = Fixture::open(cx, page);
    fixture.point_at(cx, 10., 12.);
    fixture.with_window(cx, |window| window.set_retention_verifier(Some(Rc::new(|_| String::new()))));
    fixture.scroll(cx, 10., 12., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.eligible, counters.patched), (1, 1), "{counters:?}");
    fixture.with_window(cx, |window| {
        assert_eq!(window.retention_mismatches(), &[] as &[String]);
        assert!(window.retained_counters().verified >= 1);
    });
}

#[gpui::test]
fn a_translate_of_a_horizontal_scroller_is_the_frame_built_whole(cx: &mut TestAppContext) {
    let page = || {
        let mut page = Page::standard();
        page.horizontal = true;
        page.bar = Some(Bar::new());
        page
    };
    let wheels = [5., 5., 7., -3., -4., 6., 2., -2., 1.];
    let (translated, counters) = after_wheels(cx, page(), true, &wheels);
    let (built, off) = after_wheels(cx, page(), false, &wheels);
    assert_eq!(off.frames, 0, "{off:?}");
    assert!(counters.patched >= 4, "{counters:?}");
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    for cause in [ScrollPatchCause::ThumbSize, ScrollPatchCause::ThumbAlpha, ScrollPatchCause::ThumbAppear, ScrollPatchCause::ThumbTrack] {
        assert_eq!(counters.held_cause(cause), 0, "{cause:?}: {counters:?}");
    }
    assert_the_same(&translated, &built, &counters, "a horizontal scroller");
}

#[gpui::test]
fn a_horizontal_wheel_moves_the_offset_along_x_only(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.horizontal = true;
    let fixture = Fixture::open(cx, page);
    fixture.scroll(cx, 10., 12., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched), (1, 1, 1), "{counters:?}");
    assert_eq!(fixture.page.handle.offset(), point(px(-5.), px(0.)));
    assert_eq!(*fixture.page.told.borrow(), [point(px(-5.), px(0.))]);
}

#[gpui::test]
fn a_scroller_moved_past_its_range_along_x_is_not_a_translate(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.horizontal = true;
    let fixture = Fixture::open(cx, page);
    let max = fixture.page.handle.max_offset().x;
    assert!(max > px(0.), "the content is wider than the scroller");
    let view = cx.update_window(fixture.window, |view, _, _| view.entity_id()).unwrap();
    for (offset, eligible) in [(px(-5.), 1), (-max - px(50.), 0), (px(20.), 0)] {
        let before = fixture.counters(cx);
        fixture.page.handle.set_offset(point(offset, px(0.)));
        fixture.with_window(cx, |window| window.note_scroll_moved(view));
        fixture.draw(cx);
        let counters = fixture.counters(cx);
        assert_eq!(counters.frames, before.frames + 1, "{offset:?}: {counters:?}");
        assert_eq!(counters.eligible, before.eligible + eligible, "{offset:?}: {counters:?}");
        assert_eq!(counters.reasons(ScrollPatchReason::Other), before.reasons(ScrollPatchReason::Other) + (1 - eligible), "{offset:?}: {counters:?}");
        // The frame built whole is the one the next scroll is judged from.
        fixture.draw(cx);
    }
}

#[gpui::test]
fn what_holds_the_content_in_an_order_of_its_own_is_layered(cx: &mut TestAppContext) {
    for layer in [Layer::Context, Layer::Sticky] {
        let mut page = Page::standard();
        page.layer = layer;
        let fixture = Fixture::open(cx, page);
        fixture.scroll(cx, 10., 12., 5.);
        let counters = fixture.counters(cx);
        assert_eq!((counters.frames, counters.eligible), (1, 0), "{counters:?}");
        assert_eq!(counters.held_cause(ScrollPatchCause::Layered), 1, "a stacking context: {counters:?}");
    }
}

#[gpui::test]
fn a_child_with_a_z_index_is_what_no_root_holds(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.layer = Layer::ZIndex;
    let fixture = Fixture::open(cx, page);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::NotRelocatable, &[ScrollPatchCause::Unrooted]);
}

#[gpui::test]
fn the_owner_is_given_the_scroller_the_translate_moves(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    let (gated, settled) = (Rc::new(RefCell::new(Vec::new())), Rc::new(RefCell::new(Vec::new())));
    fixture.with_window(cx, |window| {
        let log = gated.clone();
        window.set_scroll_patch_gate(Some(Rc::new(move |_, _, translating| {
            log.borrow_mut().push(translating);
            None
        })));
        let log = settled.clone();
        window.set_scroll_patch_settled(Some(Rc::new(move |_, _, moved| log.borrow_mut().push(moved))));
    });
    fixture.scroll(cx, 10., 12., 5.);
    let key = fixture.page.handle.scroll_key();
    assert_eq!(gated.borrow().last(), Some(&Some(key)));
    assert_eq!(*settled.borrow(), [key]);
}

#[gpui::test]
fn an_owner_that_cannot_be_told_declines_the_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.page.ready.set(false);
    fixture.scroll(cx, 10., 12., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched), (1, 1, 0), "{counters:?}");
    assert_eq!(counters.apply_declines(ScrollApplyDecline::Window), 1, "{counters:?}");
    assert!(fixture.page.told.borrow().is_empty());
    // The frame is built whole: the scroller is where the wheel put it.
    assert_eq!(fixture.page.handle.offset().y, px(-5.));
    fixture.page.ready.set(true);
    fixture.scroll(cx, 10., 12., 5.);
    assert_eq!(fixture.counters(cx).patched, 1, "{:?}", fixture.counters(cx));
}
